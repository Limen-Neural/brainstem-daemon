// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Spikenaut checkpoint loader and the live-mode startup gate.
//!
//! Canonical software artifact: Distill sidecar JSON (`snn_model.json`) as
//! published by Hugging Face `rmems/Spikenaut-SNN` (`dataset/merged_v2/`).
//! FPGA Q8.8 `.mem` dumps are not a software checkpoint.
//!
//! Live mode restores this document into `neuromod::SpikingNetwork` and
//! refuses to start on unreadable, incompatible, non-finite, or blank
//! state. Simulation mode is the only path that constructs a blank
//! `with_dimensions()` network.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use neuromod::SpikingNetwork;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::daemon::{DaemonConfig, RuntimeMode};

/// Schema identity recorded in logs for Distill sidecar JSON.
pub const SCHEMA_ID: &str = "spikenaut-sidecar-json/v1";

/// Schema identity recorded when `runtime_mode = "simulation"`.
pub const SIMULATION_SCHEMA_ID: &str = "simulation/blank";

/// Hugging Face `config.json` `model_type` for Spikenaut-SNN.
pub const HUB_MODEL_TYPE: &str = "spikenaut-snn";

/// Allowed Distill `source` values for live checkpoints.
const ALLOWED_SOURCES: &[&str] = &["spikenaut_julia"];

/// Allowed Distill `q88` values.
const ALLOWED_Q88: &[&str] = &["signed"];

/// Live mode rejects a weight matrix whose largest absolute value is at or
/// below this threshold (blank / uninitialized / `with_dimensions()` zeros).
const BLANK_WEIGHT_ABS_MAX: f32 = 1.0e-6;

/// Provenance of the network that is about to tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProvenance {
    /// Stable schema / format identifier.
    pub schema_id: String,
    /// Resolved filesystem path of the sidecar JSON, or a sentinel in
    /// simulation mode.
    pub source_path: PathBuf,
    /// SHA-256 (hex) of the sidecar file bytes. `"none"` in simulation mode.
    pub content_sha256: String,
    /// Human-readable model identity (lineage / source / encoder).
    pub model_id: String,
    /// Distill encoder pin, when present.
    pub encoder: Option<String>,
    /// Distill producer tag, when present.
    pub source: Option<String>,
    /// Distill frozen lineage, when present.
    pub frozen_lineage: Option<String>,
}

/// Restore the runtime network for `config`, or fail closed.
///
/// Live mode never falls back to a blank `with_dimensions()` network.
pub fn restore_network(config: &DaemonConfig) -> Result<(SpikingNetwork, ModelProvenance)> {
    match config.runtime_mode {
        RuntimeMode::Simulation => Ok(blank_simulation_network(config)),
        RuntimeMode::Live => load_live_checkpoint(config),
    }
}

fn blank_simulation_network(config: &DaemonConfig) -> (SpikingNetwork, ModelProvenance) {
    let network =
        SpikingNetwork::with_dimensions(config.lif_count, config.izh_count, config.channels);
    let provenance = ModelProvenance {
        schema_id: SIMULATION_SCHEMA_ID.to_string(),
        source_path: PathBuf::from("<simulation>"),
        content_sha256: "none".to_string(),
        model_id: "simulation/blank".to_string(),
        encoder: None,
        source: None,
        frozen_lineage: None,
    };
    (network, provenance)
}

