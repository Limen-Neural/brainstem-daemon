// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Fake-clock / fake-core runtime that can inject faults at lifecycle boundaries.

use anyhow::{Result, bail};

use super::clock::{BACKPRESSURE_BUDGET_NS, FakeClock, TICK_PERIOD_NS};
use super::durable::{DurableState, DurableStore, FAKE_CHECKPOINT_ID, InflightRecord};
use super::fakes::{CommittedOutput, FakeCore, MetricEvent, ScriptedSource, TickResult};
use super::inject::{FaultInjector, injected_terminal_health};
use super::{Boundary, FaultPoint, Health, RestartOutcome, SequencedIngress, packets_for_seed};

/// Hard cap on ticks per harness instance. Replaces wall-clock timeouts.
pub const MAX_STEPS: u32 = 64;

/// Synchronous runtime session used by the fault-injection suite.
pub struct RuntimeHarness {
    clock: FakeClock,
    core: FakeCore,
    store: DurableStore,
    durable: DurableState,
    injector: FaultInjector,
    source: ScriptedSource,
    published: Vec<CommittedOutput>,
    metrics: Vec<MetricEvent>,
    health: Health,
    session_id: u64,
    live: bool,
    live_loop_entered: bool,
    incomplete_prior: bool,
    restart_outcome: Option<RestartOutcome>,
    skipped_replays: u64,
    steps_taken: u32,
    channel_open: bool,
}

impl RuntimeHarness {
    pub fn builder(seed: u64) -> HarnessBuilder {
        HarnessBuilder::new(seed)
    }

