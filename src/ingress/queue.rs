// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Per-class bounded queue with overflow policy and metrics.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::IngressPacket;

use super::{ClassMetrics, EnqueueOutcome, MessageClass, OverflowPolicy};

pub(super) enum WaitMode {
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
    shutdown_refused: u64,
    high_water_mark: usize,
    producer_waits: u64,
    producer_wait_ns: u64,
}

pub(super) struct BoundedQueue {
    capacity: usize,
    max_payload_len: usize,
    policy: OverflowPolicy,
    block_timeout: Duration,
    class: MessageClass,
    inner: Mutex<QueueState>,
    not_full: Condvar,
    not_empty: Condvar,
}

impl BoundedQueue {
    pub(super) fn new(
        class: MessageClass,
        capacity: usize,
        policy: OverflowPolicy,
        block_timeout: Duration,
        max_payload_len: usize,
    ) -> Self {
        let capacity = match policy {
            OverflowPolicy::Coalesce => 1,
            _ => capacity,
        };
        Self {
            capacity,
            max_payload_len,
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
                shutdown_refused: 0,
                high_water_mark: 0,
                producer_waits: 0,
                producer_wait_ns: 0,
            }),
            not_full: Condvar::default(),
            not_empty: Condvar::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn payload_too_large(&self, packet: &IngressPacket) -> bool {
        let mods = packet.modulators.as_ref().map_or(0, Vec::len);
        packet.stimuli.len() > self.max_payload_len || mods > self.max_payload_len
    }

    fn refuse(&self, guard: &mut QueueState, outcome: &'static str) -> EnqueueOutcome {
        guard.rejected += 1;
        log_overflow(self.class, outcome);
        EnqueueOutcome::Rejected
    }

    fn refuse_shutdown(&self, guard: &mut QueueState) -> EnqueueOutcome {
        guard.shutdown_refused += 1;
        EnqueueOutcome::Shutdown
    }

    pub(super) fn refuse_closed(&self) -> EnqueueOutcome {
        let mut guard = self.lock();
        self.refuse_shutdown(&mut guard)
    }

    pub(super) fn push(&self, packet: IngressPacket, wait: WaitMode) -> EnqueueOutcome {
        if self.payload_too_large(&packet) {
            let mut guard = self.lock();
            if guard.closed {
                return self.refuse_shutdown(&mut guard);
            }
            return self.refuse(&mut guard, "rejected");
        }

        let deadline = match wait {
            WaitMode::Never => None,
            WaitMode::HonorPolicy if self.block_timeout.is_zero() => None,
            // checked_add: a Duration that overflows Instant must not panic.
            WaitMode::HonorPolicy => Instant::now().checked_add(self.block_timeout),
        };

        let mut guard = self.lock();
        loop {
            if guard.closed {
                return self.refuse_shutdown(&mut guard);
            }
            if self.policy == OverflowPolicy::Coalesce {
                return self.push_coalesce(&mut guard, packet);
            }
            if guard.items.len() < self.capacity {
                return self.push_free(&mut guard, packet);
            }
            match self.policy {
                OverflowPolicy::Reject => return self.refuse(&mut guard, "rejected"),
                OverflowPolicy::DropOldest => return self.drop_oldest_and_push(&mut guard, packet),
                OverflowPolicy::Coalesce => unreachable!("coalesce handled above"),
                OverflowPolicy::BlockTimeout => match self.wait_for_slot(guard, deadline) {
                    Ok(g) => guard = g,
                    Err(outcome) => return outcome,
                },
            }
        }
    }

    fn drop_oldest_and_push(
        &self,
        guard: &mut QueueState,
        packet: IngressPacket,
    ) -> EnqueueOutcome {
        let _ = guard.items.pop_front();
        guard.dropped += 1;
        log_overflow(self.class, "dropped");
        match self.push_free(guard, packet) {
            EnqueueOutcome::Accepted { depth } => EnqueueOutcome::DroppedOldest { depth },
            other => other,
        }
    }

    fn wait_for_slot<'a>(
        &'a self,
        mut guard: MutexGuard<'a, QueueState>,
        deadline: Option<Instant>,
    ) -> Result<MutexGuard<'a, QueueState>, EnqueueOutcome> {
        let remaining = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::ZERO,
        };
        if remaining.is_zero() {
            return Err(self.refuse(&mut guard, "rejected"));
        }
        guard.producer_waits += 1;
        let started = Instant::now();
        let (g, wait_result) = self
            .not_full
            .wait_timeout(guard, remaining)
            .unwrap_or_else(|e| e.into_inner());
        guard = g;
        guard.producer_wait_ns += started.elapsed().as_nanos() as u64;
        if guard.closed {
            return Err(self.refuse_shutdown(&mut guard));
        }
        if wait_result.timed_out() && guard.items.len() >= self.capacity {
            return Err(self.refuse(&mut guard, "rejected"));
        }
        Ok(guard)
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

    pub(super) fn pop(&self) -> Option<IngressPacket> {
        let mut guard = self.lock();
        let packet = guard.items.pop_front()?;
        self.not_full.notify_one();
        Some(packet)
    }

    pub(super) fn drain_all(&self) -> Vec<IngressPacket> {
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

    pub(super) fn shutdown(&self) {
        let mut guard = self.lock();
        guard.closed = true;
        self.not_full.notify_all();
        self.not_empty.notify_all();
    }

    pub(super) fn metrics(&self) -> ClassMetrics {
        let guard = self.lock();
        ClassMetrics {
            accepted: guard.accepted,
            rejected: guard.rejected,
            dropped: guard.dropped,
            coalesced: guard.coalesced,
            shutdown_refused: guard.shutdown_refused,
            depth: guard.items.len(),
            high_water_mark: guard.high_water_mark,
            producer_waits: guard.producer_waits,
            producer_wait_ns: guard.producer_wait_ns,
        }
    }
}

fn log_overflow(class: MessageClass, outcome: &'static str) {
    static LAST_NS: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    const INTERVAL_NS: u64 = 1_000_000_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
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