fn load_live_checkpoint(config: &DaemonConfig) -> Result<(SpikingNetwork, ModelProvenance)> {
    if config.izh_count != 0 {
        bail!(
            "live Spikenaut checkpoints are LIF-only; izh_count must be 0, got {}",
            config.izh_count
        );
    }

    let sidecar_path = resolve_sidecar_path(&config.model_path)?;
    let bytes = fs::read(&sidecar_path)
        .with_context(|| format!("failed to read checkpoint {}", sidecar_path.display()))?;
    let content_sha256 = hex_sha256(&bytes);

    let text = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "checkpoint {} is not valid UTF-8 (live mode cannot load a binary/.mem dump as a software model)",
            sidecar_path.display()
        )
    })?;

    let document: SidecarDocument = serde_json::from_str(text).with_context(|| {
        format!(
            "failed to parse Spikenaut sidecar JSON from {}",
            sidecar_path.display()
        )
    })?;

    validate_schema(&document)?;
    if let Some(hub_path) = find_hub_config(&sidecar_path) {
        validate_hub_config(&hub_path, &document)?;
    }

    let channels = weight_width(&document)?;
    validate_ingress_contract(config, document.neurons.len(), channels)?;
    validate_finite_parameters(&document)?;
    validate_nonblank_weights(&document)?;

    let mut network =
        SpikingNetwork::with_dimensions(config.lif_count, config.izh_count, config.channels);
    apply_sidecar(&mut network, &document)?;

    let provenance = ModelProvenance {
        schema_id: SCHEMA_ID.to_string(),
        source_path: sidecar_path.clone(),
        content_sha256,
        model_id: model_id(&document, &sidecar_path),
        encoder: document.encoder.clone(),
        source: document.source.clone(),
        frozen_lineage: document.frozen_lineage.clone(),
    };
    Ok((network, provenance))
}

/// Resolve `model_path` to a sidecar JSON file.
///
/// Accepts a JSON file, a Hugging Face hub `config.json`, or a directory
/// containing `snn_model.json` or `dataset/merged_v2/snn_model.json`.
pub fn resolve_sidecar_path(model_path: &Path) -> Result<PathBuf> {
    if !model_path.exists() {
        bail!(
            "live mode cannot enter the tick loop: checkpoint not found at {}",
            model_path.display()
        );
    }

    if model_path.is_dir() {
        let direct = model_path.join("snn_model.json");
        if direct.is_file() {
            return Ok(direct);
        }
        let nested = model_path.join("dataset/merged_v2/snn_model.json");
        if nested.is_file() {
            return Ok(nested);
        }
        bail!(
            "checkpoint directory {} has no snn_model.json or dataset/merged_v2/snn_model.json",
            model_path.display()
        );
    }

    let ext = model_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if ext.eq_ignore_ascii_case("mem") {
        bail!(
            "live mode cannot load FPGA Q8.8 .mem dumps as a software checkpoint; \
             point model_path at Distill sidecar snn_model.json (Hugging Face rmems/Spikenaut-SNN)"
        );
    }

    if model_path
        .file_name()
        .is_some_and(|name| name == "config.json")
    {
        let root = model_path.parent().unwrap_or(model_path);
        let nested = root.join("dataset/merged_v2/snn_model.json");
        if nested.is_file() {
            return Ok(nested);
        }
        let sibling = root.join("snn_model.json");
        if sibling.is_file() {
            return Ok(sibling);
        }
        bail!(
            "Hugging Face config.json at {} has no sibling snn_model.json or dataset/merged_v2/snn_model.json",
            model_path.display()
        );
    }

    Ok(model_path.to_path_buf())
}

#[derive(Debug, Deserialize)]
struct SidecarDocument {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    encoder: Option<String>,
    #[serde(default)]
    q88: Option<String>,
    #[serde(default)]
    frozen_lineage: Option<String>,
    neurons: Vec<SidecarNeuron>,
}

#[derive(Debug, Deserialize)]
struct SidecarNeuron {
    decay_rate: f64,
    membrane_potential: f64,
    threshold: f64,
    last_spike: bool,
    weights: Vec<f64>,
    #[serde(default)]
    output_weights: Option<Vec<f64>>,
}

#[derive(Debug, Deserialize)]
struct HubConfig {
    #[serde(default)]
    model_type: Option<String>,
    #[serde(default)]
    n_neurons: Option<usize>,
    #[serde(default)]
    n_channels: Option<usize>,
}

