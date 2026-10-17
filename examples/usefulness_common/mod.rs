// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Helpers shared by the usefulness experiment drivers
//! (`experiments/usefulness`). Each driver uses a subset, so unused items are
//! allowed here.
#![allow(dead_code)]

pub mod frames;
pub mod process;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use brainstem_daemon::ingress::STIMULUS_SCHEMA;
use corpus_ipc::{BatchMetadata, IpcMessage, StimulusBatch};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize)]
pub struct Modulators {
    pub dopamine: f32,
    pub cortisol: f32,
    pub acetylcholine: f32,
    pub tempo: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub offset: u64,
    pub values: Vec<f32>,
    pub valid_mask: Option<Vec<bool>>,
    pub modulators: Option<Modulators>,
}

#[derive(Debug, Deserialize)]
pub struct Transport {
    pub pulse_spacing_ticks: u64,
    pub pulses: u64,
    pub amplitude: f32,
    pub expected_by_channel: HashMap<String, Vec<u16>>,
}

/// `experiments/usefulness/fixtures/workload.json`.
#[derive(Debug, Deserialize)]
pub struct Workload {
    pub fixture_sha256: String,
    pub channels: usize,
    pub ticks_per_cycle: u64,
    pub cycles: u64,
    pub ticks: u64,
    pub warmup_ticks: u64,
    pub cycle_events: Vec<Event>,
    pub transport: Transport,
}

impl Workload {
    pub fn load(path: &Path) -> Result<Self> {
        let workload: Self = read_json(path, "workload")?;
        if workload.ticks != workload.ticks_per_cycle * workload.cycles {
            bail!("workload ticks do not match cycles × ticks_per_cycle");
        }
        Ok(workload)
    }

    /// The configured ingress width must equal the workload width.
    pub fn check_channels(&self, channels: usize) -> Result<()> {
        if channels != self.channels {
            bail!(
                "config channels {channels} != workload channels {}",
                self.channels
            );
        }
        Ok(())
    }

    pub fn expand(&self) -> HashMap<u64, Event> {
        let mut events = HashMap::new();
        for cycle in 0..self.cycles {
            for event in &self.cycle_events {
                events.insert(cycle * self.ticks_per_cycle + event.offset, event.clone());
            }
        }
        events
    }

    /// Stimulus and modulator frames one full run sends.
    pub fn frame_counts(&self) -> (u64, u64) {
        let stimuli = self
            .cycle_events
            .iter()
            .filter(|e| !e.values.is_empty())
            .count() as u64;
        let modulators = self
            .cycle_events
            .iter()
            .filter(|e| e.modulators.is_some())
            .count() as u64;
        (stimuli * self.cycles, modulators * self.cycles)
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path, what: &str) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read {what} {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {what} {}", path.display()))
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Typed `IpcMessage::Stimuli` frame carrying the schema token the daemon
/// requires, stamped with the current time so the freshness rule accepts it.
pub fn stimulus_frame(
    session: &str,
    batch_id: u64,
    values: Vec<f32>,
    valid_mask: Option<Vec<bool>>,
) -> Result<Vec<u8>> {
    let mut custom = HashMap::new();
    custom.insert("schema".to_string(), STIMULUS_SCHEMA.to_string());
    let batch = StimulusBatch {
        session_id: Some(session.to_string()),
        batch_id,
        timestamp: now_ns(),
        values,
        valid_mask,
        metadata: Some(BatchMetadata {
            processing_latency_ns: None,
            source: Some(format!("{session}-driver")),
            custom,
        }),
    };
    Ok(serde_json::to_vec(&IpcMessage::Stimuli(batch))?)
}

/// A `/proc/<pid>/status` value in kB (`VmHWM:`, `VmRSS:`); `None` pid means this process.
pub fn vm_status_kb(pid: Option<u32>, key: &str) -> Option<u64> {
    let path = pid.map_or_else(
        || "/proc/self/status".to_string(),
        |pid| format!("/proc/{pid}/status"),
    );
    let status = fs::read_to_string(path).ok()?;
    status
        .lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Nearest-rank percentile: `sorted[round((n - 1) * p)]`.
pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted.get(rank).copied()
}
