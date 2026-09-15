// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Bounded ingress queues that feed the tick loop.
//!
//! Inventory of channels that can feed a tick:
//!
//! | Class | Source today | Default overflow |
//! |---|---|---|
//! | Sensory | `StimulusSource::next_ingress` stimuli | drop-oldest |
//! | Reward | `IngressPacket.modulators` | coalesce |
//! | Control | in-band control envelopes; OS SIGINT/SIGTERM stay out-of-band | block-timeout |
//! | Telemetry | reserved bulk class (no in-crate producer yet) | drop-oldest |
//!
//! Each class has its own capacity and policy so bulk telemetry cannot share a
//! buffer with control/safety traffic. Overflow never uses one implicit policy.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::backend::IngressPacket;

/// Low-cardinality label set for ingress metrics (four values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageClass {
    Sensory,
    Reward,
    Control,
    Telemetry,
}

impl MessageClass {
    /// All classes in drain-priority order: control before bulk.
    pub const ALL: [Self; 4] = [Self::Control, Self::Reward, Self::Sensory, Self::Telemetry];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sensory => "sensory",
            Self::Reward => "reward",
            Self::Control => "control",
            Self::Telemetry => "telemetry",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Sensory => 0,
            Self::Reward => 1,
            Self::Control => 2,
            Self::Telemetry => 3,
        }
    }
}

/// What happens when a class queue is at capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Wait up to `block_timeout_ms` for a slot, then reject.
    BlockTimeout,
    /// Refuse the new event immediately.
    Reject,
    /// Evict the oldest queued event and accept the new one.
    DropOldest,
    /// Keep a single latest snapshot (depth 0 or 1).
    Coalesce,
}

impl OverflowPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlockTimeout => "block_timeout",
            Self::Reject => "reject",
            Self::DropOldest => "drop_oldest",
            Self::Coalesce => "coalesce",
        }
    }
}

/// Result surfaced to the producer. `Rejected` and `Shutdown` mean the new
/// event was not queued; the other variants mean it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum EnqueueOutcome {
    Accepted {
        depth: usize,
    },
    /// New event accepted; one older event was dropped.
    DroppedOldest {
        depth: usize,
    },
    /// New event accepted; it replaced the previous occupant.
    Coalesced {
        depth: usize,
    },
    /// New event not queued (full + reject, or block wait timed out).
    Rejected,
    /// Queue closed; event not queued. Waiting producers are unblocked.
    Shutdown,
}

impl EnqueueOutcome {
    /// True when the caller's event is in the queue.
    pub fn accepted(self) -> bool {
        matches!(
            self,
            Self::Accepted { .. } | Self::DroppedOldest { .. } | Self::Coalesced { .. }
        )
    }
}

fn default_sensory_capacity() -> usize {
    64
}
fn default_reward_capacity() -> usize {
    8
}
fn default_control_capacity() -> usize {
    16
}
fn default_telemetry_capacity() -> usize {
    128
}
fn default_sensory_policy() -> OverflowPolicy {
    OverflowPolicy::DropOldest
}
fn default_reward_policy() -> OverflowPolicy {
    OverflowPolicy::Coalesce
}
fn default_control_policy() -> OverflowPolicy {
    OverflowPolicy::BlockTimeout
}
fn default_telemetry_policy() -> OverflowPolicy {
    OverflowPolicy::DropOldest
}
fn default_block_timeout_ms() -> u64 {
    5
}

/// Per-class capacities and overflow policies. Missing TOML keys use defaults.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IngressConfig {
    #[serde(default = "default_sensory_capacity")]
    pub sensory_capacity: usize,
    #[serde(default = "default_sensory_policy")]
    pub sensory_policy: OverflowPolicy,
    #[serde(default = "default_reward_capacity")]
    pub reward_capacity: usize,
    #[serde(default = "default_reward_policy")]
    pub reward_policy: OverflowPolicy,
    #[serde(default = "default_control_capacity")]
    pub control_capacity: usize,
    #[serde(default = "default_control_policy")]
    pub control_policy: OverflowPolicy,
    #[serde(default = "default_telemetry_capacity")]
    pub telemetry_capacity: usize,
    #[serde(default = "default_telemetry_policy")]
    pub telemetry_policy: OverflowPolicy,
    /// Used only by `block_timeout` queues.
    #[serde(default = "default_block_timeout_ms")]
    pub block_timeout_ms: u64,
}

