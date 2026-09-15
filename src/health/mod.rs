// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Process health: liveness, readiness, recoverable degradation, and sticky fatal state.
//!
//! Supervisors should treat [`HealthSnapshot::live`] and [`HealthSnapshot::ready`] as
//! independent. A live process has not necessarily loaded a valid checkpoint. Recoverable
//! reasons (`stale_input`, `overload`) clear only after the condition is observed healthy.
//! Fatal and draining states never return to ready in the same process.
//!
//! Snapshot reads take a separate lock from the tick loop's backend and network, so they
//! do not wait on `StimulusSource` or `SpikingNetwork::step`.

mod clock;
mod handle;
mod machine;
mod snapshot;

pub use clock::{Clock, FakeClock, SystemClock};
pub use handle::HealthHandle;
pub use machine::{HealthEvent, HealthLimits, HealthMachine};
pub use snapshot::{
    CheckpointIdentity, FatalCode, FatalState, HealthPhase, HealthSnapshot, InputFreshness,
    QueuePressure, ReasonCode,
};

#[cfg(test)]
mod tests;
