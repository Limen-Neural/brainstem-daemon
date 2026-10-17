// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! In-process driver for the usefulness experiment (`experiments/usefulness`).
//!
//! Modes:
//! - `daemon-live` / `daemon-simulation`: the supported library path
//!   (`DaemonConfig::load` → `BrainstemDaemon::try_with_backend` →
//!   `run_for_ticks`) with a workload `StimulusSource` and a recording
//!   `SpikeSink`. Frames are encoded as typed `corpus-ipc` messages and
//!   decoded through the public `accept_ipc_json` path.
//! - `baseline`: the same restored network stepped directly with the daemon's
//!   documented decoding applied by this driver.
//!
//! Usage: `usefulness_inprocess <mode> <config.toml> <workload.json> <out.jsonl> <summary.json>`
//!
//! Requires `--features corpus-ipc`. Linux only (`/proc/self/status`).

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode};
use brainstem_daemon::ingress::{IngressPolicy, MessageClass, STIMULUS_SCHEMA, accept_ipc_json};
use brainstem_daemon::{BackendPair, IngressPacket, SpikeEvent, SpikeSink, StimulusSource};
use corpus_ipc::{BatchMetadata, IpcMessage, NeuromodulatorSnapshot, StimulusBatch};
use neuromod::NeuroModulators;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SESSION_ID: &str = "usefulness";

#[derive(Debug, Clone, Deserialize)]
struct Modulators {
    dopamine: f32,
    cortisol: f32,
    acetylcholine: f32,
    tempo: f32,
}

#[derive(Debug, Clone, Deserialize)]
struct Event {
    offset: u64,
    values: Vec<f32>,
    valid_mask: Option<Vec<bool>>,
    modulators: Option<Modulators>,
}

#[derive(Debug, Deserialize)]
struct Workload {
    fixture_sha256: String,
    channels: usize,
    ticks_per_cycle: u64,
    cycles: u64,
    ticks: u64,
    warmup_ticks: u64,
    cycle_events: Vec<Event>,
}

impl Workload {
    fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read workload {}", path.display()))?;
        let workload: Self = serde_json::from_str(&text).context("parse workload")?;
        if workload.ticks != workload.ticks_per_cycle * workload.cycles {
            bail!("workload ticks do not match cycles × ticks_per_cycle");
        }
        Ok(workload)
    }

    fn expand(&self) -> HashMap<u64, Event> {
        let mut events = HashMap::new();
        for cycle in 0..self.cycles {
            for event in &self.cycle_events {
                events.insert(cycle * self.ticks_per_cycle + event.offset, event.clone());
            }
        }
        events
    }
}

/// One tick as observed by the driver.
#[derive(Debug, Clone)]
struct TickRecord {
    channels: Vec<u16>,
    latency_ns: u64,
    decode_ns: Option<u64>,
}

#[derive(Default)]
struct Shared {
    polls: Vec<(Instant, Option<u64>)>,
    emits: Vec<(Instant, Vec<u16>)>,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn encode_stimulus(event: &Event, batch_id: u64) -> Result<Vec<u8>> {
    let mut custom = HashMap::new();
    custom.insert("schema".to_string(), STIMULUS_SCHEMA.to_string());
    let batch = StimulusBatch {
        session_id: Some(SESSION_ID.to_string()),
        batch_id,
        timestamp: now_ns(),
        values: event.values.clone(),
        valid_mask: event.valid_mask.clone(),
        metadata: Some(BatchMetadata {
            processing_latency_ns: None,
            source: Some("usefulness-driver".to_string()),
            custom,
        }),
    };
    Ok(serde_json::to_vec(&IpcMessage::Stimuli(batch))?)
}

fn encode_modulators(mods: &Modulators, tick: u64) -> Result<Vec<u8>> {
    let snapshot = NeuromodulatorSnapshot {
        tick: i64::try_from(tick)?,
        dopamine: mods.dopamine,
        cortisol: mods.cortisol,
        acetylcholine: mods.acetylcholine,
        tempo: mods.tempo,
    };
    Ok(serde_json::to_vec(&IpcMessage::Neuromodulators(snapshot))?)
}

/// Producer-side encoding happens before the poll instant; decoding through
/// the public ingress path is timed separately as `decode_ns`.
struct WorkloadSource {
    events: HashMap<u64, Event>,
    tick: u64,
    next_batch_id: u64,
    policy: IngressPolicy,
    shared: Arc<Mutex<Shared>>,
    stimulus_frames: u64,
    modulator_frames: u64,
}

impl StimulusSource for WorkloadSource {
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
        let tick = self.tick;
        self.tick += 1;
        let Some(event) = self.events.get(&tick) else {
            self.shared
                .lock()
                .unwrap()
                .polls
                .push((Instant::now(), None));
            return Ok(None);
        };
        let stimulus_bytes = if event.values.is_empty() {
            None
        } else {
            let id = self.next_batch_id;
            self.next_batch_id += 1;
            self.stimulus_frames += 1;
            Some(encode_stimulus(event, id)?)
        };
        let modulator_bytes = match event.modulators.as_ref() {
            Some(mods) => {
                self.modulator_frames += 1;
                Some(encode_modulators(mods, tick)?)
            }
            None => None,
        };

