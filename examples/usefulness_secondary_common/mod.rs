// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Shared workload, frame encoding, and raw-output helpers for the secondary usefulness
//! experiment drivers (`experiments/usefulness-secondary/`). Public crate APIs only.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use brainstem_daemon::ingress::STIMULUS_SCHEMA;
use corpus_ipc::{BatchMetadata, IpcMessage, NeuromodulatorSnapshot, StimulusBatch, Validate};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const WORKLOAD_PATH: &str = "experiments/usefulness-secondary/fixtures/workload.json";
pub const FIXTURE_PATH: &str = "experiments/usefulness-secondary/fixtures/snn_model.json";
pub const SESSION_ID: &str = "usefulness-experiment";

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Modulators {
    pub dopamine: f32,
    pub cortisol: f32,
    pub acetylcholine: f32,
    pub tempo: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    pub name: String,
    pub ticks: usize,
    pub values: Vec<f32>,
    #[serde(default)]
    pub valid_mask: Option<Vec<bool>>,
    #[serde(default)]
    pub neuromodulators: Option<Modulators>,
}

impl Segment {
    /// Reject a segment whose `values` or `valid_mask` width differs from `channels`.
    fn check_width(&self, channels: usize) -> Result<()> {
        let mask_len = self.valid_mask.as_ref().map_or(channels, Vec::len);
        if self.values.len() != channels || mask_len != channels {
            bail!("segment {} width != {channels}", self.name);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransportSpec {
    pub tick_rate_hz: u32,
    pub probes: usize,
    pub probe_interval_ms: u64,
    pub probe_value: f32,
    pub priming_channel: usize,
    pub priming_interval_ms: u64,
    pub priming_timeout_ms: u64,
    pub settle_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workload {
    pub workload_version: String,
    pub channels: usize,
    pub cycles: usize,
    pub warmup_cycles: usize,
    pub segments: Vec<Segment>,
    pub transport: TransportSpec,
}

impl Workload {
    pub fn load() -> Result<Self> {
        let text = std::fs::read_to_string(WORKLOAD_PATH)
            .with_context(|| format!("failed to read {WORKLOAD_PATH}"))?;
        let workload: Self = serde_json::from_str(&text)?;
        for segment in &workload.segments {
            segment.check_width(workload.channels)?;
        }
        Ok(workload)
    }

    pub fn cycle_len(&self) -> usize {
        self.segments.iter().map(|s| s.ticks).sum()
    }

    pub fn warmup_ticks(&self) -> usize {
        self.warmup_cycles * self.cycle_len()
    }

    /// Expand the workload into one input per tick for `variant`.
    pub fn expand(&self, variant: Variant) -> Vec<TickInput> {
        let mut ticks = Vec::with_capacity(self.cycles * self.cycle_len());
        for _ in 0..self.cycles {
            for segment in &self.segments {
                let (values, valid_mask) = variant.apply(segment);
                for _ in 0..segment.ticks {
                    ticks.push(TickInput {
                        segment: segment.name.clone(),
                        values: values.clone(),
                        valid_mask: valid_mask.clone(),
                        modulators: segment.neuromodulators,
                    });
                }
            }
        }
        ticks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    Main,
    Zeroed,
    Unmasked,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Zeroed => "zeroed",
            Self::Unmasked => "unmasked",
        }
    }

    fn apply(self, segment: &Segment) -> (Vec<f32>, Option<Vec<bool>>) {
        match (self, segment.valid_mask.as_ref()) {
            (Self::Main, mask) => (segment.values.clone(), mask.cloned()),
            (Self::Zeroed, Some(mask)) => {
                let values = segment
                    .values
                    .iter()
                    .zip(mask)
                    .map(|(&v, &ok)| if ok { v } else { 0.0 })
                    .collect();
                (values, Some(vec![true; mask.len()]))
            }
            (Self::Zeroed, None) => (segment.values.clone(), None),
            (Self::Unmasked, _) => (segment.values.clone(), None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TickInput {
    pub segment: String,
    pub values: Vec<f32>,
    pub valid_mask: Option<Vec<bool>>,
    pub modulators: Option<Modulators>,
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Encode one typed stimulus frame through the published `IpcMessage` contract.
pub fn stimulus_frame(
    batch_id: u64,
    values: &[f32],
    valid_mask: Option<&[bool]>,
) -> Result<Vec<u8>> {
    let mut custom = HashMap::default();
    custom.insert("schema".to_string(), STIMULUS_SCHEMA.to_string());
    let batch = StimulusBatch {
        session_id: Some(SESSION_ID.to_string()),
        batch_id,
        timestamp: now_ns(),
        values: values.to_vec(),
        valid_mask: valid_mask.map(<[bool]>::to_vec),
        metadata: Some(BatchMetadata {
            processing_latency_ns: None,
            source: Some("usefulness-driver".to_string()),
            custom,
        }),
    };
    batch
        .validate()
        .map_err(|e| anyhow::anyhow!("driver built an invalid StimulusBatch: {e}"))?;
    Ok(serde_json::to_vec(&IpcMessage::Stimuli(batch))?)
}

/// Encode one typed neuromodulator frame.
pub fn modulator_frame(tick: i64, mods: Modulators) -> Result<Vec<u8>> {
    let snapshot = NeuromodulatorSnapshot {
        tick,
        dopamine: mods.dopamine,
        cortisol: mods.cortisol,
        acetylcholine: mods.acetylcholine,
        tempo: mods.tempo,
    };
    snapshot
        .validate()
        .map_err(|e| anyhow::anyhow!("driver built an invalid NeuromodulatorSnapshot: {e}"))?;
    Ok(serde_json::to_vec(&IpcMessage::Neuromodulators(snapshot))?)
}

pub fn file_sha256(path: &str) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {path}"))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `VmHWM` / `VmRSS` (kB) from a Linux `/proc/<pid>/status` file.
pub fn proc_memory_kb(status_path: &str) -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(status_path) else {
        return (None, None);
    };
    let field = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
    };
    (field("VmHWM:"), field("VmRSS:"))
}

pub fn jsonl_writer(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path).with_context(|| {
        format!("failed to create {}", path.display())
    })?))
}

pub fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut file = jsonl_writer(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    Ok(())
}

/// Minimal blocking HTTP GET against the control surface.
pub fn http_get(addr: &str, path: &str) -> Result<String> {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::default();
    stream.read_to_string(&mut response)?;
    Ok(response)
}