fn validate_schema(document: &SidecarDocument) -> Result<()> {
    if document.neurons.is_empty() {
        bail!("invalid Spikenaut sidecar: `neurons` is empty");
    }
    if let Some(source) = document.source.as_deref()
        && !ALLOWED_SOURCES.contains(&source)
    {
        bail!(
            "incompatible Spikenaut sidecar source {source:?}; expected one of {ALLOWED_SOURCES:?}"
        );
    }
    if let Some(q88) = document.q88.as_deref()
        && !ALLOWED_Q88.contains(&q88)
    {
        bail!("incompatible Spikenaut sidecar q88 {q88:?}; expected one of {ALLOWED_Q88:?}");
    }
    if let Some(encoder) = document.encoder.as_deref()
        && encoder.is_empty()
    {
        bail!("invalid Spikenaut sidecar: `encoder` is empty");
    }
    Ok(())
}

fn find_hub_config(sidecar_path: &Path) -> Option<PathBuf> {
    let dir = sidecar_path.parent()?;
    let sibling = dir.join("config.json");
    if sibling.is_file() {
        return Some(sibling);
    }
    let merged = dir.file_name()? == "merged_v2";
    if merged {
        let root = dir.parent()?.parent()?;
        let hub = root.join("config.json");
        if hub.is_file() {
            return Some(hub);
        }
    }
    None
}

fn validate_hub_config(path: &Path, document: &SidecarDocument) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read Hugging Face config {}", path.display()))?;
    let hub: HubConfig = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse Hugging Face config {}", path.display()))?;
    if let Some(model_type) = hub.model_type.as_deref()
        && model_type != HUB_MODEL_TYPE
    {
        bail!(
            "incompatible Hugging Face model_type {model_type:?} in {}; expected {HUB_MODEL_TYPE}",
            path.display()
        );
    }
    if let Some(n_neurons) = hub.n_neurons
        && n_neurons != document.neurons.len()
    {
        bail!(
            "Hugging Face n_neurons {n_neurons} does not match sidecar neuron count {}",
            document.neurons.len()
        );
    }
    if let Some(n_channels) = hub.n_channels {
        let width = weight_width(document)?;
        if n_channels != width {
            bail!(
                "Hugging Face n_channels {n_channels} does not match sidecar input width {width}"
            );
        }
    }
    Ok(())
}

fn weight_width(document: &SidecarDocument) -> Result<usize> {
    let expected = document.neurons[0].weights.len();
    if expected == 0 {
        bail!("invalid Spikenaut sidecar: neuron 0 has an empty weight row");
    }
    for (index, neuron) in document.neurons.iter().enumerate() {
        if neuron.weights.len() != expected {
            bail!(
                "invalid Spikenaut sidecar: neuron {index} has {} weights, expected {expected}",
                neuron.weights.len()
            );
        }
    }
    Ok(expected)
}

fn validate_ingress_contract(
    config: &DaemonConfig,
    lif_count: usize,
    channels: usize,
) -> Result<()> {
    if config.lif_count != lif_count {
        bail!(
            "checkpoint LIF count {lif_count} does not match configured lif_count {}",
            config.lif_count
        );
    }
    if config.channels != channels {
        bail!(
            "checkpoint input width {channels} does not match configured channels {} (ingress contract)",
            config.channels
        );
    }
    Ok(())
}

fn validate_finite_parameters(document: &SidecarDocument) -> Result<()> {
    for (index, neuron) in document.neurons.iter().enumerate() {
        require_finite_unit_interval(neuron.decay_rate, &format!("neuron {index} decay_rate"))?;
        require_finite(
            neuron.membrane_potential,
            &format!("neuron {index} membrane_potential"),
        )?;
        require_finite(neuron.threshold, &format!("neuron {index} threshold"))?;
        for (column, &weight) in neuron.weights.iter().enumerate() {
            require_finite(weight, &format!("neuron {index} weight {column}"))?;
        }
        if let Some(outputs) = neuron.output_weights.as_ref() {
            for (column, &weight) in outputs.iter().enumerate() {
                require_finite(weight, &format!("neuron {index} output_weight {column}"))?;
            }
        }
    }
    Ok(())
}