        let decode_start = Instant::now();
        let now = now_ns();
        let mut packet = match stimulus_bytes.as_deref() {
            Some(bytes) => accept_ipc_json(bytes, &self.policy, now)?,
            None => IngressPacket::default(),
        };
        if let Some(bytes) = modulator_bytes.as_deref() {
            let mods = accept_ipc_json(bytes, &self.policy, now)?;
            packet.modulators = mods.modulators;
        }
        let poll = Instant::now();
        let decode_ns = poll.duration_since(decode_start).as_nanos() as u64;
        self.shared
            .lock()
            .unwrap()
            .polls
            .push((poll, Some(decode_ns)));
        Ok(Some(packet))
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
        Ok(())
    }
}

struct RecordingSink {
    shared: Arc<Mutex<Shared>>,
}

impl SpikeSink for RecordingSink {
    fn emit(&mut self, spikes: &[SpikeEvent], _batch_time: Duration) -> Result<()> {
        let at = Instant::now();
        let mut channels: Vec<u16> = spikes.iter().map(|s| s.channel).collect();
        channels.sort_unstable();
        self.shared.lock().unwrap().emits.push((at, channels));
        Ok(())
    }
}

fn vm_status_kb(key: &str) -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn write_records(path: &Path, records: &[TickRecord]) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    for (tick, record) in records.iter().enumerate() {
        let channels: Vec<String> = record.channels.iter().map(u16::to_string).collect();
        write!(
            out,
            "{{\"tick\":{tick},\"channels\":[{}],\"latency_ns\":{}",
            channels.join(","),
            record.latency_ns
        )?;
        if let Some(decode_ns) = record.decode_ns {
            write!(out, ",\"decode_ns\":{decode_ns}")?;
        }
        writeln!(out, "}}")?;
    }
    out.flush()?;
    Ok(())
}

fn timing_summary(
    records: &[TickRecord],
    warmup: usize,
    first_measured_start: Instant,
    last_end: Instant,
) -> serde_json::Value {
    let measured = records.len().saturating_sub(warmup);
    let wall = last_end.duration_since(first_measured_start);
    let ticks_per_s = if wall.is_zero() {
        0.0
    } else {
        measured as f64 / wall.as_secs_f64()
    };
    serde_json::json!({
        "ticks_total": records.len(),
        "warmup_ticks_excluded": warmup,
        "measured_ticks": measured,
        "measured_wall_ns": wall.as_nanos() as u64,
        "ticks_per_s": ticks_per_s,
    })
}

fn class_metrics(metrics: &brainstem_daemon::ClassMetrics) -> serde_json::Value {
    serde_json::json!({
        "accepted": metrics.accepted,
        "rejected": metrics.rejected,
        "dropped": metrics.dropped,
        "coalesced": metrics.coalesced,
        "shutdown_refused": metrics.shutdown_refused,
        "high_water_mark": metrics.high_water_mark,
    })
}

struct Run {
    records: Vec<TickRecord>,
    first_measured_start: Instant,
    last_end: Instant,
    summary: serde_json::Value,
}

