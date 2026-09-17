// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Spikenaut checkpoint loader and the live-mode startup gate.
//!
//! Canonical software artifact: Distill sidecar JSON (`snn_model.json`) as
//! published by Hugging Face `rmems/Spikenaut-SNN` (`dataset/merged_v2/`).
//! Field-programmable gate array (FPGA) Q8.8 `.mem` dumps are not a
//! software checkpoint.
//!
//! Live mode restores this document into `neuromod::SpikingNetwork` and
//! refuses to start on unreadable, incompatible, non-finite, or blank
//! state. Simulation mode is the only path that constructs a blank
//! `with_dimensions()` network.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use neuromod::{RmStdpConfig, SpikingNetwork};
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
    crate::daemon::validate_neuron_count(config)?;
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
    require_live_izh_count(config)?;
    let sidecar_path = resolve_sidecar_path(&config.model_path)?;
    let (bytes, document) = parse_sidecar_file(&sidecar_path)?;
    validate_live_document(config, &document, &sidecar_path)?;
    let network = restore_spiking_network(config, &document)?;
    Ok((network, provenance_of(&document, &sidecar_path, &bytes)))
}

fn require_live_izh_count(config: &DaemonConfig) -> Result<()> {
    if config.izh_count != 0 {
        bail!(
            "live Spikenaut checkpoints are LIF-only; izh_count must be 0, got {}",
            config.izh_count
        );
    }
    Ok(())
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
        match sidecar_in_directory(model_path) {
            Some(path) => return Ok(path),
            None => bail!(
                "checkpoint directory {} has no snn_model.json or dataset/merged_v2/snn_model.json",
                model_path.display()
            ),
        }
    }

    if is_mem_dump(model_path) {
        bail!(
            "live mode cannot load FPGA Q8.8 .mem dumps as a software checkpoint; \
             point model_path at Distill sidecar snn_model.json (Hugging Face rmems/Spikenaut-SNN)"
        );
    }

    if is_hub_config(model_path) {
        return resolve_from_hub_config(model_path);
    }

    Ok(model_path.to_path_buf())
}

fn sidecar_in_directory(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("snn_model.json");
    if direct.is_file() {
        return Some(direct);
    }
    let nested = dir.join("dataset/merged_v2/snn_model.json");
    nested.is_file().then_some(nested)
}

fn is_mem_dump(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mem"))
}

fn is_hub_config(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "config.json")
}

fn resolve_from_hub_config(config_path: &Path) -> Result<PathBuf> {
    let root = config_path.parent().unwrap_or(config_path);
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
        config_path.display()
    );
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
    /// Sidecar readout row. neuromod 0.6.0 has no Distill readout matrix;
    /// values are validated as finite and then ignored (fail-closed rejection
    /// of this field is stacked on PR #57).
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

fn parse_sidecar_file(sidecar_path: &Path) -> Result<(Vec<u8>, SidecarDocument)> {
    let bytes = fs::read(sidecar_path)
        .with_context(|| format!("failed to read checkpoint {}", sidecar_path.display()))?;
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
    Ok((bytes, document))
}

fn validate_live_document(
    config: &DaemonConfig,
    document: &SidecarDocument,
    sidecar_path: &Path,
) -> Result<()> {
    validate_schema(document)?;
    if let Some(hub_path) = find_hub_config(sidecar_path) {
        validate_hub_config(&hub_path, document)?;
    }
    let channels = weight_width(document)?;
    validate_ingress_contract(config, document.neurons.len(), channels)?;
    validate_finite_parameters(document)?;
    validate_nonblank_weights(document)
}

fn validate_schema(document: &SidecarDocument) -> Result<()> {
    if document.neurons.is_empty() {
        bail!("invalid Spikenaut sidecar: `neurons` is empty");
    }
    require_allowed_source(document.source.as_deref())?;
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

fn require_allowed_source(source: Option<&str>) -> Result<()> {
    match source {
        Some(source) if ALLOWED_SOURCES.contains(&source) => Ok(()),
        Some(source) => bail!(
            "incompatible Spikenaut sidecar source {source:?}; expected one of {ALLOWED_SOURCES:?}"
        ),
        None => bail!(
            "incompatible Spikenaut sidecar: missing `source`; expected one of {ALLOWED_SOURCES:?}"
        ),
    }
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
    // Distill stores f64; neuromod 0.6 restores f32. Reject values that
    // overflow so restore cannot inject infinities.
    if !(value as f32).is_finite() {
        bail!("invalid Spikenaut sidecar: {context} is {value}, which overflows f32");
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

fn restore_spiking_network(
    config: &DaemonConfig,
    document: &SidecarDocument,
) -> Result<SpikingNetwork> {
    let mut network =
        SpikingNetwork::with_dimensions(config.lif_count, config.izh_count, config.channels);
    apply_sidecar(&mut network, document)?;
    Ok(network)
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
        dst.base_threshold = src.threshold as f32;
        dst.decay_rate = src.decay_rate as f32;
        dst.last_spike = src.last_spike;
    }
    // Inference-only: dopamine-gated R-STDP must not retrain Distill weights.
    // neuromod 0.6.0 `step` still assigns `decay_rate` from acetylcholine,
    // blends `threshold` toward 0.05..=0.50, and L1-renormalizes rows whose
    // weights already sum above 1e-6 — those are engine contracts, not
    // sidecar fields this loader can freeze without forking neuromod.
    network.set_rm_stdp_config(RmStdpConfig {
        reward_lr: 0.0,
        ..network.stdp_config
    });
    Ok(())
}

fn provenance_of(document: &SidecarDocument, sidecar_path: &Path, bytes: &[u8]) -> ModelProvenance {
    ModelProvenance {
        schema_id: SCHEMA_ID.to_string(),
        source_path: sidecar_path.to_path_buf(),
        content_sha256: hex_sha256(bytes),
        model_id: model_id(document, sidecar_path),
        encoder: document.encoder.clone(),
        source: document.source.clone(),
        frozen_lineage: document.frozen_lineage.clone(),
    }
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
mod tests;