fn require_finite(value: f64, context: &str) -> Result<()> {
    if !value.is_finite() {
        bail!("invalid Spikenaut sidecar: {context} is {value}, expected a finite number");
    }
    Ok(())
}

fn require_finite_unit_interval(value: f64, context: &str) -> Result<()> {
    require_finite(value, context)?;
    if !(value > 0.0 && value < 1.0) {
        bail!(
            "invalid Spikenaut sidecar: {context} is {value}, expected a decay multiplier in (0, 1)"
        );
    }
    Ok(())
}

fn validate_nonblank_weights(document: &SidecarDocument) -> Result<()> {
    let mut max_abs = 0.0_f32;
    for neuron in &document.neurons {
        for &weight in &neuron.weights {
            max_abs = max_abs.max(weight.abs() as f32);
        }
    }
    if max_abs <= BLANK_WEIGHT_ABS_MAX {
        bail!(
            "live mode cannot load a blank/uninitialized checkpoint (max |weight| {max_abs} <= {BLANK_WEIGHT_ABS_MAX})"
        );
    }
    Ok(())
}

fn apply_sidecar(network: &mut SpikingNetwork, document: &SidecarDocument) -> Result<()> {
    if network.neurons.len() != document.neurons.len() {
        bail!(
            "internal restore mismatch: network has {} LIF cells, sidecar has {}",
            network.neurons.len(),
            document.neurons.len()
        );
    }
    for (dst, src) in network.neurons.iter_mut().zip(document.neurons.iter()) {
        dst.weights = src.weights.iter().map(|w| *w as f32).collect();
        dst.membrane_potential = src.membrane_potential as f32;
        dst.threshold = src.threshold as f32;
        dst.decay_rate = src.decay_rate as f32;
        dst.last_spike = src.last_spike;
    }
    Ok(())
}

fn model_id(document: &SidecarDocument, sidecar_path: &Path) -> String {
    if let Some(lineage) = document.frozen_lineage.as_deref().filter(|s| !s.is_empty()) {
        return format!("spikenaut-snn:{lineage}");
    }
    match (document.source.as_deref(), document.encoder.as_deref()) {
        (Some(source), Some(encoder)) => format!("spikenaut-snn:{source}:{encoder}"),
        _ => format!("spikenaut-snn:{}", sidecar_path.display()),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DaemonConfig;
    use crate::registry::ServiceConfig;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brainstem-checkpoint-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp fixture dir");
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
        assert_eq!(provenance.schema_id, SCHEMA_ID);
        assert_eq!(provenance.model_id, "spikenaut-snn:test-fixture");
        assert_eq!(provenance.source_path, path);
        assert_eq!(provenance.content_sha256.len(), 64);
        assert_eq!(provenance.encoder.as_deref(), Some("v3_state_telemetry"));
        assert_eq!(provenance.frozen_lineage.as_deref(), Some("test-fixture"));
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

    fn restore_failed(cfg: &DaemonConfig) -> anyhow::Error {
        match restore_network(cfg) {
            Ok(_) => panic!("expected restore_network to fail"),
            Err(err) => err,
        }
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
        let cfg = live_config(path, 16, 16);
        let err = restore_failed(&cfg);
        let message = err.to_string();
        assert!(
            message.contains("LIF count") || message.contains("input width"),
            "unexpected error: {message}"
        );
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
            message.contains("finite")
                || message.contains("parse")
                || message.contains("out of range"),
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
    }

    #[test]
    fn live_does_not_silently_replace_failed_load_with_blank_state() {
        let cfg = live_config(PathBuf::from("/tmp/does-not-exist-snn_model.json"), 2, 2);
        assert!(restore_network(&cfg).is_err());
    }
}
