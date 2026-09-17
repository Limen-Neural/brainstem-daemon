// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! End-to-end smoke: Thalamic fixture → corpus-ipc types → Brainstem runtime.
//!
//! Requires `--features corpus-ipc`. CPU-only; no GPU.

#[path = "fixtures/thalamic_producer.rs"]
mod thalamic;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode};
use brainstem_daemon::ingress::{IngressConfig, IngressPolicy, accept_ipc_json};
use brainstem_daemon::{BackendPair, CollectingSpikeSink, IngressPacket, StimulusSource};
use corpus_ipc::{IpcMessage, StimulusBatch};
use thalamic::{StimulusTransport, ThalamicProducer};

const CHANNELS: usize = 4;
const LIF: usize = 4;
const IZH: usize = 0;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        // Cargo sets CARGO_TARGET_TMPDIR for tests; fall back to this crate's
        // target/ so fixtures never use the shared system temp directory.
        let root = std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from,
        );
        let dir = root.join(format!("{prefix}-{}-{}", std::process::id(), now_ns()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct QueuedStimulusSource {
    packets: VecDeque<Result<IngressPacket, String>>,
}

impl StimulusSource for QueuedStimulusSource {
    fn next_ingress(&mut self) -> anyhow::Result<Option<IngressPacket>> {
        match self.packets.pop_front() {
            Some(Ok(packet)) => Ok(Some(packet)),
            Some(Err(err)) => Err(anyhow::anyhow!(err)),
            None => Ok(None),
        }
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
}

struct CaptureTransport {
    frames: Vec<Vec<u8>>,
}

impl StimulusTransport for CaptureTransport {
    fn send(&mut self, frame: &[u8]) -> Result<()> {
        self.frames.push(frame.to_vec());
        Ok(())
    }
}

fn smoke_sidecar_json() -> String {
    r#"{
        "source": "spikenaut_julia",
        "encoder": "v3_state_telemetry",
        "q88": "signed",
        "frozen_lineage": "thalamic-smoke",
        "neurons": [
            {
                "decay_rate": 0.85,
                "membrane_potential": 0.0,
                "threshold": 0.02,
                "last_spike": false,
                "weights": [2.0, 0.12, 0.13, 0.14]
            },
            {
                "decay_rate": 0.85,
                "membrane_potential": 0.0,
                "threshold": 0.02,
                "last_spike": false,
                "weights": [0.15, 0.16, 0.17, 0.18]
            },
            {
                "decay_rate": 0.85,
                "membrane_potential": 0.0,
                "threshold": 0.02,
                "last_spike": false,
                "weights": [0.19, 0.20, 0.21, 0.22]
            },
            {
                "decay_rate": 0.85,
                "membrane_potential": 0.0,
                "threshold": 0.02,
                "last_spike": false,
                "weights": [0.23, 0.24, 0.25, 0.26]
            }
        ]
    }"#
    .to_string()
}

fn write_smoke_sidecar(dir: &Path) -> PathBuf {
    let path = dir.join("snn_model.json");
    std::fs::write(&path, smoke_sidecar_json()).expect("write sidecar");
    path
}

fn smoke_config(model_path: PathBuf) -> DaemonConfig {
    DaemonConfig {
        tick_rate_hz: 1000,
        log_level: "info".into(),
        spine_sub_port: 5555,
        spine_pub_port: 5556,
        model_path,
        lif_count: LIF,
        izh_count: IZH,
        channels: CHANNELS,
        runtime_mode: RuntimeMode::Live,
        services: Vec::new(),
        ingress: IngressConfig::default(),
        control_bind: None,
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

#[test]
fn thalamic_fixture_has_no_spiking_network() {
    let src = include_str!("fixtures/thalamic_producer.rs");
    assert!(
        !src.contains("use neuromod"),
        "Thalamic fixture must not import neuromod"
    );
    assert!(
        !src.contains("SpikingNetwork::"),
        "Thalamic fixture must not construct a SpikingNetwork"
    );
}

#[test]
fn checkpoint_is_loaded_instead_of_blank_network() {
    let dir = TempDir::new("brainstem-smoke-ckpt");
    let path = write_smoke_sidecar(dir.path());

    let source = QueuedStimulusSource {
        packets: VecDeque::new(),
    };
    let sink = CollectingSpikeSink::new();
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    let stats = BrainstemDaemon::try_with_backend(smoke_config(path), pair)
        .unwrap()
        .run_for_ticks(1)
        .unwrap();

    let loaded = stats.loaded_checkpoint.expect("checkpoint must be loaded");
    assert_eq!(loaded.model_id, "spikenaut-snn:thalamic-smoke");
    assert_eq!(loaded.schema_id, "spikenaut-sidecar-json/v1");
}

#[test]
fn typed_frame_crosses_ipc_and_produces_runtime_result() {
    let dir = TempDir::new("brainstem-smoke-e2e");
    let path = write_smoke_sidecar(dir.path());

    let mut thalamic = ThalamicProducer::new();
    let batch = thalamic.simulate_telemetry(42, CHANNELS);
    let expected_mask = batch.valid_mask.clone();
    let bytes = thalamic.encode_frame(&batch).unwrap();
    let mut transport = CaptureTransport { frames: Vec::new() };
    thalamic.publish(Some(&mut transport), &bytes);
    assert_eq!(thalamic.published, 1);
    assert!(thalamic.safety_healthy);

    let policy = IngressPolicy::new(CHANNELS, Some(Duration::from_secs(1)));
    let packet = accept_ipc_json(&transport.frames[0], &policy, now_ns()).unwrap();
    assert_eq!(packet.batch_id, Some(42));
    assert_eq!(packet.valid_mask, expected_mask);
    assert_eq!(packet.session_id.as_deref(), Some("thalamic-smoke"));

    let source = QueuedStimulusSource {
        packets: VecDeque::from([Ok(packet)]),
    };
    let sink = CollectingSpikeSink::new();
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    let stats = BrainstemDaemon::try_with_backend(smoke_config(path), pair)
        .unwrap()
        .run_for_ticks(1)
        .unwrap();

    assert_eq!(stats.ticks, 1);
    assert_eq!(stats.accepted_batches, 1);
    assert_eq!(stats.last_batch_id, Some(42));
    assert_eq!(stats.last_valid_mask, expected_mask);
    assert!(stats.loaded_checkpoint.is_some());
}

#[test]
fn rejected_frame_is_counted_without_stopping_the_loop() {
    let dir = TempDir::new("brainstem-smoke-reject");
    let path = write_smoke_sidecar(dir.path());
    let source = QueuedStimulusSource {
        packets: VecDeque::from([Ok(IngressPacket {
            rejected: true,
            ..IngressPacket::default()
        })]),
    };
    let sink = CollectingSpikeSink::new();
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    let stats = BrainstemDaemon::try_with_backend(smoke_config(path), pair)
        .unwrap()
        .run_for_ticks(1)
        .unwrap();
    assert_eq!(stats.ticks, 1);
    assert_eq!(stats.rejected_batches, 1);
    assert_eq!(stats.accepted_batches, 0);
}

#[test]
fn validity_mask_survives_wire_contract() {
    let thalamic = ThalamicProducer::new();
    let batch = thalamic.simulate_telemetry(1, CHANNELS);
    let wire = thalamic.encode_frame(&batch).unwrap();
    let decoded: IpcMessage = serde_json::from_slice(&wire).unwrap();
    match decoded {
        IpcMessage::Stimuli(StimulusBatch {
            valid_mask, values, ..
        }) => {
            assert_eq!(valid_mask, batch.valid_mask);
            assert_eq!(values.len(), CHANNELS);
            assert!(!valid_mask.as_ref().unwrap()[1]);
        }
        other => panic!("expected Stimuli, got {other:?}"),
    }
}

#[test]
fn schema_incompatibility_fails_loudly() {
    let policy = IngressPolicy::new(CHANNELS, Some(Duration::from_secs(1)));

    let old_udp = br#"{"type":"Stimuli","values":[1.0,0.0,0.0,0.0]}"#;
    let err = accept_ipc_json(old_udp, &policy, now_ns()).unwrap_err();
    assert!(err.to_string().contains("deserialize"));

    let thalamic = ThalamicProducer::new();
    let mut batch = thalamic.simulate_telemetry(2, CHANNELS);
    batch
        .metadata
        .as_mut()
        .unwrap()
        .custom
        .insert("schema".into(), "not-a-supported-schema".into());
    let bytes = serde_json::to_vec(&IpcMessage::Stimuli(batch)).unwrap();
    let err = accept_ipc_json(&bytes, &policy, now_ns()).unwrap_err();
    assert!(err.to_string().contains("schema"));

    let ping = serde_json::to_vec(&IpcMessage::Ping).unwrap();
    let err = accept_ipc_json(&ping, &policy, now_ns()).unwrap_err();
    assert!(err.to_string().contains("Ping"));
}

#[test]
fn thalamic_stays_healthy_when_brainstem_unavailable() {
    let mut thalamic = ThalamicProducer::new();
    thalamic.safety_tick(true);
    let batch = thalamic.simulate_telemetry(9, CHANNELS);
    let bytes = thalamic.encode_frame(&batch).unwrap();

    // No transport: Brainstem is not running.
    thalamic.publish(None, &bytes);
    assert!(thalamic.safety_healthy);
    assert_eq!(thalamic.publish_errors, 1);
    assert_eq!(thalamic.published, 0);

    // A later successful-looking publish must not clobber a thermal fault.
    thalamic.safety_tick(false);
    let mut transport = CaptureTransport { frames: Vec::new() };
    thalamic.publish(Some(&mut transport), &bytes);
    assert!(!thalamic.safety_healthy);
    assert_eq!(thalamic.published, 1);

    thalamic.safety_tick(true);
    assert!(thalamic.safety_healthy);
    let _still_producing = thalamic.simulate_telemetry(10, CHANNELS);
}
