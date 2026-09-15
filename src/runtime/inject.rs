// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! One-shot fault injector and harness-owned health mapping.

use anyhow::{Result, bail};

use super::{Boundary, FaultPoint, Health};

#[derive(Debug, Default)]
pub(crate) struct FaultInjector {
    armed: Option<FaultPoint>,
}

impl FaultInjector {
    pub(crate) fn none() -> Self {
        Self { armed: None }
    }

    pub(crate) fn arm(point: FaultPoint) -> Self {
        Self { armed: Some(point) }
    }

    pub(crate) fn set(&mut self, point: FaultPoint) {
        self.armed = Some(point);
    }

    pub(crate) fn fire(&mut self, point: FaultPoint) -> Result<()> {
        if self.armed == Some(point) {
            self.armed = None;
            bail!(injected_message(point));
        }
        Ok(())
    }

    pub(crate) fn take(&mut self, point: FaultPoint) -> bool {
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

/// Terminal health assigned by the harness. Kept independent of
/// [`super::expected_outcome`] so the matrix test can detect drift.
pub(crate) fn injected_terminal_health(point: FaultPoint) -> Health {
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