impl Default for IngressConfig {
    fn default() -> Self {
        Self {
            sensory_capacity: default_sensory_capacity(),
            sensory_policy: default_sensory_policy(),
            reward_capacity: default_reward_capacity(),
            reward_policy: default_reward_policy(),
            control_capacity: default_control_capacity(),
            control_policy: default_control_policy(),
            telemetry_capacity: default_telemetry_capacity(),
            telemetry_policy: default_telemetry_policy(),
            block_timeout_ms: default_block_timeout_ms(),
        }
    }
}

impl IngressConfig {
    /// Tiny capacities for stalled-consumer / overload tests.
    pub fn tiny_fixture() -> Self {
        Self {
            sensory_capacity: 2,
            sensory_policy: OverflowPolicy::DropOldest,
            reward_capacity: 1,
            reward_policy: OverflowPolicy::Coalesce,
            control_capacity: 4,
            control_policy: OverflowPolicy::Reject,
            telemetry_capacity: 2,
            telemetry_policy: OverflowPolicy::DropOldest,
            block_timeout_ms: 0,
        }
    }

    pub fn capacity(&self, class: MessageClass) -> usize {
        match class {
            MessageClass::Sensory => self.sensory_capacity,
            MessageClass::Reward => self.reward_capacity,
            MessageClass::Control => self.control_capacity,
            MessageClass::Telemetry => self.telemetry_capacity,
        }
    }

    pub fn policy(&self, class: MessageClass) -> OverflowPolicy {
        match class {
            MessageClass::Sensory => self.sensory_policy,
            MessageClass::Reward => self.reward_policy,
            MessageClass::Control => self.control_policy,
            MessageClass::Telemetry => self.telemetry_policy,
        }
    }

    pub fn block_timeout(&self) -> Duration {
        Duration::from_millis(self.block_timeout_ms)
    }

    /// Capacities must be ≥ 1 so a class cannot be configured into a silent
    /// black hole.
    pub fn validate(&self) -> anyhow::Result<()> {
        for class in [
            MessageClass::Sensory,
            MessageClass::Reward,
            MessageClass::Control,
            MessageClass::Telemetry,
        ] {
            if self.capacity(class) == 0 {
                anyhow::bail!("ingress {} capacity must be >= 1", class.as_str());
            }
        }
        Ok(())
    }
}

/// Snapshot of one class. Labels are the class name only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassMetrics {
    pub accepted: u64,
    pub rejected: u64,
    pub dropped: u64,
    pub coalesced: u64,
    pub depth: usize,
    pub high_water_mark: usize,
    pub producer_waits: u64,
    pub producer_wait_ns: u64,
}

impl ClassMetrics {
    /// Events that were not delivered and are not sitting in the queue.
    pub fn lost_or_coalesced(self) -> u64 {
        self.rejected + self.dropped + self.coalesced
    }
}

/// Four-class metrics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressMetrics {
    pub sensory: ClassMetrics,
    pub reward: ClassMetrics,
    pub control: ClassMetrics,
    pub telemetry: ClassMetrics,
}

impl IngressMetrics {
    pub fn class(&self, class: MessageClass) -> ClassMetrics {
        match class {
            MessageClass::Sensory => self.sensory,
            MessageClass::Reward => self.reward,
            MessageClass::Control => self.control,
            MessageClass::Telemetry => self.telemetry,
        }
    }
}

/// Packets drained for one tick, control first so it cannot sit behind bulk.
#[derive(Debug, Clone, Default)]
pub struct DrainedTick {
    pub control: Vec<IngressPacket>,
    pub reward: Option<IngressPacket>,
    pub sensory: Option<IngressPacket>,
    pub telemetry_drained: usize,
}

impl DrainedTick {
    /// Merge sensory + reward into the packet the network step already consumes.
    pub fn into_packet(self) -> IngressPacket {
        IngressPacket {
            stimuli: self.sensory.map(|p| p.stimuli).unwrap_or_default(),
            modulators: self.reward.and_then(|p| p.modulators),
        }
    }
}

enum WaitMode {
    HonorPolicy,
    Never,
}

