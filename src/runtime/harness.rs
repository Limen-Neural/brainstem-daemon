// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Fake-clock / fake-core runtime that can inject faults at lifecycle boundaries.

use std::collections::VecDeque;

use anyhow::{Result, bail};

use super::durable::{DurableState, DurableStore, FAKE_CHECKPOINT_ID, InflightRecord};
use super::{Boundary, FaultPoint, Health, RestartOutcome, SequencedIngress};

/// Hard cap on ticks per harness instance. Replaces wall-clock timeouts.
pub const MAX_STEPS: u32 = 64;

/// Fake-clock nanoseconds advanced on a backpressure timeout (10 ms).
pub const BACKPRESSURE_BUDGET_NS: u64 = 10_000_000;

/// One nanosecond-scale tick period at 1 kHz, applied without sleeping.
pub const TICK_PERIOD_NS: u64 = 1_000_000;

/// Deterministic clock. `advance` never sleeps or yields.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now_ns: u64,
}

impl FakeClock {
    pub fn new(now_ns: u64) -> Self {
        Self { now_ns }
    }

    pub fn now_ns(&self) -> u64 {
        self.now_ns
    }

    pub fn advance(&mut self, ns: u64) {
        self.now_ns = self.now_ns.saturating_add(ns);
    }
}

/// CPU-only stand-in for `neuromod::SpikingNetwork`.
///
/// Membrane / last-stimuli fields are volatile: a restarted harness gets a
/// fresh core even when the durable store is reused.
#[derive(Debug, Clone, Default)]
pub struct FakeCore {
    pub steps: u64,
    last_stimuli: Option<Vec<f32>>,
}

impl FakeCore {
    pub fn last_stimuli(&self) -> Option<&[f32]> {
        self.last_stimuli.as_deref()
    }

    fn step(&mut self, stimuli: &[f32]) -> Result<Vec<u16>> {
        self.steps += 1;
        self.last_stimuli = Some(stimuli.to_vec());
        Ok(stimuli
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.5)
            .filter_map(|(i, _)| u16::try_from(i).ok())
            .collect())
    }
}

/// Scripted ingress. No sockets.
#[derive(Debug, Clone, Default)]
pub struct ScriptedSource {
    packets: VecDeque<SequencedIngress>,
}

impl ScriptedSource {
    pub fn new(packets: impl IntoIterator<Item = SequencedIngress>) -> Self {
        Self {
            packets: packets.into_iter().collect(),
        }
    }

    pub fn push(&mut self, packet: SequencedIngress) {
        self.packets.push_back(packet);
    }

    fn push_front(&mut self, packet: SequencedIngress) {
        self.packets.push_front(packet);
    }

    fn next(&mut self) -> Option<SequencedIngress> {
        self.packets.pop_front()
    }
}

/// One successfully published tick. Appended only after metric publication.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedOutput {
    pub session_id: u64,
    pub tick_seq: u64,
    pub ingress_seq: u64,
    pub spike_ids: Vec<u16>,
    pub time_ns: u64,
}

/// Metric publication that is allowed only for committed ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricEvent {
    pub session_id: u64,
    pub tick_seq: u64,
    pub ingress_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickResult {
    Committed,
    SkippedReplay,
    NoIngress,
}

#[derive(Debug, Default)]
struct FaultInjector {
    armed: Option<FaultPoint>,
}

impl FaultInjector {
    fn none() -> Self {
        Self { armed: None }
    }

    fn arm(point: FaultPoint) -> Self {
        Self { armed: Some(point) }
    }

    fn set(&mut self, point: FaultPoint) {
        self.armed = Some(point);
    }

    fn fire(&mut self, point: FaultPoint) -> Result<()> {
        if self.armed == Some(point) {
            self.armed = None;
            bail!(injected_message(point));
        }
        Ok(())
    }

    fn take(&mut self, point: FaultPoint) -> bool {
        if self.armed == Some(point) {
            self.armed = None;
            true
        } else {
            false
        }
    }
}

fn injected_message(point: FaultPoint) -> String {
    format!("injected fault: {point:?}")
}

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
            Ok(None) => self.start_fresh_session(),
            Ok(Some(state)) => self.recover_session(state),
            Err(err) => {
                self.reject_checkpoint();
                Err(err)
            }
        }
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
        let tick_seq = self
            .durable
            .committed_tick_seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("tick sequence overflow"))?;
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

/// Terminal health assigned by the harness. Kept independent of
/// [`super::expected_outcome`] so the matrix test can detect drift.
fn injected_terminal_health(point: FaultPoint) -> Health {
    match point {
        FaultPoint::Before(Boundary::Initialize)
        | FaultPoint::After(Boundary::Initialize)
        | FaultPoint::Before(Boundary::CheckpointValidation)
        | FaultPoint::After(Boundary::CheckpointValidation)
        | FaultPoint::Before(Boundary::TickExecute)
        | FaultPoint::After(Boundary::TickExecute)
        | FaultPoint::After(Boundary::MetricPublish)
        | FaultPoint::After(Boundary::Shutdown)
        | FaultPoint::CoreStepError => Health::Faulted,
        FaultPoint::MalformedCheckpoint => Health::CheckpointInvalid,
        FaultPoint::Before(Boundary::Ingress)
        | FaultPoint::After(Boundary::Ingress)
        | FaultPoint::ClosedChannel
        | FaultPoint::Before(Boundary::MetricPublish)
        | FaultPoint::BackpressureTimeout => Health::Degraded,
        FaultPoint::Before(Boundary::Shutdown) | FaultPoint::InterruptedShutdown => {
            Health::IncompleteShutdown
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

/// Scripted packets for a seed. Stimuli (and therefore fake-core spikes)
/// vary, but sequence numbers stay `1..=8`.
pub fn packets_for_seed(seed: u64) -> Vec<SequencedIngress> {
    (1..=8)
        .map(|seq| SequencedIngress {
            seq,
            stimuli: stimuli_for(seed, seq),
        })
        .collect()
}

fn stimuli_for(seed: u64, seq: u64) -> Vec<f32> {
    let v = ((seed.wrapping_add(seq.wrapping_mul(17))) % 10) as f32 / 10.0;
    vec![v, 1.0 - v]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{DurableState, RestartOutcome};

    #[test]
    fn fake_core_drops_out_of_range_spike_ids() {
        let mut core = FakeCore::default();
        let mut stimuli = vec![0.0; (u16::MAX as usize) + 2];
        stimuli[0] = 0.9;
        stimuli[u16::MAX as usize + 1] = 0.9;
        let spikes = core.step(&stimuli).unwrap();
        assert_eq!(spikes, vec![0]);
    }

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
