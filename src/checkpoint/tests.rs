// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

use super::*;
use crate::daemon::DaemonConfig;
use crate::registry::ServiceConfig;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("checkpoint-fixtures")
        .join(format!(
            "t-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn live_config(model_path: PathBuf, lif: usize, channels: usize) -> DaemonConfig {
    DaemonConfig {
        tick_rate_hz: 1000,
        log_level: "info".to_string(),
        spine_sub_port: 5555,
        spine_pub_port: 5556,
        model_path,
        lif_count: lif,
        izh_count: 0,
        channels,
        services: vec![ServiceConfig::named("telemetry")],
        runtime_mode: RuntimeMode::Live,
    }
}

fn valid_sidecar_json() -> String {
    r#"{
            "source": "spikenaut_julia",
            "encoder": "v3_state_telemetry",
            "q88": "signed",
            "frozen_lineage": "test-fixture",
            "neurons": [
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.1,
                    "threshold": 1.0,
                    "last_spike": false,
                    "weights": [0.5, -0.25]
                },
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.0,
                    "threshold": 1.0,
                    "last_spike": true,
                    "weights": [0.1, 0.75]
                }
            ]
        }"#
    .to_string()
}

fn write_json(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write fixture");
    path
}

fn restore_failed(cfg: &DaemonConfig) -> anyhow::Error {
    match restore_network(cfg) {
        Ok(_) => panic!("expected restore_network to fail"),
        Err(err) => err,
    }
}

#[test]
fn live_loads_valid_sidecar_and_records_provenance() {
    let dir = unique_dir();
    let path = write_json(&dir, "snn_model.json", &valid_sidecar_json());
    let cfg = live_config(path.clone(), 2, 2);

    let (network, provenance) = restore_network(&cfg).expect("valid checkpoint");

    assert_eq!(network.neurons.len(), 2);
    assert_eq!(network.iz_neurons.len(), 0);
    assert_eq!(network.num_channels, 2);
    assert!((network.neurons[0].weights[0] - 0.5).abs() < 1e-6);
    assert!((network.neurons[1].weights[1] - 0.75).abs() < 1e-6);
    assert!(network.neurons[1].last_spike);
    assert_eq!(network.stdp_config.reward_lr, 0.0);
    assert_eq!(provenance.schema_id, SCHEMA_ID);
    assert_eq!(provenance.model_id, "spikenaut-snn:test-fixture");
    assert_eq!(provenance.source_path, path);
    assert_eq!(
        provenance.content_sha256,
        hex_sha256(valid_sidecar_json().as_bytes())
    );
    assert_eq!(provenance.encoder.as_deref(), Some("v3_state_telemetry"));
    assert_eq!(provenance.frozen_lineage.as_deref(), Some("test-fixture"));
}

#[test]
fn live_accepts_sidecar_output_weights_without_restoring_them() {
    let dir = unique_dir();
    let json = r#"{
            "source": "spikenaut_julia",
            "encoder": "v3_state_telemetry",
            "q88": "signed",
            "neurons": [
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.1,
                    "threshold": 1.0,
                    "last_spike": false,
                    "weights": [0.5, -0.25],
                    "output_weights": [0.2, 0.3]
                },
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.0,
                    "threshold": 1.0,
                    "last_spike": true,
                    "weights": [0.1, 0.75],
                    "output_weights": [0.4, 0.5]
                }
            ]
        }"#;
    let path = write_json(&dir, "snn_model.json", json);
    let cfg = live_config(path, 2, 2);
    let (network, _) = restore_network(&cfg).expect("output_weights are sidecar readout");
    assert!((network.neurons[0].weights[0] - 0.5).abs() < 1e-6);
}

#[test]
fn live_resolves_huggingface_directory_layout() {
    let root = unique_dir();
    let nested = root.join("dataset/merged_v2");
    fs::create_dir_all(&nested).expect("hf layout");
    write_json(&nested, "snn_model.json", &valid_sidecar_json());
    write_json(
        &root,
        "config.json",
        r#"{"model_type":"spikenaut-snn","n_neurons":2,"n_channels":2}"#,
    );

    let cfg = live_config(root.clone(), 2, 2);
    let (_, provenance) = restore_network(&cfg).expect("hf directory");
    assert_eq!(provenance.source_path, nested.join("snn_model.json"));
    assert_eq!(provenance.schema_id, SCHEMA_ID);
}