fn run_daemon(cfg: DaemonConfig, workload: &Workload) -> Result<Run> {
    let shared = Arc::new(Mutex::new(Shared::default()));
    {
        let mut guard = shared.lock().unwrap();
        guard.polls.reserve(workload.ticks as usize);
        guard.emits.reserve(workload.ticks as usize);
    }
    let source = WorkloadSource {
        events: workload.expand(),
        tick: 0,
        next_batch_id: 1,
        policy: IngressPolicy::new(cfg.channels, Some(Duration::from_secs(1))),
        shared: Arc::clone(&shared),
        stimulus_frames: 0,
        modulator_frames: 0,
    };
    let stimulus_frames_expected = workload.cycles
        * workload
            .cycle_events
            .iter()
            .filter(|e| !e.values.is_empty())
            .count() as u64;
    let modulator_frames_expected = workload.cycles
        * workload
            .cycle_events
            .iter()
            .filter(|e| e.modulators.is_some())
            .count() as u64;
    let sink = RecordingSink {
        shared: Arc::clone(&shared),
    };
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    let daemon = BrainstemDaemon::try_with_backend(cfg, pair).context("construct daemon")?;
    let health = daemon.health();
    let ingress = daemon.ingress();

    let started = Instant::now();
    let stats = daemon
        .run_for_ticks(workload.ticks)
        .context("run_for_ticks")?;
    let finished = Instant::now();

    let snapshot = health.snapshot();
    let metrics = ingress.metrics();
    let shared = Arc::try_unwrap(shared)
        .map_err(|_| anyhow::anyhow!("backend still holds the shared recorder"))?
        .into_inner()
        .unwrap();
    if shared.polls.len() != shared.emits.len() {
        bail!(
            "poll/emit count mismatch: {} polls, {} emits (a tick was skipped or failed)",
            shared.polls.len(),
            shared.emits.len()
        );
    }
    let records: Vec<TickRecord> = shared
        .polls
        .iter()
        .zip(shared.emits.iter())
        .map(|((poll, decode_ns), (emit, channels))| TickRecord {
            channels: channels.clone(),
            latency_ns: emit.duration_since(*poll).as_nanos() as u64,
            decode_ns: *decode_ns,
        })
        .collect();
    let warmup = workload.warmup_ticks as usize;
    let first_measured_start = shared
        .polls
        .get(warmup)
        .map(|(at, _)| *at)
        .unwrap_or(finished);
    let last_end = shared.emits.last().map(|(at, _)| *at).unwrap_or(finished);
    let provenance = stats.loaded_checkpoint.as_ref();

    let summary = serde_json::json!({
        "run_wall_ns": finished.duration_since(started).as_nanos() as u64,
        "runtime_stats": {
            "ticks": stats.ticks,
            "accepted_batches": stats.accepted_batches,
            "rejected_batches": stats.rejected_batches,
            "receive_errors": stats.receive_errors,
            "emit_errors": stats.emit_errors,
            "dropped_spikes": stats.dropped_spikes,
            "diagnostic_emissions": stats.diagnostic_emissions,
            "suppressed_diagnostics": stats.suppressed_diagnostics,
            "last_batch_id": stats.last_batch_id,
        },
        "checkpoint": provenance.map(|p| serde_json::json!({
            "schema_id": p.schema_id,
            "model_id": p.model_id,
            "content_sha256": p.content_sha256,
            "source_path": p.source_path.display().to_string(),
        })),
        "stimulus_frames_sent": stimulus_frames_expected,
        "modulator_frames_sent": modulator_frames_expected,
        "health": serde_json::to_value(&snapshot)?,
        "ingress": {
            "sensory": class_metrics(&metrics.class(MessageClass::Sensory)),
            "reward": class_metrics(&metrics.class(MessageClass::Reward)),
            "control": class_metrics(&metrics.class(MessageClass::Control)),
            "telemetry": class_metrics(&metrics.class(MessageClass::Telemetry)),
        },
    });
    Ok(Run {
        records,
        first_measured_start,
        last_end,
        summary,
    })
}

/// Mirror of the daemon's `decode_inputs`: copy, pad/truncate, mask → 0,
/// positional modulator mapping, defaults when absent.
fn decode_event(event: Option<&Event>, stimuli: &mut [f32]) -> NeuroModulators {
    stimuli.fill(0.0);
    let Some(event) = event else {
        return NeuroModulators::default();
    };
    let upto = event.values.len().min(stimuli.len());
    stimuli[..upto].copy_from_slice(&event.values[..upto]);
    if let Some(mask) = event.valid_mask.as_ref() {
        for (idx, valid) in mask.iter().enumerate().take(stimuli.len()) {
            if !*valid {
                stimuli[idx] = 0.0;
            }
        }
    }
    match event.modulators.as_ref() {
        Some(m) => NeuroModulators {
            dopamine: m.dopamine,
            serotonin: m.cortisol,
            acetylcholine: m.acetylcholine,
            norepinephrine: m.tempo,
        },
        None => NeuroModulators::default(),
    }
}

