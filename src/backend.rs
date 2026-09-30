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
    /// Per-channel validity mask copied from a typed `StimulusBatch` when present.
    /// `false` means the corresponding stimulus is a placeholder, not a real zero.
    pub valid_mask: Option<Vec<bool>>,
    /// Optional typed batch id from `corpus-ipc` stimulus ingress.
    pub batch_id: Option<u64>,
    /// Optional stimulus timestamp in nanoseconds from `corpus-ipc`.
    pub timestamp_ns: Option<u64>,
    /// Optional producer session from `StimulusBatch.session_id`.
    pub session_id: Option<String>,
    /// True when the source consumed an invalid/incompatible frame this tick.
    /// The tick loop still advances; `RuntimeStats.rejected_batches` counts these.
    pub rejected: bool,
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
        Ok(Some(IngressPacket::default()))
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

/// Collecting sink for tests and the Thalamic integration smoke harness.
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
    /// Cap on modulator-only frames drained in one tick so a flood cannot stall 1 kHz.
    const MAX_DRAIN_PER_TICK: usize = 32;

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
        /// Unspecified-width constructor: typed `StimulusBatch` width is not checked.
        /// Prefer [`Self::with_channels`] when the daemon config width is known.
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

        /// Cache `mods` as the held modulator vector, replayed on idle ticks.
        ///
        /// Returns `false` (and caches nothing) when any value is non-finite
        /// (NaN or ±Inf). This is the belt-and-suspenders guard required by
        /// issue #66: modulators must be validated before any held/cached state
        /// is updated, so a bad vector can never be replayed. The typed wire
        /// path already rejects non-finite modulators at deserialization, but
        /// this guard also protects any future non-typed producer.
        fn hold_modulators(&mut self, mods: &[f32]) -> bool {
            if mods.iter().any(|v| !v.is_finite()) {
                return false;
            }
            let dst = self.last_modulators.get_or_insert_with(Vec::new);
            dst.clear();
            dst.extend_from_slice(mods);
            true
        }

        fn skip_ingress(&self, rejected: bool) -> Option<IngressPacket> {
            if !rejected && self.last_modulators.is_none() {
                return None;
            }
            let mut packet = IngressPacket {
                rejected,
                ..IngressPacket::default()
            };
            packet.modulators.clone_from(&self.last_modulators);
            Some(packet)
        }

        fn attach_held_modulators(&self, packet: &mut IngressPacket) {
            if packet.modulators.is_none() {
                packet.modulators.clone_from(&self.last_modulators);
            }
        }

        /// Cache `mods` for idle replay, or reject the frame when they are
        /// non-finite (issue #66: never cache or replay non-finite modulators).
        /// Returns `Some(skip_packet)` for the caller to return on rejection, or
        /// `None` when the modulators were held successfully. Shared by both
        /// held-modulator branches in `next_ingress` so the warn message and
        /// `skip_ingress(true)` reject path stay identical.
        fn hold_or_reject(&mut self, mods: &[f32]) -> Option<Option<IngressPacket>> {
            if self.hold_modulators(mods) {
                return None;
            }
            tracing::warn!("Rejected ingress frame: non-finite modulator value");
            Some(self.skip_ingress(true))
        }
    }

    impl StimulusSource for ZmqStimulusSource {
        fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
            let policy = IngressPolicy::new(self.channels, Some(self.max_age));

            for _ in 0..MAX_DRAIN_PER_TICK {
                let recvd = self
                    .socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("ZMQ stimulus source not initialized"))?
                    .socket
                    .recv_bytes(::zmq::DONTWAIT);
                match recvd {
                    Ok(buf) => match accept_ipc_json(&buf, &policy, Self::now_ns()) {
                        Ok(packet) if packet.stimuli.is_empty() && packet.modulators.is_some() => {
                            if let Some(mods) = packet.modulators.as_ref()
                                && let Some(skip) = self.hold_or_reject(mods)
                            {
                                return Ok(skip);
                            }
                            // Modulation-only: keep draining so a Stimuli frame
                            // in the same tick is not delayed by one period.
                            continue;
                        }
                        Ok(mut packet) => {
                            match packet.modulators.as_ref() {
                                Some(mods) => {
                                    if let Some(skip) = self.hold_or_reject(mods) {
                                        return Ok(skip);
                                    }
                                }
                                None => self.attach_held_modulators(&mut packet),
                            }
                            return Ok(Some(packet));
                        }
                        Err(e) => {
                            tracing::warn!("Rejected ingress frame: {e}");
                            // Consumed-but-invalid: stop draining this tick so a
                            // later valid frame is still available next period.
                            return Ok(self.skip_ingress(true));
                        }
                    },
                    Err(::zmq::Error::EAGAIN) => {
                        return Ok(self.skip_ingress(false));
                    }
                    Err(e) => return Err(anyhow::anyhow!("ZMQ recv failed: {e}")),
                }
            }

            Ok(self.skip_ingress(false))
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

    /// Application-side send counters observed by [`ZmqSpikeSink`].
    ///
    /// These count only what the publisher itself can observe at the moment it
    /// hands a batch to (or withholds it from) ZeroMQ. They do NOT and cannot
    /// measure subscriber-side PUB loss (see [`ZmqSpikeSink`] docs).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct SinkSendStats {
        /// `send()` was actually called on the socket (a frame was handed to ZeroMQ).
        pub attempted: u64,
        /// An empty batch was withheld because the empty-batch policy suppresses empties.
        pub suppressed: u64,
        /// `send()` returned an error.
        pub failed: u64,
    }

    /// ZeroMQ PUB spike egress.
    ///
    /// # Loss semantics (read carefully)
    ///
    /// This sink accounts for exactly three **application-side** outcomes that
    /// the publisher can directly observe, exposed via [`Self::stats`]:
    ///
    /// - `attempted`: `send()` was called (a frame was handed to ZeroMQ);
    /// - `suppressed`: an empty batch was withheld under the empty-batch policy
    ///   (`send_empty_batches == false`), so no frame was handed to ZeroMQ;
    /// - `failed`: `send()` returned an error.
    ///
    /// It does **not** measure subscriber-side delivery. ZeroMQ PUB is
    /// best-effort: it silently drops messages for subscribers that are slow,
    /// absent, or over the send high-water mark (SNDHWM). That subscriber-side
    /// loss is **inherently not observable by the publisher** — an `attempted`
    /// send that returns `Ok` says the frame was accepted into ZeroMQ's egress,
    /// not that any subscriber received it. Do not read these counters as a
    /// delivery guarantee or a measure of dropped-at-subscriber messages.
    pub struct ZmqSpikeSink {
        socket: std::sync::Mutex<SafeSocket>,
        /// Reusable buffer to convert to corpus-ipc event type without allocating every tick.
        corpus_buf: Vec<CorpusSpikeEvent>,
        /// When `false`, empty spike batches are suppressed (not sent) and
        /// counted as `suppressed`. Defaults to `true` (send empty batches),
        /// preserving the historical always-send behavior.
        send_empty_batches: bool,
        /// Application-side send accounting (see [`SinkSendStats`]).
        stats: SinkSendStats,
    }

    impl ZmqSpikeSink {
        /// Construct a sink that always sends, including empty batches.
        ///
        /// Backward-compatible with existing callers/tests that relied on the
        /// original always-send behavior.
        pub fn new(socket: ::zmq::Socket) -> Self {
            Self::with_policy(socket, true)
        }

        /// Construct a sink with an explicit empty-batch policy.
        ///
        /// When `send_empty_batches` is `false`, `emit(&[])` is suppressed (no
        /// frame is sent) and counted as `suppressed`.
        pub fn with_policy(socket: ::zmq::Socket, send_empty_batches: bool) -> Self {
            Self {
                socket: std::sync::Mutex::new(SafeSocket { socket }),
                corpus_buf: Vec::new(),
                send_empty_batches,
                stats: SinkSendStats::default(),
            }
        }

        /// Snapshot of the application-side send counters.
        ///
        /// See the type-level docs: these reflect only attempted / suppressed /
        /// failed sends the publisher can observe, never subscriber-side PUB loss.
        pub fn stats(&self) -> SinkSendStats {
            self.stats
        }
    }

    impl SpikeSink for ZmqSpikeSink {
        fn emit(&mut self, spikes: &[SpikeEvent], batch_time: std::time::Duration) -> Result<()> {
            // Documented empty-batch suppression policy: when configured to
            // suppress empties, an empty batch is not handed to ZeroMQ. This is
            // an application-side decision the publisher fully observes.
            if spikes.is_empty() && !self.send_empty_batches {
                self.stats.suppressed += 1;
                return Ok(());
            }

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

            // Unversioned tagged JSON (`{"Spikes":{...}}`) matches the pre-envelope
            // encoding still accepted by corpus-ipc 0.1. The envelope encoder
            // (`encode_ipc_message_json`) is a wire-format change for existing PUB
            // consumers, so it is not used here.
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
            // `send()` returning Ok means the frame was accepted into ZeroMQ's
            // egress, NOT that any subscriber received it. Subscriber-side drops
            // (slow/absent/over-SNDHWM) are silent and unobservable here.
            self.stats.attempted += 1;
            if let Err(e) = guard.socket.send(payload, 0) {
                self.stats.failed += 1;
                // Preserve existing behavior: propagate the error so run_tick's
                // emit_errors path and fatal handling still apply.
                return Err(anyhow::anyhow!("ZMQ PUB send failed: {e}"));
            }
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

        fn bind_loopback(channels: usize) -> (::zmq::Socket, ZmqStimulusSource) {
            let context = ::zmq::Context::new();
            let publisher = context.socket(::zmq::PUB).unwrap();
            publisher.bind("tcp://127.0.0.1:*").unwrap();
            let endpoint = match publisher.get_last_endpoint().unwrap() {
                Ok(ep) => ep,
                Err(bytes) => String::from_utf8(bytes).expect("endpoint utf8"),
            };

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
                    Ok(Some(packet)) if packet.rejected => {
                        return last;
                    }
                    Ok(Some(_)) => return last,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            last
        }

        #[test]
        fn typed_json_frame_round_trips_over_zmq() {
            let (publisher, mut source) = bind_loopback(4);
            let frame = sample_frame(4, 100);
            publisher.send(&frame, 0).unwrap();

            let packet = poll_ingress(&mut source)
                .expect("recv")
                .expect("ZMQ SUB should receive a typed StimulusBatch");
            assert!(!packet.rejected);
            assert_eq!(packet.batch_id, Some(100));
            assert_eq!(packet.valid_mask.as_ref().map(|m| m[1]), Some(false));
            assert!((packet.stimuli[0] - 1.0).abs() < f32::EPSILON);
            assert_eq!(packet.session_id.as_deref(), Some("zmq-loopback"));
        }

        #[test]
        fn rejected_frame_does_not_hard_fail_ingress() {
            let (publisher, mut source) = bind_loopback(4);
            let ping = serde_json::to_vec(&IpcMessage::Ping).expect("serialize Ping");
            publisher.send(&ping, 0).unwrap();

            let ping_result = poll_ingress(&mut source);
            assert!(
                ping_result.is_ok(),
                "rejected Ping must not be a hard receive failure: {ping_result:?}"
            );
            let ping_packet = ping_result.expect("ok").expect("rejection is signaled");
            assert!(
                ping_packet.rejected,
                "Ping must set rejected so RuntimeStats can count it"
            );
            assert!(ping_packet.stimuli.is_empty());

            let frame = sample_frame(4, 101);
            publisher.send(&frame, 0).unwrap();
            let packet = (0..50)
                .find_map(|_| match source.next_ingress() {
                    Ok(Some(packet)) if !packet.rejected => Some(packet),
                    Ok(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    }
                    Err(err) => panic!("ingress error: {err}"),
                })
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

            // Modulation-only frames are drained; after EAGAIN the held snapshot
            // is returned so idle ticks keep DA/cortisol/ACh/tempo.
            let packet = (0..50)
                .find_map(|_| match source.next_ingress() {
                    Ok(Some(packet)) if packet.modulators.is_some() => Some(packet),
                    Ok(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    }
                    Err(err) => panic!("ingress error: {err}"),
                })
                .expect("ZMQ SUB should hold Neuromodulators after drain");
            assert_eq!(
                packet.modulators.as_deref(),
                Some(&[0.4, 0.3, 0.2, 1.0][..])
            );
            assert!(packet.stimuli.is_empty());

            let idle = source
                .next_ingress()
                .expect("EAGAIN with held modulators must not be a hard failure");
            let idle = idle.expect("idle tick must carry last neuromodulator snapshot");
            assert!(idle.stimuli.is_empty());
            assert!(!idle.rejected);
            assert_eq!(idle.modulators.as_deref(), Some(&[0.4, 0.3, 0.2, 1.0][..]));
        }

        #[test]
        fn non_finite_modulators_are_never_held_or_replayed() {
            // Invariant (issue #66): a non-finite modulator frame must never be
            // cached into `last_modulators` and replayed on idle ticks.
            //
            // Layer that rejects it here: the corpus-ipc typed DESERIALIZE layer.
            // A `NeuromodulatorSnapshot` with a non-finite field cannot even be
            // built into a wire frame with valid JSON — `serde_json` encodes NaN
            // as `null`, and `NeuromodulatorSnapshot`'s validating deserialize
            // (`check_range` -> `check_finite`) rejects it. So `accept_ipc_json`
            // returns an error and `next_ingress` takes the invalid-frame arm
            // (`warn!("Rejected ingress frame: ..")` + `skip_ingress(true)`)
            // before `hold_modulators` is ever called. The `hold_modulators`
            // finite guard is the belt-and-suspenders backstop for any future
            // non-typed path.
            let (publisher, mut source) = bind_loopback(4);
            let snapshot = NeuromodulatorSnapshot {
                tick: 1,
                dopamine: f32::NAN,
                cortisol: 0.3,
                acetylcholine: 0.2,
                tempo: 1.0,
            };
            let frame =
                serde_json::to_vec(&IpcMessage::Neuromodulators(snapshot)).expect("serialize");
            publisher.send(&frame, 0).unwrap();

            // The frame is rejected (skipped), never held. Because nothing is
            // cached, subsequent ticks return `None` (no held modulators, no
            // stimuli), and no idle tick ever replays the bad vector.
            let mut saw_rejected = false;
            for _ in 0..50 {
                match source.next_ingress().expect("ingress must not hard-fail") {
                    Some(packet) => {
                        // A rejected/skip packet may carry the (still None) held
                        // modulators, but must never carry the NaN vector.
                        assert!(
                            packet.modulators.is_none(),
                            "non-finite modulators must never be held/replayed, got {:?}",
                            packet.modulators
                        );
                        if packet.rejected {
                            saw_rejected = true;
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            assert!(
                saw_rejected,
                "the non-finite modulator frame must be surfaced as a rejected/skip outcome"
            );
        }

        // ─────────────────────────────────────────────────────────────────
        // PUB / ZmqSpikeSink egress tests (issue #68 / LIM-1320).
        //
        // These mirror the SUB-side `bind_loopback`/`get_last_endpoint`
        // pattern but for the sink side: the sink owns a PUB socket bound on
        // `tcp://127.0.0.1:*`, and a SUB receiver connects to that endpoint.

        /// Build a `ZmqSpikeSink` over a PUB socket bound on loopback with the
        /// given empty-batch policy, and return the sink plus the resolved
        /// endpoint so tests can attach a subscriber.
        fn bind_sink(send_empty_batches: bool) -> (ZmqSpikeSink, String) {
            let context = ::zmq::Context::new();
            let pub_socket = context.socket(::zmq::PUB).unwrap();
            // Apply the same bounded-lifecycle options the binary sets.
            pub_socket.set_sndhwm(1000).unwrap();
            pub_socket.set_linger(0).unwrap();
            pub_socket.bind("tcp://127.0.0.1:*").unwrap();
            let endpoint = match pub_socket.get_last_endpoint().unwrap() {
                Ok(ep) => ep,
                Err(bytes) => String::from_utf8(bytes).expect("endpoint utf8"),
            };
            let sink = ZmqSpikeSink::with_policy(pub_socket, send_empty_batches);
            (sink, endpoint)
        }

        fn connect_subscriber(endpoint: &str) -> ::zmq::Socket {
            let context = ::zmq::Context::new();
            let sub = context.socket(::zmq::SUB).unwrap();
            sub.set_subscribe(b"").unwrap();
            sub.set_rcvhwm(16).unwrap();
            sub.connect(endpoint).unwrap();
            // Allow the PUB/SUB connection to settle before publishing.
            std::thread::sleep(Duration::from_millis(150));
            sub
        }

        fn sample_spikes() -> Vec<SpikeEvent> {
            vec![
                SpikeEvent {
                    channel: 0,
                    time: 7,
                    strength: 1.0,
                },
                SpikeEvent {
                    channel: 3,
                    time: 9,
                    strength: 0.5,
                },
            ]
        }

        /// Poll a SUB socket for one frame, tolerating startup slop.
        fn recv_frame(sub: &::zmq::Socket) -> Option<Vec<u8>> {
            for _ in 0..50 {
                match sub.recv_bytes(::zmq::DONTWAIT) {
                    Ok(buf) => return Some(buf),
                    Err(::zmq::Error::EAGAIN) => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("SUB recv failed: {e}"),
                }
            }
            None
        }

        #[test]
        fn no_subscriber_emit_is_ok_and_counts_attempted() {
            // With no connected subscriber, PUB does not error; the frame is
            // silently dropped by ZeroMQ. That drop is NOT observable here — we
            // can only assert the application-side `attempted` count.
            let (mut sink, _endpoint) = bind_sink(true);
            for _ in 0..3 {
                sink.emit(&sample_spikes(), Duration::from_millis(1))
                    .expect("emit with no subscriber must be Ok (loss is silent/unobservable)");
            }
            let stats = sink.stats();
            assert_eq!(stats.attempted, 3);
            assert_eq!(stats.failed, 0);
            assert_eq!(stats.suppressed, 0);
        }

        #[test]
        fn empty_batch_policy_sends_when_enabled() {
            let (mut sink, endpoint) = bind_sink(true);
            let sub = connect_subscriber(&endpoint);

            sink.emit(&[], Duration::from_millis(2))
                .expect("empty emit must be Ok when send_empty_batches=true");
            let stats = sink.stats();
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.suppressed, 0);

            let buf = recv_frame(&sub).expect("subscriber must receive the empty-batch frame");
            let msg: IpcMessage = serde_json::from_slice(&buf).expect("deserialize IpcMessage");
            match msg {
                IpcMessage::Spikes(batch) => assert!(batch.spikes.is_empty()),
                other => panic!("expected Spikes, got {other:?}"),
            }
        }

        #[test]
        fn empty_batch_policy_suppresses_when_disabled() {
            let (mut sink, endpoint) = bind_sink(false);
            let sub = connect_subscriber(&endpoint);

            sink.emit(&[], Duration::from_millis(2))
                .expect("suppressed empty emit still returns Ok");
            let stats = sink.stats();
            assert_eq!(stats.suppressed, 1);
            assert_eq!(stats.attempted, 0);
            assert_eq!(stats.failed, 0);

            assert!(
                recv_frame(&sub).is_none(),
                "no frame must be delivered when the empty batch is suppressed"
            );

            // A non-empty batch still sends under the suppress-empties policy.
            sink.emit(&sample_spikes(), Duration::from_millis(3))
                .expect("non-empty emit must send under suppress-empties policy");
            assert_eq!(sink.stats().attempted, 1);
            assert!(
                recv_frame(&sub).is_some(),
                "the non-empty batch must still be delivered"
            );
        }

        #[test]
        fn slow_or_disconnected_subscriber_drops_are_not_observable() {
            // Connect a subscriber, then drop it, then keep emitting. The
            // publisher cannot observe the subscriber-side drops: every emit
            // still returns Ok and only `attempted` advances.
            let (mut sink, endpoint) = bind_sink(true);
            let sub = connect_subscriber(&endpoint);
            drop(sub);

            for _ in 0..5 {
                sink.emit(&sample_spikes(), Duration::from_millis(1))
                    .expect("emit to a disconnected subscriber must remain Ok");
            }
            let stats = sink.stats();
            assert_eq!(
                stats.attempted, 5,
                "only application-side attempts are counted"
            );
            assert_eq!(stats.failed, 0);
            assert_eq!(stats.suppressed, 0);
        }

        #[test]
        fn sink_teardown_is_bounded_by_finite_linger() {
            // With a finite LINGER (0), constructing, emitting with no
            // subscriber, then dropping the sink must complete promptly and not
            // hang on pending PUB messages. This is the observable, testable
            // proxy for signal-driven teardown not blocking on egress.
            let (mut sink, _endpoint) = bind_sink(true);
            for _ in 0..10 {
                sink.emit(&sample_spikes(), Duration::from_millis(1))
                    .expect("emit before teardown");
            }
            let start = std::time::Instant::now();
            drop(sink);
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "finite LINGER must bound sink teardown; took {:?}",
                start.elapsed()
            );
        }

        #[test]
        fn wire_payload_is_preserved_round_trip() {
            // Prove the unversioned tagged-JSON payload is unchanged: emit one
            // non-empty batch and deserialize the received bytes back into
            // corpus_ipc::IpcMessage, asserting Spikes with the expected
            // batch_id/timestamp derived from batch_time and the same spikes.
            let (mut sink, endpoint) = bind_sink(true);
            let sub = connect_subscriber(&endpoint);

            let batch_time = Duration::from_millis(1234);
            let spikes = sample_spikes();
            sink.emit(&spikes, batch_time)
                .expect("emit non-empty batch");

            let buf = recv_frame(&sub).expect("subscriber must receive the frame");
            let msg: IpcMessage = serde_json::from_slice(&buf).expect("deserialize IpcMessage");
            match msg {
                IpcMessage::Spikes(batch) => {
                    assert_eq!(batch.session_id, None);
                    assert_eq!(batch.batch_id, batch_time.as_millis() as u64);
                    assert_eq!(batch.timestamp, batch_time.as_nanos() as u64);
                    assert!(batch.metadata.is_none());
                    assert_eq!(batch.spikes.len(), spikes.len());
                    for (got, want) in batch.spikes.iter().zip(spikes.iter()) {
                        assert_eq!(got.channel, want.channel);
                        assert_eq!(got.time, want.time);
                        assert!((got.strength - want.strength).abs() < f32::EPSILON);
                    }
                }
                other => panic!("expected Spikes, got {other:?}"),
            }
        }
    }
}

#[cfg(feature = "corpus-ipc")]
pub use zmq_impl::{SinkSendStats, ZmqSpikeSink, ZmqStimulusSource};
