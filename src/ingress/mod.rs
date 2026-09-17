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

mod queue;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;

use crate::backend::IngressPacket;

use queue::{BoundedQueue, WaitMode};

/// Hard cap so a TOML typo cannot request an enormous `VecDeque`.
pub const MAX_QUEUE_CAPACITY: usize = 16_384;

/// Hard cap so `block_timeout` cannot overflow platform `Instant` addition.
pub const MAX_BLOCK_TIMEOUT_MS: u64 = 86_400_000;

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
fn default_max_payload_len() -> usize {
    4_096
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
    /// Max `stimuli` / `modulators` length admitted into any class queue.
    #[serde(default = "default_max_payload_len")]
    pub max_payload_len: usize,
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
            max_payload_len: default_max_payload_len(),
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
            max_payload_len: default_max_payload_len(),
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

    /// Capacities must be in `1..=MAX_QUEUE_CAPACITY`. Payload length must be ≥ 1.
    /// `block_timeout_ms` must be in `0..=MAX_BLOCK_TIMEOUT_MS`.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_payload_len == 0 {
            anyhow::bail!("ingress max_payload_len must be >= 1");
        }
        if self.block_timeout_ms > MAX_BLOCK_TIMEOUT_MS {
            anyhow::bail!(
                "ingress block_timeout_ms {} exceeds MAX_BLOCK_TIMEOUT_MS ({})",
                self.block_timeout_ms,
                MAX_BLOCK_TIMEOUT_MS
            );
        }
        for class in [
            MessageClass::Sensory,
            MessageClass::Reward,
            MessageClass::Control,
            MessageClass::Telemetry,
        ] {
            let cap = self.capacity(class);
            if cap == 0 {
                anyhow::bail!("ingress {} capacity must be >= 1", class.as_str());
            }
            if cap > MAX_QUEUE_CAPACITY {
                anyhow::bail!(
                    "ingress {} capacity {} exceeds MAX_QUEUE_CAPACITY ({})",
                    class.as_str(),
                    cap,
                    MAX_QUEUE_CAPACITY
                );
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
    pub shutdown_refused: u64,
    pub depth: usize,
    pub high_water_mark: usize,
    pub producer_waits: u64,
    pub producer_wait_ns: u64,
}

impl ClassMetrics {
    /// Overflow losses that never reached a consumer (not shutdown).
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
    ///
    /// Control envelopes are intentionally separate: the network step has no
    /// control actuator. Callers must inspect `control` (the tick loop logs it)
    /// so those events are observed rather than starved behind bulk.
    pub fn into_packet(self) -> IngressPacket {
        IngressPacket {
            stimuli: self.sensory.map(|p| p.stimuli).unwrap_or_default(),
            modulators: self.reward.and_then(|p| p.modulators),
        }
    }
}

struct IngressShared {
    config: IngressConfig,
    queues: [BoundedQueue; 4],
    shutting_down: AtomicBool,
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
        let max_payload_len = config.max_payload_len;
        let make = |class: MessageClass| {
            BoundedQueue::new(
                class,
                config.capacity(class),
                config.policy(class),
                timeout,
                max_payload_len,
            )
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
                shutting_down: AtomicBool::new(false),
            }),
        })
    }

    pub fn config(&self) -> &IngressConfig {
        &self.inner.config
    }

    fn queue(&self, class: MessageClass) -> &BoundedQueue {
        &self.inner.queues[class.index()]
    }

    fn push(&self, class: MessageClass, packet: IngressPacket, wait: WaitMode) -> EnqueueOutcome {
        // Shared flag is set before any per-queue close so a producer racing
        // shutdown cannot land on a still-open later class.
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return self.queue(class).refuse_closed();
        }
        self.queue(class).push(packet, wait)
    }

    /// Enqueue honoring the class policy, including `block_timeout` waits.
    pub fn enqueue(&self, class: MessageClass, packet: IngressPacket) -> EnqueueOutcome {
        self.push(class, packet, WaitMode::HonorPolicy)
    }

    /// Never wait. `block_timeout` queues reject immediately when full.
    ///
    /// The tick loop uses this so admitting a backend packet cannot stall the
    /// 1 kHz cadence.
    pub fn try_enqueue(&self, class: MessageClass, packet: IngressPacket) -> EnqueueOutcome {
        self.push(class, packet, WaitMode::Never)
    }

    /// Split a backend packet onto the sensory and reward queues.
    /// Empty `stimuli` skip the sensory queue (they cannot evict in-process
    /// sensory). Non-empty `modulators` are still admitted to the reward queue.
    pub fn admit_backend_packet(&self, packet: IngressPacket) {
        let IngressPacket {
            stimuli,
            modulators,
        } = packet;
        if !stimuli.is_empty() {
            let _ = self.try_enqueue(
                MessageClass::Sensory,
                IngressPacket {
                    stimuli,
                    modulators: None,
                },
            );
        }
        if let Some(modulators) = modulators.filter(|m| !m.is_empty()) {
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
        self.inner.shutting_down.store(true, Ordering::Release);
        for q in &self.inner.queues {
            q.shutdown();
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
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
            "| class | policy | cap | depth | hwm | accepted | rejected | dropped | coalesced | shutdown | waits | wait_ns |\n\
             |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for class in MessageClass::ALL {
            let c = m.class(class);
            let cap = match cfg.policy(class) {
                OverflowPolicy::Coalesce => 1,
                _ => cfg.capacity(class),
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                class.as_str(),
                cfg.policy(class).as_str(),
                cap,
                c.depth,
                c.high_water_mark,
                c.accepted,
                c.rejected,
                c.dropped,
                c.coalesced,
                c.shutdown_refused,
                c.producer_waits,
                c.producer_wait_ns,
            ));
        }
        out
    }
}
