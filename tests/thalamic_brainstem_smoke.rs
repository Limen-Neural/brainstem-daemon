// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! End-to-end smoke: Thalamic fixture → corpus-ipc types → Brainstem runtime.
//!
//! Requires `--features corpus-ipc`. CPU-only; no GPU.

#[path = "fixtures/thalamic_producer.rs"]
mod thalamic;

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;
use brainstem_daemon::checkpoint::{NetworkDims, write_nonblank_checkpoint};
use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig};
use brainstem_daemon::ingress::{IngressPolicy, accept_ipc_json};
use brainstem_daemon::{BackendPair, CollectingSpikeSink, IngressPacket, StimulusSource};
use corpus_ipc::{IpcMessage, StimulusBatch};
use thalamic::{StimulusTransport, ThalamicProducer};

const CHANNELS: usize = 4;
const LIF: usize = 4;
const IZH: usize = 0;

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

fn smoke_dims() -> NetworkDims {
    NetworkDims {
        lif_count: LIF,
        izh_count: IZH,
        channels: CHANNELS,
    }
}

fn smoke_config(model_path: std::path::PathBuf) -> DaemonConfig {
    DaemonConfig {
        tick_rate_hz: 1000,
        log_level: "info".into(),
        spine_sub_port: 5555,
        spine_pub_port: 5556,
        model_path,
        lif_count: LIF,
        izh_count: IZH,
        channels: CHANNELS,
        services: Vec::new(),
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
    let dir = std::env::temp_dir().join(format!(
        "brainstem-smoke-ckpt-{}-{}",
        std::process::id(),
        now_ns()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("smoke.json");
    let identity = write_nonblank_checkpoint(&path, "smoke-thalamic-v1", smoke_dims()).unwrap();

    let source = QueuedStimulusSource {
        packets: VecDeque::new(),
    };
    let sink = CollectingSpikeSink::new();
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    let stats = BrainstemDaemon::try_with_backend(smoke_config(path.clone()), pair)
        .unwrap()
        .run_for_ticks(1)
        .unwrap();

    let loaded = stats.loaded_checkpoint.expect("checkpoint must be loaded");
    assert_eq!(loaded.model_id, "smoke-thalamic-v1");
    assert_eq!(loaded.fingerprint, identity.fingerprint);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn typed_frame_crosses_ipc_and_produces_runtime_result() {
    let dir = std::env::temp_dir().join(format!(
        "brainstem-smoke-e2e-{}-{}",
        std::process::id(),
        now_ns()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("smoke.json");
    write_nonblank_checkpoint(&path, "smoke-thalamic-v1", smoke_dims()).unwrap();

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
    let _ = std::fs::remove_dir_all(dir);
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

    // Safety evaluation continues after the failed publish.
    thalamic.safety_tick(true);
    assert!(thalamic.safety_healthy);
    let _still_producing = thalamic.simulate_telemetry(10, CHANNELS);
}
