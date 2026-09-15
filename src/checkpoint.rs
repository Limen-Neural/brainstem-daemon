// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Explicit Spikenaut/neuromod checkpoint load and validation.
//!
//! Live ticks should restore a recorded network rather than silently
//! constructing a blank `SpikingNetwork::with_dimensions` when a checkpoint
//! file is present. Missing files still yield `None` so the historical stub
//! path (dummy `model_path`) keeps working; a file that exists but is invalid
//! fails closed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use neuromod::SpikingNetwork;
use serde::{Deserialize, Serialize};

/// Checkpoint schema version understood by this loader.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Dimensions the checkpoint must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkDims {
    pub lif_count: usize,
    pub izh_count: usize,
    pub channels: usize,
}

/// Provenance recorded after a successful load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointIdentity {
    pub model_id: String,
    pub schema_version: u32,
    pub path: PathBuf,
    pub fingerprint: String,
    pub dims: NetworkDims,
}

/// On-disk envelope around a serializable `neuromod::SpikingNetwork`.
#[derive(Serialize, Deserialize)]
pub struct CheckpointFile {
    pub schema_version: u32,
    pub model_id: String,
    pub network: SpikingNetwork,
}

impl CheckpointFile {
    /// Validate schema, dimensions, finite parameters, and non-blank weights.
    pub fn validate(&self, expected: NetworkDims) -> Result<()> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION {
            bail!(
                "checkpoint schema_version {} is incompatible with runtime {}",
                self.schema_version,
                CHECKPOINT_SCHEMA_VERSION
            );
        }
        if self.model_id.trim().is_empty() {
            bail!("checkpoint model_id must be non-empty");
        }

        let got = NetworkDims {
            lif_count: self.network.neurons.len(),
            izh_count: self.network.iz_neurons.len(),
            channels: self.network.num_channels,
        };
        if got != expected {
            bail!(
                "checkpoint dimensions lif={}, izh={}, channels={} do not match config lif={}, izh={}, channels={}",
                got.lif_count,
                got.izh_count,
                got.channels,
                expected.lif_count,
                expected.izh_count,
                expected.channels
            );
        }

        if self.network.input_spike_times.len() != expected.channels
            || self.network.predictive_state.len() != expected.channels
        {
            bail!("checkpoint input-state width does not match channel count");
        }

        let mut any_weight = false;
        for (idx, neuron) in self.network.neurons.iter().enumerate() {
            if neuron.weights.len() != expected.channels {
                bail!(
                    "checkpoint LIF[{idx}] weight width {} != channels {}",
                    neuron.weights.len(),
                    expected.channels
                );
            }
            if !neuron.membrane_potential.is_finite()
                || !neuron.decay_rate.is_finite()
                || !neuron.threshold.is_finite()
                || !neuron.base_threshold.is_finite()
            {
                bail!("checkpoint LIF[{idx}] has a non-finite parameter");
            }
            for (ch, weight) in neuron.weights.iter().enumerate() {
                if !weight.is_finite() {
                    bail!("checkpoint LIF[{idx}] weight[{ch}] is not finite");
                }
                if weight.abs() > 1e-8 {
                    any_weight = true;
                }
            }
        }

        if expected.lif_count > 0 && !any_weight {
            bail!("checkpoint looks blank: all LIF input weights are zero");
        }

        Ok(())
    }
}

/// Load `path` when it exists. `Ok(None)` if the path is absent.
pub fn try_load_checkpoint(
    path: &Path,
    expected: NetworkDims,
) -> Result<Option<(SpikingNetwork, CheckpointIdentity)>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(load_checkpoint(path, expected)?))
}

/// Load and validate a checkpoint file. Fails if the file is missing or invalid.
pub fn load_checkpoint(
    path: &Path,
    expected: NetworkDims,
) -> Result<(SpikingNetwork, CheckpointIdentity)> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read checkpoint {}", path.display()))?;
    let parsed: CheckpointFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse checkpoint JSON {}", path.display()))?;
    parsed.validate(expected)?;

    let identity = CheckpointIdentity {
        model_id: parsed.model_id.clone(),
        schema_version: parsed.schema_version,
        path: path.to_path_buf(),
        fingerprint: fnv1a64_hex(&bytes),
        dims: expected,
    };
    Ok((parsed.network, identity))
}