struct QueueState {
    items: VecDeque<IngressPacket>,
    closed: bool,
    accepted: u64,
    rejected: u64,
    dropped: u64,
    coalesced: u64,
    high_water_mark: usize,
    producer_waits: u64,
    producer_wait_ns: u64,
}

struct BoundedQueue {
    capacity: usize,
    policy: OverflowPolicy,
    block_timeout: Duration,
    class: MessageClass,
    inner: Mutex<QueueState>,
    not_full: Condvar,
    not_empty: Condvar,
}

impl BoundedQueue {
    fn new(
        class: MessageClass,
        capacity: usize,
        policy: OverflowPolicy,
        block_timeout: Duration,
    ) -> Self {
        // Coalesce is a latest-snapshot: at most one occupant.
        let capacity = match policy {
            OverflowPolicy::Coalesce => 1,
            _ => capacity,
        };
        Self {
            capacity,
            policy,
            block_timeout,
            class,
            inner: Mutex::new(QueueState {
                items: VecDeque::with_capacity(capacity),
                closed: false,
                accepted: 0,
                rejected: 0,
                dropped: 0,
                coalesced: 0,
                high_water_mark: 0,
                producer_waits: 0,
                producer_wait_ns: 0,
            }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, packet: IngressPacket, wait: WaitMode) -> EnqueueOutcome {
        let mut guard = self.lock();
        loop {
            if guard.closed {
                return EnqueueOutcome::Shutdown;
            }

            if self.policy == OverflowPolicy::Coalesce {
                return self.push_coalesce(&mut guard, packet);
            }

            if guard.items.len() < self.capacity {
                return self.push_free(&mut guard, packet);
            }

            match self.policy {
                OverflowPolicy::Reject => {
                    guard.rejected += 1;
                    log_overflow(self.class, "rejected");
                    return EnqueueOutcome::Rejected;
                }
                OverflowPolicy::DropOldest => {
                    let _ = guard.items.pop_front();
                    guard.dropped += 1;
                    log_overflow(self.class, "dropped");
                    return match self.push_free(&mut guard, packet) {
                        EnqueueOutcome::Accepted { depth } => {
                            EnqueueOutcome::DroppedOldest { depth }
                        }
                        other => other,
                    };
                }
                OverflowPolicy::Coalesce => unreachable!("coalesce handled above"),
                OverflowPolicy::BlockTimeout => {
                    let timeout = match wait {
                        WaitMode::Never => Duration::ZERO,
                        WaitMode::HonorPolicy => self.block_timeout,
                    };
                    if timeout.is_zero() {
                        guard.rejected += 1;
                        log_overflow(self.class, "rejected");
                        return EnqueueOutcome::Rejected;
                    }
                    guard.producer_waits += 1;
                    let started = Instant::now();
                    let (g, wait_result) = self
                        .not_full
                        .wait_timeout(guard, timeout)
                        .unwrap_or_else(|e| e.into_inner());
                    guard = g;
                    guard.producer_wait_ns += started.elapsed().as_nanos() as u64;
                    if guard.closed {
                        return EnqueueOutcome::Shutdown;
                    }
                    if wait_result.timed_out() && guard.items.len() >= self.capacity {
                        guard.rejected += 1;
                        log_overflow(self.class, "rejected");
                        return EnqueueOutcome::Rejected;
                    }
                }
            }
        }
    }

    fn push_free(&self, guard: &mut QueueState, packet: IngressPacket) -> EnqueueOutcome {
        guard.items.push_back(packet);
        guard.accepted += 1;
        let depth = guard.items.len();
        if depth > guard.high_water_mark {
            guard.high_water_mark = depth;
        }
        self.not_empty.notify_one();
        EnqueueOutcome::Accepted { depth }
    }

    fn push_coalesce(&self, guard: &mut QueueState, packet: IngressPacket) -> EnqueueOutcome {
        if guard.items.is_empty() {
            return self.push_free(guard, packet);
        }
        guard.items.clear();
        guard.items.push_back(packet);
        guard.accepted += 1;
        guard.coalesced += 1;
        log_overflow(self.class, "coalesced");
        let depth = guard.items.len();
        if depth > guard.high_water_mark {
            guard.high_water_mark = depth;
        }
        self.not_empty.notify_one();
        EnqueueOutcome::Coalesced { depth }
    }

    fn pop(&self) -> Option<IngressPacket> {
        let mut guard = self.lock();
        let packet = guard.items.pop_front()?;
        self.not_full.notify_one();
        Some(packet)
    }

    fn drain_all(&self) -> Vec<IngressPacket> {
        let mut guard = self.lock();
        if guard.items.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(guard.items.len());
        while let Some(p) = guard.items.pop_front() {
            out.push(p);
        }
        self.not_full.notify_all();
        out
    }

    fn shutdown(&self) {
        let mut guard = self.lock();
        guard.closed = true;
        self.not_full.notify_all();
        self.not_empty.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn metrics(&self) -> ClassMetrics {
        let guard = self.lock();
        ClassMetrics {
            accepted: guard.accepted,
            rejected: guard.rejected,
            dropped: guard.dropped,
            coalesced: guard.coalesced,
            depth: guard.items.len(),
            high_water_mark: guard.high_water_mark,
            producer_waits: guard.producer_waits,
            producer_wait_ns: guard.producer_wait_ns,
        }
    }
}

/// At most one warn per class per second (low-cardinality `class` + `outcome`).
fn log_overflow(class: MessageClass, outcome: &'static str) {
    static LAST_NS: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    const INTERVAL_NS: u64 = 1_000_000_000;
    let now = now_ns();
    let slot = &LAST_NS[class.index()];
    let last = slot.load(Ordering::Relaxed);
    if now.saturating_sub(last) < INTERVAL_NS {
        return;
    }
    if slot
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    tracing::warn!(class = class.as_str(), outcome, "ingress overflow");
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

struct IngressShared {
    config: IngressConfig,
    queues: [BoundedQueue; 4],
}

/// Cloneable handle to the four bounded class queues.
#[derive(Clone)]
pub struct BoundedIngress {
    inner: Arc<IngressShared>,
}

impl BoundedIngress {
    pub fn new(config: IngressConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let timeout = config.block_timeout();
        let make = |class: MessageClass| {
            BoundedQueue::new(class, config.capacity(class), config.policy(class), timeout)
        };
        Ok(Self {
            inner: Arc::new(IngressShared {
                queues: [
                    make(MessageClass::Sensory),
                    make(MessageClass::Reward),
                    make(MessageClass::Control),
                    make(MessageClass::Telemetry),
                ],
                config,
            }),
        })
    }

    pub fn config(&self) -> &IngressConfig {
        &self.inner.config
    }

    fn queue(&self, class: MessageClass) -> &BoundedQueue {
        &self.inner.queues[class.index()]
    }

    /// Enqueue honoring the class policy, including `block_timeout` waits.
    pub fn enqueue(&self, class: MessageClass, packet: IngressPacket) -> EnqueueOutcome {
        self.queue(class).push(packet, WaitMode::HonorPolicy)
    }

    /// Never wait. `block_timeout` queues reject immediately when full.
    ///
    /// The tick loop uses this so admitting a backend packet cannot stall the
    /// 1 kHz cadence.
    pub fn try_enqueue(&self, class: MessageClass, packet: IngressPacket) -> EnqueueOutcome {
        self.queue(class).push(packet, WaitMode::Never)
    }

    /// Split a backend packet onto the sensory and reward queues.
    pub fn admit_backend_packet(&self, packet: IngressPacket) {
        let IngressPacket {
            stimuli,
            modulators,
        } = packet;
        let _ = self.try_enqueue(
            MessageClass::Sensory,
            IngressPacket {
                stimuli,
                modulators: None,
            },
        );
        if let Some(modulators) = modulators {
            let _ = self.try_enqueue(
                MessageClass::Reward,
                IngressPacket {
                    stimuli: Vec::new(),
                    modulators: Some(modulators),
                },
            );
        }
    }

    pub fn pop(&self, class: MessageClass) -> Option<IngressPacket> {
        self.queue(class).pop()
    }

    /// Drain in priority order: all control, one reward, one sensory, all telemetry.
    pub fn drain_for_tick(&self) -> DrainedTick {
        DrainedTick {
            control: self.queue(MessageClass::Control).drain_all(),
            reward: self.queue(MessageClass::Reward).pop(),
            sensory: self.queue(MessageClass::Sensory).pop(),
            telemetry_drained: self.queue(MessageClass::Telemetry).drain_all().len(),
        }
    }

    /// Unblock waiting producers and refuse further enqueue.
    pub fn shutdown(&self) {
        for q in &self.inner.queues {
            q.shutdown();
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.inner.queues.iter().all(|q| q.is_closed())
    }

    pub fn metrics(&self) -> IngressMetrics {
        IngressMetrics {
            sensory: self.queue(MessageClass::Sensory).metrics(),
            reward: self.queue(MessageClass::Reward).metrics(),
            control: self.queue(MessageClass::Control).metrics(),
            telemetry: self.queue(MessageClass::Telemetry).metrics(),
        }
    }

    /// Markdown table for docs / PR snapshots.
    pub fn render_snapshot(&self) -> String {
        let m = self.metrics();
        let cfg = &self.inner.config;
        let mut out = String::from(
            "| class | policy | cap | depth | hwm | accepted | rejected | dropped | coalesced | waits | wait_ns |\n\
             |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for class in MessageClass::ALL {
            let c = m.class(class);
            let cap = match cfg.policy(class) {
                OverflowPolicy::Coalesce => 1,
                _ => cfg.capacity(class),
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                class.as_str(),
                cfg.policy(class).as_str(),
                cap,
                c.depth,
                c.high_water_mark,
                c.accepted,
                c.rejected,
                c.dropped,
                c.coalesced,
                c.producer_waits,
                c.producer_wait_ns,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn pkt(tag: f32) -> IngressPacket {
        IngressPacket {
            stimuli: vec![tag],
            modulators: None,
        }
    }

    fn reward_pkt(tag: f32) -> IngressPacket {
        IngressPacket {
            stimuli: Vec::new(),
            modulators: Some(vec![tag, 0.0, 0.0, 0.0]),
        }
    }

    fn identity_holds(attempts: u64, metrics: ClassMetrics) {
        assert_eq!(
            metrics.accepted + metrics.rejected,
            attempts,
            "accepted + rejected must equal enqueue attempts: {metrics:?}"
        );
        assert_eq!(
            metrics.depth as u64 + metrics.lost_or_coalesced(),
            attempts,
            "stalled identity: depth + lost/coalesced = attempts: {metrics:?}"
        );
    }

    #[test]
    fn drop_oldest_bounds_depth_with_stalled_consumer() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        const N: u64 = 50;
        for i in 0..N {
            let outcome = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
            assert!(outcome.accepted());
        }
        let m = ingress.metrics().sensory;
        assert_eq!(m.depth, 2);
        assert_eq!(m.high_water_mark, 2);
        assert_eq!(m.accepted, N);
        assert_eq!(m.dropped, N - 2);
        assert_eq!(m.rejected, 0);
        assert_eq!(m.coalesced, 0);
        identity_holds(N, m);

        // Latest two frames remain (FIFO drop-oldest).
        let a = ingress.pop(MessageClass::Sensory).unwrap();
        let b = ingress.pop(MessageClass::Sensory).unwrap();
        assert_eq!(a.stimuli[0], (N - 2) as f32);
        assert_eq!(b.stimuli[0], (N - 1) as f32);
        assert!(ingress.pop(MessageClass::Sensory).is_none());
    }

    #[test]
    fn reject_does_not_grow_past_capacity() {
        let mut cfg = IngressConfig::tiny_fixture();
        cfg.sensory_policy = OverflowPolicy::Reject;
        cfg.sensory_capacity = 1;
        let ingress = BoundedIngress::new(cfg).unwrap();

        assert!(ingress.enqueue(MessageClass::Sensory, pkt(1.0)).accepted());
        assert_eq!(
            ingress.enqueue(MessageClass::Sensory, pkt(2.0)),
            EnqueueOutcome::Rejected
        );
        assert_eq!(
            ingress.enqueue(MessageClass::Sensory, pkt(3.0)),
            EnqueueOutcome::Rejected
        );

        let m = ingress.metrics().sensory;
        assert_eq!(m.depth, 1);
        assert_eq!(m.accepted, 1);
        assert_eq!(m.rejected, 2);
        identity_holds(3, m);
        assert_eq!(ingress.pop(MessageClass::Sensory).unwrap().stimuli[0], 1.0);
    }

    #[test]
    fn coalesce_keeps_latest_snapshot() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        assert!(
            ingress
                .enqueue(MessageClass::Reward, reward_pkt(1.0))
                .accepted()
        );
        assert_eq!(
            ingress.enqueue(MessageClass::Reward, reward_pkt(2.0)),
            EnqueueOutcome::Coalesced { depth: 1 }
        );
        assert_eq!(
            ingress.enqueue(MessageClass::Reward, reward_pkt(3.0)),
            EnqueueOutcome::Coalesced { depth: 1 }
        );

        let m = ingress.metrics().reward;
        assert_eq!(m.depth, 1);
        assert_eq!(m.high_water_mark, 1);
        assert_eq!(m.accepted, 3);
        assert_eq!(m.coalesced, 2);
        assert_eq!(m.dropped, 0);
        assert_eq!(m.rejected, 0);
        identity_holds(3, m);

        let got = ingress.pop(MessageClass::Reward).unwrap();
        assert_eq!(got.modulators.unwrap()[0], 3.0);
    }

    #[test]
    fn lost_or_coalesced_increments_exactly_one_counter() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        for i in 0..10 {
            let _ = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
            let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(i as f32));
            let _ = ingress.enqueue(MessageClass::Control, pkt(i as f32));
            let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
        }

        for class in [
            MessageClass::Sensory,
            MessageClass::Reward,
            MessageClass::Control,
            MessageClass::Telemetry,
        ] {
            let m = ingress.metrics().class(class);
            let distinct_loss_buckets =
                u64::from(m.rejected > 0) + u64::from(m.dropped > 0) + u64::from(m.coalesced > 0);
            assert!(
                distinct_loss_buckets <= 1,
                "{class:?} mixed loss counters: {m:?}"
            );
            identity_holds(10, m);
        }
    }

    #[test]
    fn control_is_not_starved_by_full_telemetry() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        for i in 0..20 {
            let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
        }
        let tel = ingress.metrics().telemetry;
        assert_eq!(tel.depth, 2);
        assert!(tel.dropped >= 18);

        let outcome = ingress.enqueue(MessageClass::Control, pkt(99.0));
        assert_eq!(outcome, EnqueueOutcome::Accepted { depth: 1 });

        let drained = ingress.drain_for_tick();
        assert_eq!(drained.control.len(), 1);
        assert_eq!(drained.control[0].stimuli[0], 99.0);
        assert_eq!(drained.telemetry_drained, 2);
        assert!(drained.sensory.is_none());
    }

    #[test]
    fn drain_for_tick_priority_is_control_then_reward_then_sensory() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        let _ = ingress.enqueue(MessageClass::Telemetry, pkt(1.0));
        let _ = ingress.enqueue(MessageClass::Sensory, pkt(2.0));
        let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(3.0));
        let _ = ingress.enqueue(MessageClass::Control, pkt(4.0));

        let drained = ingress.drain_for_tick();
        assert_eq!(drained.control.len(), 1);
        assert_eq!(drained.control[0].stimuli[0], 4.0);
        assert_eq!(drained.telemetry_drained, 1);
        let packet = drained.into_packet();
        assert_eq!(packet.stimuli, vec![2.0]);
        assert_eq!(packet.modulators.unwrap()[0], 3.0);
    }

    #[test]
    fn block_timeout_zero_rejects_when_full_without_waiting() {
        let mut cfg = IngressConfig::tiny_fixture();
        cfg.control_policy = OverflowPolicy::BlockTimeout;
        cfg.control_capacity = 1;
        cfg.block_timeout_ms = 0;
        let ingress = BoundedIngress::new(cfg).unwrap();

        assert!(ingress.enqueue(MessageClass::Control, pkt(1.0)).accepted());
        assert_eq!(
            ingress.enqueue(MessageClass::Control, pkt(2.0)),
            EnqueueOutcome::Rejected
        );
        let m = ingress.metrics().control;
        assert_eq!(m.producer_waits, 0);
        assert_eq!(m.rejected, 1);
        assert_eq!(m.depth, 1);
    }

    #[test]
    fn try_enqueue_never_blocks_block_timeout_class() {
        let mut cfg = IngressConfig::default();
        cfg.control_policy = OverflowPolicy::BlockTimeout;
        cfg.control_capacity = 1;
        cfg.block_timeout_ms = 60_000;
        let ingress = BoundedIngress::new(cfg).unwrap();
        assert!(
            ingress
                .try_enqueue(MessageClass::Control, pkt(1.0))
                .accepted()
        );
        assert_eq!(
            ingress.try_enqueue(MessageClass::Control, pkt(2.0)),
            EnqueueOutcome::Rejected
        );
    }

    #[test]
    fn shutdown_unblocks_waiting_producer() {
        let mut cfg = IngressConfig::tiny_fixture();
        cfg.control_policy = OverflowPolicy::BlockTimeout;
        cfg.control_capacity = 1;
        cfg.block_timeout_ms = 60_000;
        let ingress = BoundedIngress::new(cfg).unwrap();
        assert!(ingress.enqueue(MessageClass::Control, pkt(1.0)).accepted());

        let producer = ingress.clone();
        let handle = thread::spawn(move || producer.enqueue(MessageClass::Control, pkt(2.0)));

        while ingress.metrics().control.producer_waits == 0 {
            thread::yield_now();
        }
        ingress.shutdown();
        let outcome = handle.join().expect("producer thread panicked");
        assert_eq!(outcome, EnqueueOutcome::Shutdown);
        assert!(ingress.is_shutdown());

        assert_eq!(
            ingress.enqueue(MessageClass::Control, pkt(3.0)),
            EnqueueOutcome::Shutdown
        );
    }

    #[test]
    fn sustained_overload_bounds_memory_and_accounts_every_event() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        const N: u64 = 10_000;
        for i in 0..N {
            let _ = ingress.enqueue(MessageClass::Sensory, pkt(i as f32));
            let _ = ingress.enqueue(MessageClass::Reward, reward_pkt(i as f32));
            let _ = ingress.enqueue(MessageClass::Telemetry, pkt(i as f32));
            if i % 100 == 0 {
                let _ = ingress.enqueue(MessageClass::Control, pkt(i as f32));
            }
        }

        let m = ingress.metrics();
        assert_eq!(m.sensory.depth, 2);
        assert_eq!(m.sensory.high_water_mark, 2);
        identity_holds(N, m.sensory);

        assert_eq!(m.reward.depth, 1);
        assert_eq!(m.reward.high_water_mark, 1);
        identity_holds(N, m.reward);

        assert_eq!(m.telemetry.depth, 2);
        identity_holds(N, m.telemetry);

        let control_attempts = N / 100;
        assert_eq!(m.control.depth, 4);
        identity_holds(control_attempts, m.control);
        // Control still accepted while telemetry was overflowing.
        assert!(m.control.accepted >= 4);
        assert_eq!(m.control.accepted, 4);
        assert_eq!(m.control.rejected, control_attempts - 4);

        let snapshot = ingress.render_snapshot();
        assert!(snapshot.contains("| sensory | drop_oldest |"));
        assert!(snapshot.contains("| control | reject |"));
        assert!(snapshot.contains("| reward | coalesce |"));
        eprintln!("overload fixture metrics:\n{snapshot}");
    }

    #[test]
    fn zero_capacity_is_rejected_at_construction() {
        let mut cfg = IngressConfig::default();
        cfg.sensory_capacity = 0;
        let err = match BoundedIngress::new(cfg) {
            Ok(_) => panic!("expected zero capacity to fail"),
            Err(err) => err,
        };
        let err = err.to_string();
        assert!(err.contains("sensory"));
        assert!(err.contains("capacity"));
    }

    #[test]
    fn admit_backend_packet_splits_stimuli_and_modulators() {
        let ingress = BoundedIngress::new(IngressConfig::tiny_fixture()).unwrap();
        ingress.admit_backend_packet(IngressPacket {
            stimuli: vec![0.5, 0.25],
            modulators: Some(vec![1.0, 2.0, 3.0, 4.0]),
        });
        let drained = ingress.drain_for_tick();
        let packet = drained.into_packet();
        assert_eq!(packet.stimuli, vec![0.5, 0.25]);
        assert_eq!(packet.modulators.unwrap()[0], 1.0);
    }
}
