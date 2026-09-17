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

/// Length of the neuromodulator tail appended after the stimulus prefix.
///
/// Order matches `neuromod` 0.6: dopamine, serotonin, acetylcholine,
/// norepinephrine. Do not invent a parallel in-tree modulator struct.
///
/// The optional `corpus-ipc` ZMQ adapter converts the pinned Nero tail
/// (dopamine, cortisol, acetylcholine, tempo) into this order before
/// filling [`IngressPacket::modulators`].
pub const NEUROMODULATOR_COUNT: usize = 4;

/// Packet returned by a `StimulusSource` for one tick.
#[derive(Debug, Clone, Default)]
pub struct IngressPacket {
    /// The core stimulus vector (the "readout" part expected by the network).
    pub stimuli: Vec<f32>,
    /// Optional raw modulator values in [`NEUROMODULATOR_COUNT`] order
    /// (dopamine, serotonin, acetylcholine, norepinephrine).
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
    ///
    /// `batch_time` is the tick-level wall-clock duration since `UNIX_EPOCH`
    /// that was used to stamp each `SpikeEvent.time` in this batch.  Sinks
    /// that emit batch metadata (e.g. ZMQ `batch_id` / `timestamp`) must use
    /// this value so both fields stay aligned with per-spike times.
    fn emit(&mut self, spikes: &[SpikeEvent], batch_time: std::time::Duration) -> Result<()>;

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
    fn emit(&mut self, _spikes: &[SpikeEvent], _batch_time: std::time::Duration) -> Result<()> {
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
    fn emit(&mut self, spikes: &[SpikeEvent], _batch_time: std::time::Duration) -> Result<()> {
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
        /// stimulus prefix from the pinned Nero neuromodulator tail
        /// ([`NEUROMODULATOR_COUNT`] floats: DA / cortisol / ACh / tempo).
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

    /// Split a pinned `ZmqBrainBackend` readout into stimuli + modulators.
    ///
    /// The ZMQ packet is an untyped `f32` vector. This crate's convention is:
    /// first `channels` floats are stimuli; the next four are the Nero tail
    /// documented by pinned `corpus-ipc` `NeroManifoldSnapshot` (dopamine,
    /// cortisol, acetylcholine, tempo). That snapshot type is not parsed here;
    /// only the positional layout is preserved.
    fn split_zmq_readout(readout: Vec<f32>, channels: usize) -> IngressPacket {
        if channels > 0 && readout.len() > channels {
            let stimuli = readout[..channels].to_vec();
            let modulators = if readout.len() >= channels + NEUROMODULATOR_COUNT {
                Some(pinned_nero_tail_to_v06(
                    &readout[channels..channels + NEUROMODULATOR_COUNT],
                ))
            } else {
                None
            };
            IngressPacket {
                stimuli,
                modulators,
            }
        } else {
            IngressPacket {
                stimuli: readout,
                modulators: None,
            }
        }
    }

    /// Convert the pinned Nero 4-float tail onto `neuromod` 0.6
    /// [`IngressPacket`] order (dopamine, serotonin, acetylcholine,
    /// norepinephrine).
    ///
    /// Cortisol and tempo have no 0.6 analogue. Leave serotonin /
    /// norepinephrine at `NeuroModulators::default()` rather than treating
    /// cortisol as 5-HT or tempo as NE.
    fn pinned_nero_tail_to_v06(tail: &[f32]) -> Vec<f32> {
        debug_assert!(tail.len() >= NEUROMODULATOR_COUNT);
        let defaults = neuromod::NeuroModulators::default();
        vec![
            tail[0],
            defaults.serotonin,
            tail[2],
            defaults.norepinephrine,
        ]
    }

    impl StimulusSource for ZmqStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            let readout = self.inner.process_signals(&[])?;
            Ok(Some(split_zmq_readout(readout, self.channels)))
        }

        fn initialize(&mut self, model_path: Option<&str>) -> Result<()> {
            self.inner.initialize(model_path)?;
            Ok(())
        }
    }

    // ZMQ sockets are not thread-safe (raw pointer inside).
    // We wrap in Mutex<SafeSocket> to provide Sync safety for the public trait
    // bound (even though the daemon currently uses exclusive &mut self on a
    // current_thread runtime). This directly addresses the high-priority Gemini
    // review requesting Mutex for Sync safety.
    // The extra lock cost is accepted for the safety guarantee on the public API.
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
        fn emit(&mut self, spikes: &[SpikeEvent], batch_time: std::time::Duration) -> Result<()> {
            // Use the tick-level timestamp passed by run_tick so batch metadata
            // stays aligned with the SpikeEvent.time values in this batch.
            let batch_id = batch_time.as_millis() as u64;
            let timestamp = batch_time.as_nanos() as u64;

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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn zmq_tail_forwards_da_ach_and_drops_cortisol_tempo() {
            let defaults = neuromod::NeuroModulators::default();
            // readout = [stim0, stim1, DA, cortisol, ACh, tempo]
            let packet = split_zmq_readout(vec![1.0, 2.0, 0.5, 0.9, 0.3, 1.5], 2);
            assert_eq!(packet.stimuli, vec![1.0, 2.0]);
            let mods = packet
                .modulators
                .expect("full Nero tail should produce modulators");
            assert_eq!(mods[0], 0.5);
            assert_eq!(mods[1], defaults.serotonin);
            assert_eq!(mods[2], 0.3);
            assert_eq!(mods[3], defaults.norepinephrine);
            assert_ne!(mods[1], 0.9, "cortisol must not be copied onto serotonin");
            assert_ne!(mods[3], 1.5, "tempo must not be copied onto norepinephrine");
        }

        #[test]
        fn zmq_short_tail_omits_modulators() {
            let packet = split_zmq_readout(vec![1.0, 2.0, 0.5], 2);
            assert_eq!(packet.stimuli, vec![1.0, 2.0]);
            assert!(packet.modulators.is_none());
        }

        #[test]
        fn zmq_zero_channels_passes_whole_readout_as_stimuli() {
            let packet = split_zmq_readout(vec![0.1, 0.2, 0.3, 0.4], 0);
            assert_eq!(packet.stimuli, vec![0.1, 0.2, 0.3, 0.4]);
            assert!(packet.modulators.is_none());
        }
    }
}

#[cfg(feature = "corpus-ipc")]
pub use zmq_impl::{ZmqSpikeSink, ZmqStimulusSource};
