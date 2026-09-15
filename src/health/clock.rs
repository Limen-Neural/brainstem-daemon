// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Monotonic clock used by the health state machine.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Wall-clock monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock. Clone and share the same offset with a [`super::HealthMachine`].
#[derive(Debug, Clone)]
pub struct FakeClock {
    origin: Instant,
    offset_nanos: Arc<AtomicU64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, duration: Duration) {
        let add = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let _ = self
            .offset_nanos
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_add(add))
            });
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            offset_nanos: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.origin + Duration::from_nanos(self.offset_nanos.load(Ordering::SeqCst))
    }
}