    pub fn health(&self) -> Health {
        self.health
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn live_loop_entered(&self) -> bool {
        self.live_loop_entered
    }

    pub fn incomplete_prior(&self) -> bool {
        self.incomplete_prior
    }

    pub fn restart_outcome(&self) -> Option<RestartOutcome> {
        self.restart_outcome
    }

    pub fn durable(&self) -> &DurableState {
        &self.durable
    }

    pub fn published(&self) -> &[CommittedOutput] {
        &self.published
    }

    pub fn metrics(&self) -> &[MetricEvent] {
        &self.metrics
    }

    pub fn skipped_replays(&self) -> u64 {
        self.skipped_replays
    }

    pub fn steps_taken(&self) -> u32 {
        self.steps_taken
    }

    pub fn core(&self) -> &FakeCore {
        &self.core
    }

    pub fn clock(&self) -> &FakeClock {
        &self.clock
    }

    pub fn channel_open(&self) -> bool {
        self.channel_open
    }

    pub fn into_store(self) -> DurableStore {
        self.store
    }

    pub fn arm_fault(&mut self, point: FaultPoint) {
        self.injector.set(point);
    }

    pub fn push_ingress(&mut self, packet: SequencedIngress) {
        self.source.push(packet);
    }

    /// Insert a packet at the head of the ingress queue (replay / retry).
    pub fn push_ingress_front(&mut self, packet: SequencedIngress) {
        self.source.push_front(packet);
    }

    /// Drive initialization and checkpoint validation. The live tick loop
    /// starts only after this returns `Ok` with `health == Live`.
    pub fn boot(&mut self) -> Result<()> {
        self.health = Health::Starting;
        self.initialize()?;
        self.validate_checkpoint()?;
        self.fail_point(FaultPoint::After(Boundary::CheckpointValidation))?;
        self.health = Health::Live;
        self.live = true;
        self.live_loop_entered = true;
        debug_assert_eq!(self.durable.checkpoint_id, FAKE_CHECKPOINT_ID);
        Ok(())
    }

    /// One live tick. Refuses to run if boot did not enter `Live`.
    pub fn run_tick(&mut self) -> Result<TickResult> {
        self.begin_tick()?;
        let packet = match self.ingress()? {
            Some(packet) => packet,
            None => return Ok(TickResult::NoIngress),
        };
        if packet.seq <= self.durable.committed_ingress_seq {
            // High-water mark: sources must emit strictly increasing sequences.
            // Restart reconstructs the source; upstream is at-least-once.
            self.skipped_replays += 1;
            return Ok(TickResult::SkippedReplay);
        }
        let (tick_seq, spike_ids) = self.execute_tick(&packet)?;
        self.publish_metrics(tick_seq, packet.seq, spike_ids)?;
        Ok(TickResult::Committed)
    }

    fn initialize(&mut self) -> Result<()> {
        self.fail_point(FaultPoint::Before(Boundary::Initialize))?;
        self.core = FakeCore::default();
        self.channel_open = true;
        self.published.clear();
        self.metrics.clear();
        self.fail_point(FaultPoint::After(Boundary::Initialize))
    }

    fn validate_checkpoint(&mut self) -> Result<()> {
        self.fail_point(FaultPoint::Before(Boundary::CheckpointValidation))?;
        if self.injector.take(FaultPoint::MalformedCheckpoint) {
            self.store = DurableStore::malformed(b"{injected-malformed-checkpoint");
            self.reject_checkpoint();
            bail!("malformed checkpoint");
        }
        match self.store.load() {
            Ok(None) => self.start_fresh_session()?,
            Ok(Some(state)) => {
                if state.committed_tick_seq == u64::MAX {
                    self.reject_checkpoint();
                    bail!("tick sequence overflow");
                }
                self.recover_session(state)?;
            }
            Err(err) => {
                self.reject_checkpoint();
                return Err(err);
            }
        }
        Ok(())
    }

    fn start_fresh_session(&mut self) -> Result<()> {
        self.durable = DurableState::fresh();
        self.session_id = 1;
        self.durable.last_session_id = 1;
        self.store.persist(&self.durable)?;
        self.incomplete_prior = false;
        self.restart_outcome = Some(RestartOutcome::FreshStart);
        Ok(())
    }

    fn recover_session(&mut self, mut state: DurableState) -> Result<()> {
        let inflight = state.inflight.take();
        self.incomplete_prior = inflight.is_some();
        let Some(session_id) = state.last_session_id.checked_add(1) else {
            self.reject_checkpoint();
            bail!("session id overflow");
        };
        self.session_id = session_id;
        state.last_session_id = session_id;
        self.store.persist(&state)?;
        self.durable = state;
        self.restart_outcome = Some(if self.incomplete_prior {
            RestartOutcome::RecoveredIncomplete
        } else {
            RestartOutcome::RecoveredClean
        });
        Ok(())
    }

    fn reject_checkpoint(&mut self) {
        self.health = Health::CheckpointInvalid;
        self.restart_outcome = Some(RestartOutcome::RejectedInvalid);
        self.live = false;
    }

    fn begin_tick(&mut self) -> Result<()> {
        if !self.live {
            bail!("tick loop has not started");
        }
        self.steps_taken = self.steps_taken.saturating_add(1);
        if self.steps_taken > MAX_STEPS {
            bail!("bounded step limit {MAX_STEPS} exceeded");
        }
        self.clock.advance(TICK_PERIOD_NS);
        Ok(())
    }

    fn execute_tick(&mut self, packet: &SequencedIngress) -> Result<(u64, Vec<u16>)> {
        self.fail_point(FaultPoint::Before(Boundary::TickExecute))?;
        let Some(tick_seq) = self.durable.committed_tick_seq.checked_add(1) else {
            self.health = Health::Faulted;
            self.live = false;
            bail!("tick sequence overflow");
        };
        let mut next = self.durable.clone();
        next.inflight = Some(InflightRecord {
            session_id: self.session_id,
            tick_seq,
            ingress_seq: packet.seq,
        });
        self.store.persist(&next)?;
        self.durable = next;
        if self.injector.take(FaultPoint::CoreStepError) {
            self.apply_fault(FaultPoint::CoreStepError);
            bail!("core-step error");
        }
        let spike_ids = self.core.step(&packet.stimuli)?;
        self.fail_point(FaultPoint::After(Boundary::TickExecute))?;
        Ok((tick_seq, spike_ids))
    }

    fn publish_metrics(
        &mut self,
        tick_seq: u64,
        ingress_seq: u64,
        spike_ids: Vec<u16>,
    ) -> Result<()> {
        self.fail_point(FaultPoint::Before(Boundary::MetricPublish))?;
        if self.injector.take(FaultPoint::BackpressureTimeout) {
            self.clock.advance(BACKPRESSURE_BUDGET_NS);
            self.apply_fault(FaultPoint::BackpressureTimeout);
            bail!("backpressure timeout");
        }
        if !self.channel_open {
            self.health = Health::Degraded;
            bail!("closed publish channel");
        }
        let output = CommittedOutput {
            session_id: self.session_id,
            tick_seq,
            ingress_seq,
            spike_ids,
            time_ns: self.clock.now_ns(),
        };
        let mut next = self.durable.clone();
        next.committed_tick_seq = tick_seq;
        next.committed_ingress_seq = ingress_seq;
        next.inflight = None;
        self.store.persist(&next)?;
        self.durable = next;
        self.metrics.push(MetricEvent {
            session_id: output.session_id,
            tick_seq: output.tick_seq,
            ingress_seq: output.ingress_seq,
        });
        self.published.push(output);
        self.fail_point(FaultPoint::After(Boundary::MetricPublish))
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.health = Health::ShuttingDown;
        self.fail_point(FaultPoint::Before(Boundary::Shutdown))?;
        if self.injector.take(FaultPoint::InterruptedShutdown) {
            self.apply_fault(FaultPoint::InterruptedShutdown);
            bail!("interrupted shutdown");
        }
        self.channel_open = false;
        self.live = false;
        self.fail_point(FaultPoint::After(Boundary::Shutdown))?;
        self.health = Health::Stopped;
        Ok(())
    }

    fn ingress(&mut self) -> Result<Option<SequencedIngress>> {
        self.fail_point(FaultPoint::Before(Boundary::Ingress))?;
        if self.injector.take(FaultPoint::ClosedChannel) || !self.channel_open {
            self.apply_fault(FaultPoint::ClosedChannel);
            bail!("closed channel");
        }
        let packet = self.source.next();
        self.fail_point(FaultPoint::After(Boundary::Ingress))?;
        Ok(packet)
    }

    fn fail_point(&mut self, point: FaultPoint) -> Result<()> {
        if let Err(err) = self.injector.fire(point) {
            self.apply_fault(point);
            return Err(err);
        }
        Ok(())
    }

    fn apply_fault(&mut self, point: FaultPoint) {
        self.health = injected_terminal_health(point);
        if matches!(
            self.health,
            Health::Faulted
                | Health::Degraded
                | Health::IncompleteShutdown
                | Health::CheckpointInvalid
        ) {
            self.live = false;
        }
    }
}

/// Builder for a deterministic harness instance.
pub struct HarnessBuilder {
    seed: u64,
    store: DurableStore,
    injector: FaultInjector,
    packets: Vec<SequencedIngress>,
}

impl HarnessBuilder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            store: DurableStore::empty(),
            injector: FaultInjector::none(),
            packets: packets_for_seed(seed),
        }
    }

    pub fn store(mut self, store: DurableStore) -> Self {
        self.store = store;
        self
    }

    pub fn fault(mut self, point: FaultPoint) -> Self {
        self.injector = FaultInjector::arm(point);
        self
    }

    pub fn packets(mut self, packets: Vec<SequencedIngress>) -> Self {
        self.packets = packets;
        self
    }

    pub fn build(self) -> RuntimeHarness {
        RuntimeHarness {
            clock: FakeClock::new(self.seed.wrapping_mul(1_000_000).saturating_add(1)),
            core: FakeCore::default(),
            store: self.store,
            durable: DurableState::fresh(),
            injector: self.injector,
            source: ScriptedSource::new(self.packets),
            published: Vec::new(),
            metrics: Vec::new(),
            health: Health::Starting,
            session_id: 0,
            live: false,
            live_loop_entered: false,
            incomplete_prior: false,
            restart_outcome: None,
            skipped_replays: 0,
            steps_taken: 0,
            channel_open: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{DurableState, RestartOutcome};

    #[test]
    fn exhausted_session_id_fails_before_live_loop() {
        let mut state = DurableState::fresh();
        state.last_session_id = u64::MAX;
        let store = DurableStore::from_state(&state).unwrap();
        let mut h = RuntimeHarness::builder(0).store(store).build();
        let err = h.boot().unwrap_err().to_string();
        assert!(
            err.contains("session id overflow"),
            "unexpected error: {err}"
        );
        assert_eq!(h.health(), crate::runtime::Health::CheckpointInvalid);
        assert!(!h.live_loop_entered());
        assert_eq!(h.restart_outcome(), Some(RestartOutcome::RejectedInvalid));
    }

    #[test]
    fn exhausted_tick_seq_fails_before_live_loop() {
        let mut state = DurableState::fresh();
        state.last_session_id = 1;
        state.committed_tick_seq = u64::MAX;
        let store = DurableStore::from_state(&state).unwrap();
        let mut h = RuntimeHarness::builder(0).store(store).build();
        let err = h.boot().unwrap_err().to_string();
        assert!(
            err.contains("tick sequence overflow"),
            "unexpected error: {err}"
        );
        assert_eq!(h.health(), crate::runtime::Health::CheckpointInvalid);
        assert!(!h.is_live());
        assert!(!h.live_loop_entered());
        assert_eq!(h.restart_outcome(), Some(RestartOutcome::RejectedInvalid));
        assert!(h.run_tick().is_err());
    }

    #[test]
    fn armed_malformed_checkpoint_poisons_empty_store_for_restart() {
        let mut first = RuntimeHarness::builder(0)
            .fault(crate::runtime::FaultPoint::MalformedCheckpoint)
            .build();
        assert!(first.boot().is_err());
        assert_eq!(first.health(), crate::runtime::Health::CheckpointInvalid);
        let store = first.into_store();
        let mut restarted = RuntimeHarness::builder(0).store(store).build();
        assert!(restarted.boot().is_err());
        assert_eq!(
            restarted.restart_outcome(),
            Some(RestartOutcome::RejectedInvalid)
        );
        assert!(!restarted.live_loop_entered());
    }
}