/// Write a non-blank, dimension-checked smoke checkpoint to `path`.
pub fn write_nonblank_checkpoint(
    path: &Path,
    model_id: &str,
    dims: NetworkDims,
) -> Result<CheckpointIdentity> {
    if dims.lif_count == 0 {
        bail!("smoke checkpoint requires at least one LIF neuron");
    }
    if dims.channels == 0 {
        bail!("smoke checkpoint requires at least one input channel");
    }

    let mut network =
        SpikingNetwork::with_dimensions(dims.lif_count, dims.izh_count, dims.channels);
    for (idx, neuron) in network.neurons.iter_mut().enumerate() {
        for (ch, weight) in neuron.weights.iter_mut().enumerate() {
            *weight = 0.12 + (idx as f32) * 0.03 + (ch as f32) * 0.01;
        }
        if let Some(first) = neuron.weights.first_mut() {
            // Strong channel-0 coupling so a unit stimulus deterministically exceeds threshold.
            if idx == 0 {
                *first = 2.0;
            }
        }
        neuron.threshold = 0.02;
        neuron.base_threshold = 0.02;
        neuron.membrane_potential = 0.0;
    }

    let file = CheckpointFile {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        model_id: model_id.to_string(),
        network,
    };
    file.validate(dims)?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&file).context("failed to serialize checkpoint")?;
    fs::write(path, &bytes)
        .with_context(|| format!("failed to write checkpoint {}", path.display()))?;

    Ok(CheckpointIdentity {
        model_id: model_id.to_string(),
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        path: path.to_path_buf(),
        fingerprint: fnv1a64_hex(&bytes),
        dims,
    })
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "brainstem-checkpoint-{name}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    fn smoke_dims() -> NetworkDims {
        NetworkDims {
            lif_count: 4,
            izh_count: 0,
            channels: 4,
        }
    }

    #[test]
    fn missing_path_is_none() {
        let path = PathBuf::from("/tmp/brainstem-daemon-missing-checkpoint-does-not-exist.json");
        let loaded = try_load_checkpoint(&path, smoke_dims()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn round_trip_nonblank_checkpoint() {
        let path = temp_path("round-trip");
        let identity = write_nonblank_checkpoint(&path, "smoke-v1", smoke_dims()).unwrap();
        let (network, loaded) = load_checkpoint(&path, smoke_dims()).unwrap();
        assert_eq!(loaded.model_id, "smoke-v1");
        assert_eq!(loaded.fingerprint, identity.fingerprint);
        assert_eq!(network.neurons.len(), 4);
        assert!(network.neurons[0].weights[0].abs() > 1.0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn blank_weights_are_rejected() {
        let path = temp_path("blank");
        let network = SpikingNetwork::with_dimensions(2, 0, 2);
        let file = CheckpointFile {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            model_id: "blank".into(),
            network,
        };
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let err = match load_checkpoint(
            &path,
            NetworkDims {
                lif_count: 2,
                izh_count: 0,
                channels: 2,
            },
        ) {
            Ok(_) => panic!("expected blank checkpoint to fail"),
            Err(err) => err,
        };
        let _ = fs::remove_file(&path);
        let message = err.to_string();
        assert!(message.contains("blank"), "unexpected error: {message}");
    }

    #[test]
    fn schema_mismatch_fails_loudly() {
        let path = temp_path("schema");
        let mut file = CheckpointFile {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            model_id: "x".into(),
            network: SpikingNetwork::with_dimensions(4, 0, 4),
        };
        file.network.neurons[0].weights[0] = 1.0;
        file.schema_version = 99;
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let err = expect_load_err(&path, smoke_dims());
        let _ = fs::remove_file(&path);
        let message = err.to_string();
        assert!(
            message.contains("schema_version"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn dimension_mismatch_fails() {
        let path = temp_path("dims");
        write_nonblank_checkpoint(&path, "smoke-v1", smoke_dims()).unwrap();
        let err = expect_load_err(
            &path,
            NetworkDims {
                lif_count: 8,
                izh_count: 0,
                channels: 4,
            },
        );
        let _ = fs::remove_file(&path);
        let message = err.to_string();
        assert!(
            message.contains("dimensions"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn non_finite_weight_is_rejected_by_validate() {
        let mut network = SpikingNetwork::with_dimensions(4, 0, 4);
        network.neurons[0].weights[0] = f32::NAN;
        let file = CheckpointFile {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            model_id: "nan".into(),
            network,
        };
        let err = file.validate(smoke_dims()).unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn corrupt_json_fails_loudly() {
        let path = temp_path("corrupt");
        fs::write(&path, "{not-json").unwrap();
        let err = expect_load_err(&path, smoke_dims());
        let _ = fs::remove_file(&path);
        let message = err.to_string();
        assert!(
            message.contains("parse") || message.contains("JSON") || message.contains("expected"),
            "unexpected error: {message}"
        );
    }

    fn expect_load_err(path: &Path, dims: NetworkDims) -> anyhow::Error {
        match load_checkpoint(path, dims) {
            Ok(_) => panic!("expected checkpoint load to fail"),
            Err(err) => err,
        }
    }
}
