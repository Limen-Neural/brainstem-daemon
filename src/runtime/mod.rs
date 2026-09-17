// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Deterministic restart and fault-injection contract for the runtime.
//!
//! The live `BrainstemDaemon` tick loop still uses wall-clock `tokio` time.
//! Live mode restores Distill sidecar checkpoints (LIM-1133); this module is a
//! CPU-only fake-clock / fake-core harness that pins restart and fault-recovery
//! semantics independently of that path.
//!
//! # Durable vs volatile
//!
//! | Survives restart | Resets on restart |
//! |---|---|
//! | Checkpoint identity (`fake-core-v1`) | Open ingress/publish channels |
//! | `last_session_id` | Spike / metric buffers |
//! | `committed_tick_seq` | Fake-core last stimuli / step count |
//! | `committed_ingress_seq` | Backpressure wait position |
//! | In-flight marker (detected, then discarded) | `Health` (recomputed on boot) |
//!
//! An incomplete prior session is a durable `inflight` record: the previous
//! process died after accepting a tick and before publishing it. The next
//! boot clears that marker, does **not** emit a success for it, and allows
//! the same ingress sequence to be retried once as uncommitted work.
//!
//! Replayed ingress with `seq <= committed_ingress_seq` is skipped so it
//! cannot be double-counted as fresh work. Session ids are monotonic: every
//! boot that passes checkpoint validation uses `last_session_id + 1`.

mod clock;
mod durable;
mod fakes;
mod harness;
mod inject;

pub use clock::{BACKPRESSURE_BUDGET_NS, FakeClock, TICK_PERIOD_NS};
pub use durable::{
    DURABLE_SCHEMA_VERSION, DurableState, DurableStore, FAKE_CHECKPOINT_ID, InflightRecord,
};
pub use fakes::{CommittedOutput, FakeCore, MetricEvent, TickResult, packets_for_seed};
pub use harness::{HarnessBuilder, MAX_STEPS, RuntimeHarness};

/// Lifecycle boundaries that wrap a single documented side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Boundary {
    Initialize,
    CheckpointValidation,
    Ingress,
    TickExecute,
    MetricPublish,
    Shutdown,
}

/// Documented injection points. `Before` / `After` wrap each [`Boundary`];
/// the remaining variants are named operational faults at those boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    Before(Boundary),
    After(Boundary),
    MalformedCheckpoint,
    CoreStepError,
    ClosedChannel,
    BackpressureTimeout,
    InterruptedShutdown,
}

impl FaultPoint {
    /// Every injection point the suite is required to cover.
    pub const fn all() -> &'static [FaultPoint] {
        &ALL_FAULT_POINTS
    }
}

const ALL_FAULT_POINTS: [FaultPoint; 17] = [
    FaultPoint::Before(Boundary::Initialize),
    FaultPoint::After(Boundary::Initialize),
    FaultPoint::Before(Boundary::CheckpointValidation),
    FaultPoint::After(Boundary::CheckpointValidation),
    FaultPoint::Before(Boundary::Ingress),
    FaultPoint::After(Boundary::Ingress),
    FaultPoint::Before(Boundary::TickExecute),
    FaultPoint::After(Boundary::TickExecute),
    FaultPoint::Before(Boundary::MetricPublish),
    FaultPoint::After(Boundary::MetricPublish),
    FaultPoint::Before(Boundary::Shutdown),
    FaultPoint::After(Boundary::Shutdown),
    FaultPoint::MalformedCheckpoint,
    FaultPoint::CoreStepError,
    FaultPoint::ClosedChannel,
    FaultPoint::BackpressureTimeout,
    FaultPoint::InterruptedShutdown,
];

/// Seed matrix used by the fault suite. Reported in the PR description.
pub const FAULT_SEEDS: &[u64] = &[0, 1, 7, 42, 1337, 20260915];

/// Terminal health after the injected fault (or after a clean shutdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Starting,
    Live,
    CheckpointInvalid,
    Degraded,
    Faulted,
    ShuttingDown,
    Stopped,
    IncompleteShutdown,
}

/// How the *next* process boot interprets the durable store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOutcome {
    FreshStart,
    RecoveredClean,
    RecoveredIncomplete,
    RejectedInvalid,
}

