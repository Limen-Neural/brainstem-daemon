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
///
/// Bounds are `Send` (the daemon uses exclusive `&mut self` access on a
/// current-thread runtime; `Sync` is not required for safety).
pub trait StimulusSource: Send {
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
///
/// Bounds are `Send` (the daemon uses exclusive `&mut self` access on a
/// current-thread runtime; `Sync` is not required for safety).
pub trait SpikeSink: Send {
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
    pub source: Box<dyn StimulusSource + Send>,
    pub sink: Box<dyn SpikeSink + Send>,
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
    pub emitted: Vec<Vec<SpikeEvent>>,
}

#[cfg(test)]
impl CollectingSpikeSink {
    pub fn new() -> Self {
        Self {
            emitted: Vec::new(),
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
        self.emitted.push(spikes.to_vec());
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

    pub struct ZmqStimulusSource {
        inner: ZmqBrainBackend,
        channels: usize,
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
                channels: 0,
            }
        }

        /// Construct with known channel count so `next_ingress` can split
        /// stimulus prefix from appended neuromodulator tail (4 floats).
        ///
        /// The default `new()` uses `channels=0`, which means the entire readout
        /// is passed as stimuli and no modulators are extracted. Library users
        /// who want automatic modulator extraction must use `with_channels(cfg.channels)`.
        pub fn with_channels(ch: usize) -> Self {
            Self {
                inner: ZmqBrainBackend::new(),
                channels: ch,
            }
        }
    }

    impl StimulusSource for ZmqStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            let readout = self.inner.process_signals(&[])?;
            let ch = self.channels;
            if ch > 0 && readout.len() > ch {
                let stimuli = readout[..ch].to_vec();
                let modulators = if readout.len() >= ch + 4 {
                    Some(readout[ch..ch + 4].to_vec())
                } else {
                    None
                };
                Ok(Some(IngressPacket {
                    stimuli,
                    modulators,
                }))
            } else {
                Ok(Some(IngressPacket {
                    stimuli: readout,
                    modulators: None,
                }))
            }
        }

        fn initialize(&mut self, model_path: Option<&str>) -> Result<()> {
            self.inner.initialize(model_path)?;
            Ok(())
        }
    }

    // ZMQ sockets are not thread-safe by default (raw pointer inside).
    // We impl Send only (exclusive &mut use on current_thread runtime).
    // Do not impl Sync; ZeroMQ sockets are not thread-safe.
    struct SafeSocket {
        socket: ::zmq::Socket,
    }
    unsafe impl Send for SafeSocket {}

    pub struct ZmqSpikeSink {
        socket: std::sync::Mutex<SafeSocket>,
        /// Reusable buffer to convert to corpus-ipc event type without allocating every tick.
        corpus_buf: Vec<CorpusSpikeEvent>,
    }

    impl ZmqSpikeSink {
        pub fn new(socket: ::zmq::Socket) -> Self {
            Self {
                socket: std::sync::Mutex::new(SafeSocket { socket }),
                corpus_buf: Vec::new(),
            }
        }
    }

    impl SpikeSink for ZmqSpikeSink {
        fn emit(&mut self, spikes: &[SpikeEvent]) -> Result<()> {
            // Full-width wall-clock for batch metadata (matches original wire protocol precision).
            // SpikeEvent.time only holds truncated lower 32 bits of epoch ms; do not use it here.
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let batch_id = now.as_millis() as u64;
            let timestamp = now.as_nanos() as u64;

            // Reuse buffer capacity across ticks (capacity-preserving handoff pattern).
            self.corpus_buf.clear();
            self.corpus_buf
                .extend(spikes.iter().map(|e| CorpusSpikeEvent {
                    channel: e.channel,
                    time: e.time,
                    strength: e.strength,
                }));
            let cap = self.corpus_buf.capacity();
            let corpus_spikes = std::mem::replace(&mut self.corpus_buf, Vec::with_capacity(cap));

            let msg = SpineMessage::Spikes(SpikeBatch {
                session_id: None,
                batch_id,
                timestamp,
                spikes: corpus_spikes,
                metadata: None,
            });

            let payload = serde_json::to_vec(&msg)?;
            let guard = self
                .socket
                .lock()
                .map_err(|_| anyhow::anyhow!("ZMQ socket mutex poisoned"))?;
            guard.socket.send(payload, 0)?;
            Ok(())
        }
    }
}

#[cfg(feature = "corpus-ipc")]
pub use zmq_impl::{ZmqSpikeSink, ZmqStimulusSource};