#[test]
fn live_rejects_missing_checkpoint() {
    let cfg = live_config(PathBuf::from("/no/such/snn_model.json"), 2, 2);
    let err = restore_failed(&cfg);
    let message = err.to_string();
    assert!(
        message.contains("checkpoint not found"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_mem_dump() {
    let dir = unique_dir();
    let path = write_json(&dir, "parameters_weights.mem", "0134\n000E\n");
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    let message = err.to_string();
    assert!(
        message.contains("Q8.8") && message.contains("snn_model.json"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_corrupt_json() {
    let dir = unique_dir();
    let path = write_json(&dir, "snn_model.json", "{not json");
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    let message = format!("{err:#}");
    assert!(
        message.contains("failed to parse Spikenaut sidecar JSON"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_dimension_mismatch() {
    let dir = unique_dir();
    let path = write_json(&dir, "snn_model.json", &valid_sidecar_json());
    let cfg = live_config(path, 16, 2);
    let err = restore_failed(&cfg);
    let message = err.to_string();
    assert!(message.contains("LIF count"), "unexpected error: {message}");
}

#[test]
fn live_rejects_channel_mismatch() {
    let dir = unique_dir();
    let path = write_json(&dir, "snn_model.json", &valid_sidecar_json());
    let mut cfg = live_config(path, 2, 16);
    cfg.channels = 16;
    let err = restore_failed(&cfg);
    let message = err.to_string();
    assert!(
        message.contains("ingress contract"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_non_finite_weight() {
    let dir = unique_dir();
    let json = valid_sidecar_json().replace("0.75", "1e400");
    let path = write_json(&dir, "snn_model.json", &json);
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    let message = format!("{err:#}");
    assert!(
        message.contains("finite") || message.contains("parse") || message.contains("out of range"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_weight_that_overflows_f32() {
    let dir = unique_dir();
    let json = valid_sidecar_json().replace("0.75", "1e40");
    let path = write_json(&dir, "snn_model.json", &json);
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    let message = format!("{err:#}");
    assert!(
        message.contains("overflows f32"),
        "unexpected error: {message}"
    );
}

#[test]
fn live_rejects_nan_after_parse() {
    let mut document: SidecarDocument =
        serde_json::from_str(&valid_sidecar_json()).expect("fixture parses");
    document.neurons[0].weights[0] = f64::NAN;
    let err = validate_finite_parameters(&document).expect_err("nan");
    assert!(err.to_string().contains("finite"));
}

#[test]
fn live_rejects_blank_weights() {
    let dir = unique_dir();
    let json = r#"{
            "source": "spikenaut_julia",
            "encoder": "v3_state_telemetry",
            "q88": "signed",
            "neurons": [
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.0,
                    "threshold": 1.0,
                    "last_spike": false,
                    "weights": [0.0, 0.0]
                },
                {
                    "decay_rate": 0.85,
                    "membrane_potential": 0.0,
                    "threshold": 1.0,
                    "last_spike": false,
                    "weights": [0.0, 0.0]
                }
            ]
        }"#;
    let path = write_json(&dir, "snn_model.json", json);
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    assert!(err.to_string().contains("blank"));
}

#[test]
fn live_rejects_incompatible_source() {
    let dir = unique_dir();
    let json = valid_sidecar_json().replace("spikenaut_julia", "other_trainer");
    let path = write_json(&dir, "snn_model.json", &json);
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    assert!(err.to_string().contains("incompatible"));
}

#[test]
fn live_rejects_missing_source() {
    let dir = unique_dir();
    let json = valid_sidecar_json().replace(r#""source": "spikenaut_julia","#, "");
    let path = write_json(&dir, "snn_model.json", &json);
    let cfg = live_config(path, 2, 2);
    let err = restore_failed(&cfg);
    assert!(err.to_string().contains("missing `source`"));
}

#[test]
fn live_rejects_nonzero_izh_count() {
    let dir = unique_dir();
    let path = write_json(&dir, "snn_model.json", &valid_sidecar_json());
    let mut cfg = live_config(path, 2, 2);
    cfg.izh_count = 5;
    let err = restore_failed(&cfg);
    assert!(err.to_string().contains("LIF-only"));
}

#[test]
fn live_rejects_hub_model_type_mismatch() {
    let root = unique_dir();
    write_json(&root, "snn_model.json", &valid_sidecar_json());
    write_json(
        &root,
        "config.json",
        r#"{"model_type":"not-spikenaut","n_neurons":2,"n_channels":2}"#,
    );
    let cfg = live_config(root, 2, 2);
    let err = restore_failed(&cfg);
    assert!(err.to_string().contains("model_type"));
}

#[test]
fn simulation_uses_blank_with_dimensions_and_does_not_claim_spikenaut() {
    let cfg = DaemonConfig {
        tick_rate_hz: 1000,
        log_level: "info".to_string(),
        spine_sub_port: 5555,
        spine_pub_port: 5556,
        model_path: PathBuf::from("/no/such/model.json"),
        lif_count: 4,
        izh_count: 1,
        channels: 8,
        services: Vec::new(),
        runtime_mode: RuntimeMode::Simulation,
    };
    let (network, provenance) = restore_network(&cfg).expect("simulation");
    assert_eq!(network.neurons.len(), 4);
    assert_eq!(network.iz_neurons.len(), 1);
    assert_eq!(network.num_channels, 8);
    assert!(
        network
            .neurons
            .iter()
            .all(|n| n.weights.iter().all(|w| *w == 0.0))
    );
    assert_eq!(provenance.schema_id, SIMULATION_SCHEMA_ID);
    assert_eq!(provenance.model_id, "simulation/blank");
    assert_eq!(provenance.content_sha256, "none");
    assert_eq!(
        network.stdp_config.reward_lr,
        neuromod::RmStdpConfig::default().reward_lr
    );
}

#[test]
fn restore_rejects_neuron_count_overflow_before_allocation() {
    let mut cfg = live_config(PathBuf::from("/no/such/snn_model.json"), 2, 2);
    cfg.runtime_mode = RuntimeMode::Simulation;
    cfg.lif_count = usize::MAX;
    cfg.izh_count = 1;
    let err = restore_failed(&cfg);
    assert!(
        err.to_string().contains("overflows usize"),
        "unexpected error: {err}"
    );
}

#[test]
fn live_does_not_silently_replace_failed_load_with_blank_state() {
    let cfg = live_config(PathBuf::from("/no/such/snn_model.json"), 2, 2);
    assert!(restore_network(&cfg).is_err());
}