/// Sequenced stimulus packet.
///
/// `seq` is a monotonic high-water mark and the idempotency key. After a
/// sequence is committed, any later packet with `seq <= committed_ingress_seq`
/// is treated as a replay, including a never-seen lower sequence delivered
/// after a higher one. Sources must emit strictly increasing sequences.
#[derive(Debug, Clone, PartialEq)]
pub struct SequencedIngress {
    pub seq: u64,
    pub stimuli: Vec<f32>,
}

/// Oracle for one injection point. Tests compare the harness to this table
/// so the PR matrix cannot drift from the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedOutcome {
    pub terminal_health: Health,
    pub restart: RestartOutcome,
    pub published_before_crash: u64,
    pub live_loop_entered: bool,
}

/// Expected terminal health and restart result for each injection point.
///
/// Shutdown faults run one successful tick first, so they publish once.
pub fn expected_outcome(point: FaultPoint) -> ExpectedOutcome {
    match point {
        FaultPoint::Before(boundary) => before_boundary(boundary),
        FaultPoint::After(boundary) => after_boundary(boundary),
        FaultPoint::MalformedCheckpoint => ExpectedOutcome {
            terminal_health: Health::CheckpointInvalid,
            restart: RestartOutcome::RejectedInvalid,
            published_before_crash: 0,
            live_loop_entered: false,
        },
        FaultPoint::CoreStepError => incomplete_tick(Health::Faulted),
        FaultPoint::ClosedChannel => ingress_degraded(),
        FaultPoint::BackpressureTimeout => incomplete_tick(Health::Degraded),
        FaultPoint::InterruptedShutdown => shutdown_incomplete(),
    }
}

fn before_boundary(boundary: Boundary) -> ExpectedOutcome {
    match boundary {
        Boundary::Initialize | Boundary::CheckpointValidation => ExpectedOutcome {
            terminal_health: Health::Faulted,
            restart: RestartOutcome::FreshStart,
            published_before_crash: 0,
            live_loop_entered: false,
        },
        Boundary::Ingress => ingress_degraded(),
        Boundary::TickExecute => ExpectedOutcome {
            terminal_health: Health::Faulted,
            restart: RestartOutcome::RecoveredClean,
            published_before_crash: 0,
            live_loop_entered: true,
        },
        Boundary::MetricPublish => incomplete_tick(Health::Degraded),
        Boundary::Shutdown => shutdown_incomplete(),
    }
}

fn after_boundary(boundary: Boundary) -> ExpectedOutcome {
    match boundary {
        Boundary::Initialize => ExpectedOutcome {
            terminal_health: Health::Faulted,
            restart: RestartOutcome::FreshStart,
            published_before_crash: 0,
            live_loop_entered: false,
        },
        Boundary::CheckpointValidation => ExpectedOutcome {
            terminal_health: Health::Faulted,
            restart: RestartOutcome::RecoveredClean,
            published_before_crash: 0,
            live_loop_entered: false,
        },
        Boundary::Ingress => ingress_degraded(),
        Boundary::TickExecute => incomplete_tick(Health::Faulted),
        Boundary::MetricPublish | Boundary::Shutdown => ExpectedOutcome {
            terminal_health: Health::Faulted,
            restart: RestartOutcome::RecoveredClean,
            published_before_crash: 1,
            live_loop_entered: true,
        },
    }
}

fn ingress_degraded() -> ExpectedOutcome {
    ExpectedOutcome {
        terminal_health: Health::Degraded,
        restart: RestartOutcome::RecoveredClean,
        published_before_crash: 0,
        live_loop_entered: true,
    }
}

fn incomplete_tick(terminal_health: Health) -> ExpectedOutcome {
    ExpectedOutcome {
        terminal_health,
        restart: RestartOutcome::RecoveredIncomplete,
        published_before_crash: 0,
        live_loop_entered: true,
    }
}

fn shutdown_incomplete() -> ExpectedOutcome {
    ExpectedOutcome {
        terminal_health: Health::IncompleteShutdown,
        restart: RestartOutcome::RecoveredClean,
        published_before_crash: 1,
        live_loop_entered: true,
    }
}

#[cfg(test)]
mod tests;
