// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use std::sync::{Arc, RwLock};

use super::clock::SystemClock;
use super::machine::{HealthEvent, HealthLimits, HealthMachine};
use super::snapshot::HealthSnapshot;

/// Cloneable, non-tick-blocking handle for supervisors and the control surface.
#[derive(Clone)]
pub struct HealthHandle {
    inner: Arc<RwLock<HealthMachine>>,
}

impl HealthHandle {
    /// Live process, not yet ready. Used when the daemon is constructed.
    pub fn started(limits: HealthLimits) -> Self {
        let mut machine = HealthMachine::new(SystemClock, limits);
        machine.apply(HealthEvent::ProcessStarted);
        Self {
            inner: Arc::new(RwLock::new(machine)),
        }
    }

    pub fn from_machine(machine: HealthMachine) -> Self {
        Self {
            inner: Arc::new(RwLock::new(machine)),
        }
    }

    pub fn apply(&self, event: HealthEvent) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.apply(event);
    }

    /// Clone the current snapshot. May wait only for an in-flight `apply` (no I/O).
    pub fn snapshot(&self) -> HealthSnapshot {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.snapshot()
    }

    /// Never waits. Returns `None` if a writer currently holds the lock.
    pub fn try_snapshot(&self) -> Option<HealthSnapshot> {
        match self.inner.try_read() {
            Ok(guard) => Some(guard.snapshot()),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner().snapshot()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn lock_write_for_test(&self) -> std::sync::RwLockWriteGuard<'_, HealthMachine> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}
