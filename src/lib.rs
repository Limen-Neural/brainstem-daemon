// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon library: config-driven service registry and runtime.

pub mod backend;
pub mod checkpoint;
pub mod daemon;
pub mod ingress;
pub mod registry;

// Re-export the new pluggable I/O surface (pub from day one).
pub use backend::{
    BackendPair, IngressPacket, NEUROMODULATOR_COUNT, SpikeEvent, SpikeSink, StimulusSource,
};
pub use checkpoint::{ModelProvenance, restore_network};
pub use daemon::RuntimeMode;
pub use ingress::{
    BoundedIngress, ClassMetrics, DrainedTick, EnqueueOutcome, IngressConfig, IngressMetrics,
    MessageClass, OverflowPolicy,
};
