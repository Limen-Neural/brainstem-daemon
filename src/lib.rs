// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! CPU inference runtime for spiking neural networks, with configurable I/O.
//!
//! Load and validate TOML with [`DaemonConfig::load`]. [`BrainstemDaemon::try_new`]
//! uses an in-memory stub, even with the `corpus-ipc` feature enabled. The
//! feature enables ZeroMQ types and lets the binary wire them. Library callers
//! can inject an explicit [`BackendPair`] with [`BrainstemDaemon::try_with_backend`].
//!
//! [`RuntimeMode::Live`] is the default and requires a validated, nonblank,
//! LIF-only Distill JSON checkpoint. Sidecars containing `output_weights`
//! (including `null`) and FPGA `.mem` images are unsupported. Choose
//! [`RuntimeMode::Simulation`] explicitly to step a blank network. The 0.3.0
//! dependency contract uses `neuromod` 0.7.x and the optional `corpus-ipc` 0.2.x
//! wire types (tested with 0.2.1).
//!
//! A bounded stub simulation needs no sockets, model file, or async executor:
//!
//! ```
//! use brainstem_daemon::{BrainstemDaemon, DaemonConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config: DaemonConfig = toml::from_str(r#"
//! runtime_mode = "simulation"
//! lif_count = 1
//! izh_count = 0
//! channels = 1
//! tick_rate_hz = 100
//! log_level = "info"
//! spine_sub_port = 5555
//! spine_pub_port = 5556
//! model_path = "unused-in-simulation.json"
//! "#)?;
//! let stats = BrainstemDaemon::try_new(config)?.run_for_ticks(1)?;
//! assert_eq!(stats.ticks, 1);
//! # Ok(())
//! # }
//! ```

pub mod backend;
pub mod checkpoint;
pub mod config;
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
