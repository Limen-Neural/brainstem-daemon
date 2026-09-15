// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon library: config-driven service registry and runtime.

pub mod backend;
pub mod checkpoint;
pub mod daemon;
#[cfg(feature = "corpus-ipc")]
pub mod ingress;
pub mod registry;

// Re-export the new pluggable I/O surface (pub from day one).
pub use backend::{
    BackendPair, CollectingSpikeSink, IngressPacket, SpikeEvent, SpikeSink, StimulusSource,
};
pub use checkpoint::{
    CheckpointIdentity, NetworkDims, load_checkpoint, try_load_checkpoint,
    write_nonblank_checkpoint,
};
pub use daemon::{BrainstemDaemon, DaemonConfig, RuntimeStats};
