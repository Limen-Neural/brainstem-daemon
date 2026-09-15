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
    /// Per-channel validity mask copied from a typed `StimulusBatch` when present.
    /// `false` means the corresponding stimulus is a placeholder, not a real zero.
    pub valid_mask: Option<Vec<bool>>,
    /// Optional typed batch id from `corpus-ipc` stimulus ingress.
    pub batch_id: Option<u64>,
    /// Optional stimulus timestamp in nanoseconds from `corpus-ipc`.
    pub timestamp_ns: Option<u64>,
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
            valid_mask: None,
            batch_id: None,
            timestamp_ns: None,
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

/// Collecting sink for tests and the integration smoke harness.
#[derive(Default)]
pub struct CollectingSpikeSink {
    pub emitted: Vec<Vec<SpikeEvent>>,
}

impl CollectingSpikeSink {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self {
            emitted: Vec::new(),
        }
    }
}

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
    use crate::ingress::{IngressPolicy, accept_ipc_json};
    use corpus_ipc::{IpcMessage, SpikeBatch, SpikeEvent as CorpusSpikeEvent};

    /// Env var read by this source for the SUB endpoint. The binary also sets
    /// the historical `SPIKENAUT_ZMQ_READOUT_IPC` alias.
    pub const CORPUS_IPC_ZMQ_READOUT_ENV: &str = "CORPUS_IPC_ZMQ_READOUT_IPC";
    const LEGACY_READOUT_ENV: &str = "SPIKENAUT_ZMQ_READOUT_IPC";

    pub struct ZmqStimulusSource {
        socket: Option<SafeSocket>,
        channels: usize,
        last_modulators: Option<Vec<f32>>,
        max_age: std::time::Duration,
    }

    impl Default for ZmqStimulusSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ZmqStimulusSource {
        pub fn new() -> Self {
            Self::with_channels(0)
        }

        /// Construct with the configured ingress width used for width checks.
        pub fn with_channels(ch: usize) -> Self {
            Self {
                socket: None,
                channels: ch,
                last_modulators: None,
                max_age: std::time::Duration::from_secs(1),
            }
        }

        /// Override the freshness window applied to typed `StimulusBatch` timestamps.
        pub fn with_max_age(mut self, max_age: std::time::Duration) -> Self {
            self.max_age = max_age;
            self
        }

        /// Connect the SUB socket to an explicit endpoint (tests / custom wiring).
        pub fn connect(&mut self, endpoint: &str) -> Result<()> {
            let context = ::zmq::Context::new();
            let socket = context
                .socket(::zmq::SUB)
                .map_err(|e| anyhow::anyhow!("failed to create ZMQ SUB socket: {e}"))?;
            socket
                .set_subscribe(b"")
                .map_err(|e| anyhow::anyhow!("ZMQ subscribe: {e}"))?;
            socket
                .set_rcvhwm(16)
                .map_err(|e| anyhow::anyhow!("ZMQ rcvhwm: {e}"))?;
            socket
                .connect(endpoint)
                .map_err(|e| anyhow::anyhow!("ZMQ connect to {endpoint}: {e}"))?;
            self.socket = Some(SafeSocket { socket });
            Ok(())
        }

        fn readout_endpoint() -> String {
            std::env::var(CORPUS_IPC_ZMQ_READOUT_ENV)
                .or_else(|_| std::env::var(LEGACY_READOUT_ENV))
                .unwrap_or_else(|_| "tcp://127.0.0.1:5555".to_string())
        }

        fn now_ns() -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        }

        /// Skip ingress this tick while retaining the last neuromodulator snapshot.
        ///
        /// `None` means there is no held snapshot, matching the `StimulusSource`
        /// contract (`Ok(None)` still advances the network on zeroed stimuli).
        fn skip_ingress(&self) -> Option<IngressPacket> {
            self.last_modulators.as_ref().map(|mods| IngressPacket {
                stimuli: Vec::new(),
                modulators: Some(mods.clone()),
                valid_mask: None,
                batch_id: None,
                timestamp_ns: None,
            })
        }
    }

    impl StimulusSource for ZmqStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            let socket = self
                .socket
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("ZMQ stimulus source not initialized"))?;
            match socket.socket.recv_bytes(::zmq::DONTWAIT) {
                Ok(buf) => {
                    let policy = IngressPolicy::new(self.channels, Some(self.max_age));
                    match accept_ipc_json(&buf, &policy, Self::now_ns()) {
                        Ok(mut packet) => {
                            if let Some(mods) = packet.modulators.as_ref() {
                                self.last_modulators = Some(mods.clone());
                            } else {
                                packet.modulators = self.last_modulators.clone();
                            }
                            Ok(Some(packet))
                        }
                        Err(e) => {
                            // Consumed-but-invalid payload (stale, Ping, width, ...):
                            // skip ingress this tick but still let the caller tick.
                            tracing::warn!("Rejected ingress frame: {e}");
                            Ok(self.skip_ingress())
                        }
                    }
                }
                Err(::zmq::Error::EAGAIN) => Ok(self.skip_ingress()),
                Err(e) => Err(anyhow::anyhow!("ZMQ recv failed: {e}")),
            }
        }

        fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
            if self.socket.is_some() {
                return Ok(());
            }
            self.connect(&Self::readout_endpoint())
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

            let msg = IpcMessage::Spikes(SpikeBatch {
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
        use crate::ingress::STIMULUS_SCHEMA;
        use corpus_ipc::{BatchMetadata, NeuromodulatorSnapshot, StimulusBatch};
        use std::collections::HashMap;
        use std::time::Duration;

        fn sample_frame(channels: usize, batch_id: u64) -> Vec<u8> {
            let mut values = vec![0.0; channels];
            let mut valid_mask = vec![true; channels];
            values[0] = 1.0;
            if channels > 1 {
                valid_mask[1] = false;
            }
            let mut custom = HashMap::new();
            custom.insert("schema".into(), STIMULUS_SCHEMA.to_string());
            let batch = StimulusBatch {
                session_id: Some("zmq-loopback".into()),
                batch_id,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
                values,
                valid_mask: Some(valid_mask),
                metadata: Some(BatchMetadata {
                    processing_latency_ns: None,
                    source: Some("thalamic-relay-fixture".into()),
                    custom,
                }),
            };
            serde_json::to_vec(&IpcMessage::Stimuli(batch)).expect("serialize")
        }

        #[test]
        fn typed_json_frame_round_trips_over_zmq() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let endpoint = format!("tcp://127.0.0.1:{port}");

            let context = ::zmq::Context::new();
            let publisher = context.socket(::zmq::PUB).unwrap();
            publisher.bind(&endpoint).unwrap();

            let mut source =
                ZmqStimulusSource::with_channels(4).with_max_age(Duration::from_secs(5));
            source.connect(&endpoint).unwrap();
            std::thread::sleep(Duration::from_millis(150));

            let frame = sample_frame(4, 100);
            publisher.send(&frame, 0).unwrap();

            let packet = (0..50)
                .find_map(|_| match source.next_ingress() {
                    Ok(Some(packet)) => Some(packet),
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    }
                    Err(err) => panic!("ingress error: {err}"),
                })
                .expect("ZMQ SUB should receive a typed StimulusBatch");

            assert_eq!(packet.batch_id, Some(100));
            assert_eq!(packet.valid_mask.as_ref().map(|m| m[1]), Some(false));
            assert!((packet.stimuli[0] - 1.0).abs() < f32::EPSILON);
        }

        fn bind_loopback(channels: usize) -> (::zmq::Socket, ZmqStimulusSource) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let endpoint = format!("tcp://127.0.0.1:{port}");

            let context = ::zmq::Context::new();
            let publisher = context.socket(::zmq::PUB).unwrap();
            publisher.bind(&endpoint).unwrap();

            let mut source =
                ZmqStimulusSource::with_channels(channels).with_max_age(Duration::from_secs(5));
            source.connect(&endpoint).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            (publisher, source)
        }

        fn poll_ingress(source: &mut ZmqStimulusSource) -> Result<Option<IngressPacket>> {
            let mut last = Ok(None);
            for _ in 0..50 {
                last = source.next_ingress();
                match &last {
                    Err(err) => panic!("ingress error: {err}"),
                    Ok(Some(_)) => return last,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            last
        }

        #[test]
        fn rejected_frame_does_not_hard_fail_ingress() {
            let (publisher, mut source) = bind_loopback(4);
            let ping = serde_json::to_vec(&IpcMessage::Ping).expect("serialize Ping");
            publisher.send(&ping, 0).unwrap();

            // Ping is consumed-but-invalid: must be Ok(None), not Err, so the
            // subsequent valid batch can still be accepted on the next tick.
            let ping_result = poll_ingress(&mut source);
            assert!(
                ping_result.is_ok(),
                "rejected Ping must not be a hard receive failure: {ping_result:?}"
            );
            assert!(
                matches!(ping_result, Ok(None)),
                "Ping has no held modulators, so skip-ingress is Ok(None): {ping_result:?}"
            );

            let frame = sample_frame(4, 101);
            publisher.send(&frame, 0).unwrap();
            let packet = poll_ingress(&mut source)
                .expect("recv")
                .expect("ZMQ SUB should receive a typed StimulusBatch after Ping");
            assert_eq!(packet.batch_id, Some(101));
        }

        #[test]
        fn idle_ticks_hold_last_modulators() {
            let (publisher, mut source) = bind_loopback(4);
            let snapshot = NeuromodulatorSnapshot {
                tick: 1,
                dopamine: 0.4,
                cortisol: 0.3,
                acetylcholine: 0.2,
                tempo: 1.0,
            };
            let frame =
                serde_json::to_vec(&IpcMessage::Neuromodulators(snapshot)).expect("serialize");
            publisher.send(&frame, 0).unwrap();

            let packet = poll_ingress(&mut source)
                .expect("recv")
                .expect("ZMQ SUB should receive Neuromodulators");
            assert_eq!(
                packet.modulators.as_deref(),
                Some(&[0.4, 0.3, 0.2, 1.0][..])
            );

            let idle = source
                .next_ingress()
                .expect("EAGAIN with held modulators must not be a hard failure");
            let idle = idle.expect("idle tick must carry last neuromodulator snapshot");
            assert!(idle.stimuli.is_empty());
            assert_eq!(idle.modulators.as_deref(), Some(&[0.4, 0.3, 0.2, 1.0][..]));
        }
    }
}

#[cfg(feature = "corpus-ipc")]
pub use zmq_impl::{ZmqSpikeSink, ZmqStimulusSource};