fn run_baseline(cfg: DaemonConfig, workload: &Workload) -> Result<Run> {
    let (mut network, provenance) =
        brainstem_daemon::restore_network(&cfg).context("restore baseline network")?;
    let events = workload.expand();
    let mut stimuli = vec![0.0f32; cfg.channels];
    let mut records = Vec::with_capacity(workload.ticks as usize);
    let warmup = workload.warmup_ticks;
    let started = Instant::now();
    let mut first_measured_start = started;
    let mut last_end = started;
    for tick in 0..workload.ticks {
        let modulators = decode_event(events.get(&tick), &mut stimuli);
        let start = Instant::now();
        let ids = network
            .step(&stimuli, &modulators)
            .map_err(|e| anyhow::anyhow!("baseline step failed at tick {tick}: {e}"))?;
        let mut channels: Vec<u16> = ids.iter().filter_map(|&i| u16::try_from(i).ok()).collect();
        channels.sort_unstable();
        let end = Instant::now();
        if tick == warmup {
            first_measured_start = start;
        }
        last_end = end;
        records.push(TickRecord {
            channels,
            latency_ns: end.duration_since(start).as_nanos() as u64,
            decode_ns: None,
        });
    }
    let finished = Instant::now();
    let summary = serde_json::json!({
        "run_wall_ns": finished.duration_since(started).as_nanos() as u64,
        "checkpoint": {
            "schema_id": provenance.schema_id,
            "model_id": provenance.model_id,
            "content_sha256": provenance.content_sha256,
            "source_path": provenance.source_path.display().to_string(),
        },
        "stimulus_frames_sent": workload.cycles
            * workload.cycle_events.iter().filter(|e| !e.values.is_empty()).count() as u64,
        "modulator_frames_sent": workload.cycles
            * workload.cycle_events.iter().filter(|e| e.modulators.is_some()).count() as u64,
    });
    Ok(Run {
        records,
        first_measured_start,
        last_end,
        summary,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        bail!(
            "usage: {} <daemon-live|daemon-simulation|baseline> <config.toml> <workload.json> <out.jsonl> <summary.json>",
            args[0]
        );
    }
    let mode = args[1].as_str();
    let config_path = PathBuf::from(&args[2]);
    let workload_path = PathBuf::from(&args[3]);
    let records_path = PathBuf::from(&args[4]);
    let summary_path = PathBuf::from(&args[5]);

    let cfg = DaemonConfig::load(&config_path).context("load config")?;
    let workload = Workload::load(&workload_path)?;
    if cfg.channels != workload.channels {
        bail!(
            "config channels {} != workload channels {}",
            cfg.channels,
            workload.channels
        );
    }
    let fixture_sha256 = match cfg.runtime_mode {
        RuntimeMode::Live => {
            let digest = sha256_file(&cfg.model_path)?;
            if digest != workload.fixture_sha256 {
                bail!(
                    "fixture sha256 {digest} does not match workload fixture_sha256 {}",
                    workload.fixture_sha256
                );
            }
            Some(digest)
        }
        RuntimeMode::Simulation => None,
    };
    let expected_mode = match (mode, cfg.runtime_mode) {
        ("daemon-live", RuntimeMode::Live)
        | ("daemon-simulation", RuntimeMode::Simulation)
        | ("baseline", RuntimeMode::Live) => mode,
        _ => bail!(
            "mode {mode} does not match runtime_mode {:?}",
            cfg.runtime_mode
        ),
    };

    let run = match expected_mode {
        "baseline" => run_baseline(cfg, &workload)?,
        _ => run_daemon(cfg, &workload)?,
    };

    write_records(&records_path, &run.records)?;
    let mut summary = run.summary;
    summary["mode"] = serde_json::Value::String(mode.to_string());
    summary["config"] = serde_json::Value::String(config_path.display().to_string());
    summary["workload"] = serde_json::Value::String(workload_path.display().to_string());
    summary["fixture_sha256"] = serde_json::to_value(fixture_sha256)?;
    summary["timing"] = timing_summary(
        &run.records,
        workload.warmup_ticks as usize,
        run.first_measured_start,
        run.last_end,
    );
    summary["peak_rss_kb"] = serde_json::to_value(vm_status_kb("VmHWM:"))?;
    summary["rss_kb"] = serde_json::to_value(vm_status_kb("VmRSS:"))?;
    summary["pid"] = serde_json::to_value(std::process::id())?;
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary)? + "\n",
    )
    .with_context(|| format!("write {}", summary_path.display()))?;
    println!(
        "{mode}: {} ticks, {} nonempty, p?? see summary {}",
        run.records.len(),
        run.records
            .iter()
            .filter(|r| !r.channels.is_empty())
            .count(),
        summary_path.display()
    );
    Ok(())
}
