// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Deterministic clock. `advance` never sleeps or yields.

/// Fake-clock nanoseconds advanced on a backpressure timeout (10 ms).
pub const BACKPRESSURE_BUDGET_NS: u64 = 10_000_000;

/// One nanosecond-scale tick period at 1 kHz, applied without sleeping.
pub const TICK_PERIOD_NS: u64 = 1_000_000;

/// Deterministic clock used by the fault-injection harness.
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
