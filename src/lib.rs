// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon library: config-driven service registry and runtime.

pub mod backend;
pub mod checkpoint;
pub mod control;
pub mod daemon;
pub mod health;
pub mod ingress;
pub mod logging;
pub mod registry;
pub mod runtime;

// Re-export the new pluggable I/O surface (pub from day one).
pub use backend::{
    BackendPair, CollectingSpikeSink, IngressPacket, NEUROMODULATOR_COUNT, SpikeEvent, SpikeSink,
    StimulusSource,
};
pub use checkpoint::{ModelProvenance, restore_network};
pub use daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode, RuntimeStats};
pub use health::{
    CheckpointIdentity, FakeClock, FatalCode, HealthEvent, HealthHandle, HealthLimits,
    HealthMachine, HealthPhase, HealthSnapshot, ReasonCode, SystemClock,
};
pub use ingress::{
    BoundedIngress, ClassMetrics, DrainedTick, EnqueueOutcome, IngressConfig, IngressMetrics,
    MAX_BLOCK_TIMEOUT_MS, MAX_PAYLOAD_LEN, MAX_QUEUE_CAPACITY, MessageClass, OverflowPolicy,
};
