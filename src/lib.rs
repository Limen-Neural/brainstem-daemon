// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Brainstem daemon library: config-driven service registry and runtime.

pub mod backend;
pub mod daemon;
pub mod registry;

// Re-export the new pluggable I/O surface (pub from day one).
pub use backend::{BackendPair, IngressPacket, SpikeEvent, SpikeSink, StimulusSource};
