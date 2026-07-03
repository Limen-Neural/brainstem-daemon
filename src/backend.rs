// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Local pluggable I/O traits for stimulus ingress and spike egress.
//!
//! These types are owned by `brainstem-daemon`. They allow the core library
//! (config, registry, tick orchestration, etc.) to build and run without
//! pulling in `corpus-ipc` or `zmq`.
//!
//! When the `corpus-ipc` feature is enabled, ZMQ-based implementations are
//! provided that preserve the original wire protocol and behavior.
//!
//! This is part of the temporary decoupling effort (#10, #11) to focus on
//! core code quality first.

use anyhow::Result;

/// Packet returned by a `StimulusSource` for one tick.
#[derive(Debug, Clone, Default)]
pub struct IngressPacket {
    /// The core stimulus vector (the "readout" part expected by the network).
    pub stimuli: Vec<f32>,
    /// Optional raw modulator values (e.g. [dopamine, cortisol, acetylcholine, tempo, ...]).
    /// When `None`, the caller should use defaults (see `decode_inputs`).
    pub modulators: Option<Vec<f32>>,
}

/// Local spike event type (independent of any external crate).
#[derive(Debug, Clone)]
pub struct SpikeEvent {
    pub channel: u16,
    pub time: u32,
    pub strength: f32,
}

/// Produces ingress data (stimuli + optional modulators) for each tick.
pub trait StimulusSource: Send + Sync {
    /// Return the next ingress packet, or `None` to skip this tick (use zeroed stimuli).
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>>;

    /// One-time initialization (load weights, connect socket, etc.).
    /// Idempotent on success.
    fn initialize(&mut self, model_path: Option<&str>) -> Result<()>;

    /// Optional cleanup.
    fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Accepts emitted spikes for publication / downstream consumption.
pub trait SpikeSink: Send + Sync {
    /// Emit a batch of spikes from the current network step.
    fn emit(&mut self, spikes: &[SpikeEvent]) -> Result<()>;

    /// Optional flush for buffered sinks.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Pair of ingress/egress backends.
///
/// This is the main injection point for custom or test backends.
pub struct BackendPair {
    pub source: Box<dyn StimulusSource + Send + Sync>,
    pub sink: Box<dyn SpikeSink + Send + Sync>,
}

impl BackendPair {
    /// Create a simple stub pair for testing / core-only runs.
    /// The stub source always returns `modulators: None`.
    pub fn stub() -> Self {
        Self {
            source: Box::new(StubStimulusSource),
            sink: Box::new(NoopSpikeSink),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stub implementations (always available, no external dependencies)

/// Stub source: returns `Some(IngressPacket { stimuli: vec![], modulators: None })`.
/// Callers (e.g. the tick loop) are responsible for using configured channel count
/// to zero-fill the stimuli buffer when the packet is empty or `None`.
#[derive(Default)]
pub struct StubStimulusSource;

impl StimulusSource for StubStimulusSource {
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
        Ok(Some(IngressPacket {
            stimuli: Vec::new(),
            modulators: None,
        }))
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
        Ok(())
    }
}

/// No-op sink (used by `BackendPair::stub()`).
pub struct NoopSpikeSink;

impl SpikeSink for NoopSpikeSink {
    fn emit(&mut self, _spikes: &[SpikeEvent]) -> Result<()> {
        Ok(())
    }
}

/// Collecting sink for tests. Collects every emitted batch.
#[cfg(test)]
pub struct CollectingSpikeSink {
    pub emitted: std::sync::Mutex<Vec<Vec<SpikeEvent>>>,
}

#[cfg(test)]
impl CollectingSpikeSink {
    pub fn new() -> Self {
        Self {
            emitted: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Default for CollectingSpikeSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SpikeSink for CollectingSpikeSink {
    fn emit(&mut self, spikes: &[SpikeEvent]) -> Result<()> {
        self.emitted.lock().unwrap().push(spikes.to_vec());
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Feature-gated corpus-ipc / ZMQ implementations

#[cfg(feature = "corpus-ipc")]
mod zmq_impl {
    use super::*;
    // In the current pinned corpus-ipc revision, the main trait is exported as
    // `NeuralBackend` (deprecated alias). Importing it brings the trait methods
    // into scope for ZmqBrainBackend.
    use corpus_ipc::NeuralBackend as BackendConnector;
    use corpus_ipc::{SpikeBatch, SpikeEvent as CorpusSpikeEvent, SpineMessage, ZmqBrainBackend};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Environment variable name used by the ZMQ backend (corpus-ipc integration contract).
    /// The binary is responsible for setting this before the runtime when the feature is active.
    pub(crate) const CORPUS_IPC_READOUT_ENV: &str = "SPIKENAUT_ZMQ_READOUT_IPC";

    pub struct ZmqStimulusSource {
        inner: ZmqBrainBackend,
    }

    impl Default for ZmqStimulusSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ZmqStimulusSource {
        pub fn new() -> Self {
            Self {
                inner: ZmqBrainBackend::new(),
            }
        }
    }

    impl StimulusSource for ZmqStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            let readout = self.inner.process_signals(&[])?;
            Ok(Some(IngressPacket {
                stimuli: readout,
                modulators: None,
            }))
        }

        fn initialize(&mut self, model_path: Option<&str>) -> Result<()> {
            // Compat bridge: if SPIKENAUT name is set (what our binary uses),
            // and the CORPUS_IPC_ZMQ one is not, bridge it so the inner backend works.
            if let Ok(val) = std::env::var(CORPUS_IPC_READOUT_ENV)
                && std::env::var("CORPUS_IPC_ZMQ_READOUT_IPC").is_err()
            {
                // SAFETY: set only on main thread before runtime (binary) or per documented contract.
                unsafe {
                    std::env::set_var("CORPUS_IPC_ZMQ_READOUT_IPC", &val);
                }
            }
            self.inner.initialize(model_path)?;
            Ok(())
        }
    }

    // ZMQ sockets are not thread-safe by default (raw pointer inside).
    // We use the same pattern as corpus-ipc: exclusive ownership + unsafe Send+Sync.
    // This is safe because the daemon runs on a dedicated current_thread runtime
    // and the socket is not shared across threads.
    struct SafeSocket {
        socket: ::zmq::Socket,
    }
    unsafe impl Send for SafeSocket {}
    unsafe impl Sync for SafeSocket {}

    pub struct ZmqSpikeSink {
        socket: SafeSocket,
    }

    impl ZmqSpikeSink {
        pub fn new(socket: ::zmq::Socket) -> Self {
            Self {
                socket: SafeSocket { socket },
            }
        }
    }

    impl SpikeSink for ZmqSpikeSink {
        fn emit(&mut self, spikes: &[SpikeEvent]) -> Result<()> {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let tick = now.as_millis() as u64;

            let corpus_spikes: Vec<CorpusSpikeEvent> = spikes
                .iter()
                .map(|e| CorpusSpikeEvent {
                    channel: e.channel,
                    time: e.time,
                    strength: e.strength,
                })
                .collect();

            let msg = SpineMessage::Spikes(SpikeBatch {
                session_id: None,
                batch_id: tick,
                timestamp: now.as_nanos() as u64,
                spikes: corpus_spikes,
                metadata: None,
            });

            let payload = serde_json::to_vec(&msg)?;
            self.socket.socket.send(payload, 0)?;
            Ok(())
        }
    }
}

#[cfg(feature = "corpus-ipc")]
pub use zmq_impl::{ZmqSpikeSink, ZmqStimulusSource};
