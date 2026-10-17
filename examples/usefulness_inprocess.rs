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
//! Requires `--features corpus-ipc`. Linux only (`/proc/self/status`).

mod usefulness_common;

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use brainstem_daemon::daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode, RuntimeStats};
use brainstem_daemon::ingress::{IngressPolicy, MessageClass, accept_ipc_json};
use brainstem_daemon::{
    BackendPair, ClassMetrics, IngressMetrics, IngressPacket, ModelProvenance, SpikeEvent,
    SpikeSink, StimulusSource,
};
use clap::{Parser, ValueEnum};
use corpus_ipc::{IpcMessage, NeuromodulatorSnapshot};
use neuromod::NeuroModulators;
use serde_json::{Value, json};
use usefulness_common::{
    Event, Modulators, Workload, now_ns, sha256_file, stimulus_frame, vm_status_kb, write_json,
};

const SESSION_ID: &str = "usefulness";

/// One tick as observed by the driver.
#[derive(Debug, Clone)]
struct TickRecord {
    channels: Vec<u16>,
    latency_ns: u64,
    decode_ns: Option<u64>,
}

/// Recorder shared by the source and the sink (neither is returned after the run).
#[derive(Default)]
struct Shared {
    polls: Vec<(Instant, Option<u64>)>,
    emits: Vec<(Instant, Vec<u16>)>,
    /// Observed producer-side counts, copied in by the source at shutdown.
    stimulus_frames: u64,
    modulator_frames: u64,
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

/// Decode through the public ingress path, merging a modulator frame into the
/// stimulus packet the way the ZMQ source attaches held modulators.
fn decode_frames(
    policy: &IngressPolicy,
    stimuli: Option<&[u8]>,
    modulators: Option<&[u8]>,
) -> Result<IngressPacket> {
    let now = now_ns();
    let mut packet = match stimuli {
        Some(bytes) => accept_ipc_json(bytes, policy, now)?,
        None => IngressPacket::default(),
    };
    if let Some(bytes) = modulators {
        packet.modulators = accept_ipc_json(bytes, policy, now)?.modulators;
    }
    Ok(packet)
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

impl WorkloadSource {
    fn record_poll(&self, at: Instant, decode_ns: Option<u64>) {
        self.shared.lock().unwrap().polls.push((at, decode_ns));
    }

    fn stimulus_frame(&mut self, event: &Event) -> Result<Option<Vec<u8>>> {
        if event.values.is_empty() {
            return Ok(None);
        }
        let batch_id = self.next_batch_id;
        self.next_batch_id += 1;
        self.stimulus_frames += 1;
        let frame = stimulus_frame(
            SESSION_ID,
            batch_id,
            event.values.clone(),
            event.valid_mask.clone(),
        )?;
        Ok(Some(frame))
    }

    fn modulator_frame(&mut self, event: &Event, tick: u64) -> Result<Option<Vec<u8>>> {
        let Some(mods) = event.modulators.as_ref() else {
            return Ok(None);
        };
        self.modulator_frames += 1;
        encode_modulators(mods, tick).map(Some)
    }
}

impl StimulusSource for WorkloadSource {
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
        let tick = self.tick;
        self.tick += 1;
        let Some(event) = self.events.get(&tick).cloned() else {
            self.record_poll(Instant::now(), None);
            return Ok(None);
        };
        let stimuli = self.stimulus_frame(&event)?;
        let modulators = self.modulator_frame(&event, tick)?;
        let decode_start = Instant::now();
        let packet = decode_frames(&self.policy, stimuli.as_deref(), modulators.as_deref())?;
        let poll = Instant::now();
        let decode_ns = poll.duration_since(decode_start).as_nanos() as u64;
        self.record_poll(poll, Some(decode_ns));
        Ok(Some(packet))
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        let mut shared = self.shared.lock().unwrap();
        shared.stimulus_frames = self.stimulus_frames;
        shared.modulator_frames = self.modulator_frames;
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
) -> Value {
    let measured = records.len().saturating_sub(warmup);
    let wall = last_end.duration_since(first_measured_start);
    let ticks_per_s = if wall.is_zero() {
        0.0
    } else {
        measured as f64 / wall.as_secs_f64()
    };
    json!({
        "ticks_total": records.len(),
        "warmup_ticks_excluded": warmup,
        "measured_ticks": measured,
        "measured_wall_ns": wall.as_nanos() as u64,
        "ticks_per_s": ticks_per_s,
    })
}

fn class_json(metrics: ClassMetrics) -> Value {
    json!({
        "accepted": metrics.accepted,
        "rejected": metrics.rejected,
        "dropped": metrics.dropped,
        "coalesced": metrics.coalesced,
        "shutdown_refused": metrics.shutdown_refused,
        "high_water_mark": metrics.high_water_mark,
    })
}

fn ingress_json(metrics: &IngressMetrics) -> Value {
    json!({
        "sensory": class_json(metrics.class(MessageClass::Sensory)),
        "reward": class_json(metrics.class(MessageClass::Reward)),
        "control": class_json(metrics.class(MessageClass::Control)),
        "telemetry": class_json(metrics.class(MessageClass::Telemetry)),
    })
}

fn runtime_stats_json(stats: &RuntimeStats) -> Value {
    json!({
        "ticks": stats.ticks,
        "accepted_batches": stats.accepted_batches,
        "rejected_batches": stats.rejected_batches,
        "receive_errors": stats.receive_errors,
        "emit_errors": stats.emit_errors,
        "dropped_spikes": stats.dropped_spikes,
        "diagnostic_emissions": stats.diagnostic_emissions,
        "suppressed_diagnostics": stats.suppressed_diagnostics,
        "last_batch_id": stats.last_batch_id,
    })
}

fn checkpoint_json(provenance: &ModelProvenance) -> Value {
    json!({
        "schema_id": provenance.schema_id,
        "model_id": provenance.model_id,
        "content_sha256": provenance.content_sha256,
        "source_path": provenance.source_path.display().to_string(),
    })
}

struct Run {
    records: Vec<TickRecord>,
    first_measured_start: Instant,
    last_end: Instant,
    summary: Value,
}

fn build_backend(cfg: &DaemonConfig, workload: &Workload) -> (BackendPair, Arc<Mutex<Shared>>) {
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
    let sink = RecordingSink {
        shared: Arc::clone(&shared),
    };
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    (pair, shared)
}

/// Pair each poll with its emit; a count mismatch means a tick was skipped or failed.
fn tick_records(shared: &Shared) -> Result<Vec<TickRecord>> {
    if shared.polls.len() != shared.emits.len() {
        bail!(
            "poll/emit count mismatch: {} polls, {} emits (a tick was skipped or failed)",
            shared.polls.len(),
            shared.emits.len()
        );
    }
    let records = shared
        .polls
        .iter()
        .zip(shared.emits.iter())
        .map(|((poll, decode_ns), (emit, channels))| TickRecord {
            channels: channels.clone(),
            latency_ns: emit.duration_since(*poll).as_nanos() as u64,
            decode_ns: *decode_ns,
        })
        .collect();
    Ok(records)
}

fn run_daemon(cfg: DaemonConfig, workload: &Workload) -> Result<Run> {
    let (pair, shared) = build_backend(&cfg, workload);
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
        .map_err(|_| anyhow!("backend still holds the shared recorder"))?
        .into_inner()
        .unwrap();
    let records = tick_records(&shared)?;
    let warmup = workload.warmup_ticks as usize;
    let first_measured_start = shared.polls.get(warmup).map_or(finished, |(at, _)| *at);
    let last_end = shared.emits.last().map_or(finished, |(at, _)| *at);
    let checkpoint = stats
        .loaded_checkpoint
        .as_ref()
        .map_or(Value::Null, checkpoint_json);
    let summary = json!({
        "run_wall_ns": finished.duration_since(started).as_nanos() as u64,
        "runtime_stats": runtime_stats_json(&stats),
        "checkpoint": checkpoint,
        "stimulus_frames_sent": shared.stimulus_frames,
        "modulator_frames_sent": shared.modulator_frames,
        "health": serde_json::to_value(&snapshot)?,
        "ingress": ingress_json(&metrics),
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
    event
        .modulators
        .as_ref()
        .map_or_else(NeuroModulators::default, |m| NeuroModulators {
            dopamine: m.dopamine,
            serotonin: m.cortisol,
            acetylcholine: m.acetylcholine,
            norepinephrine: m.tempo,
        })
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
            .map_err(|e| anyhow!("baseline step failed at tick {tick}: {e}"))?;
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
    let (stimulus_frames, modulator_frames) = workload.frame_counts();
    let summary = json!({
        "run_wall_ns": finished.duration_since(started).as_nanos() as u64,
        "checkpoint": checkpoint_json(&provenance),
        "stimulus_frames_sent": stimulus_frames,
        "modulator_frames_sent": modulator_frames,
    });
    Ok(Run {
        records,
        first_measured_start,
        last_end,
        summary,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    DaemonLive,
    DaemonSimulation,
    Baseline,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::DaemonLive => "daemon-live",
            Self::DaemonSimulation => "daemon-simulation",
            Self::Baseline => "baseline",
        }
    }

    /// Live modes need a live config; simulation needs a simulation config.
    fn runtime_mode(self) -> RuntimeMode {
        match self {
            Self::DaemonLive | Self::Baseline => RuntimeMode::Live,
            Self::DaemonSimulation => RuntimeMode::Simulation,
        }
    }
}

/// In-process usefulness experiment driver (daemon vs direct engine).
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to exercise.
    #[arg(value_enum)]
    mode: Mode,
    /// Daemon TOML configuration, loaded through `DaemonConfig::load`.
    config: PathBuf,
    /// Workload JSON from `experiments/usefulness/fixtures`.
    workload: PathBuf,
    /// Per-tick JSONL output.
    records: PathBuf,
    /// Run summary JSON output.
    summary: PathBuf,
}

/// Live runs pin the fixture digest to the workload; simulation has no fixture.
fn fixture_digest(cfg: &DaemonConfig, workload: &Workload) -> Result<Option<String>> {
    if cfg.runtime_mode == RuntimeMode::Simulation {
        return Ok(None);
    }
    let digest = sha256_file(&cfg.model_path)?;
    if digest != workload.fixture_sha256 {
        bail!(
            "fixture sha256 {digest} does not match workload fixture_sha256 {}",
            workload.fixture_sha256
        );
    }
    Ok(Some(digest))
}

fn write_summary(
    args: &Args,
    run: Run,
    fixture_sha256: Option<String>,
    warmup: usize,
) -> Result<()> {
    let mut summary = run.summary;
    summary["mode"] = Value::String(args.mode.as_str().to_string());
    summary["config"] = Value::String(args.config.display().to_string());
    summary["workload"] = Value::String(args.workload.display().to_string());
    summary["fixture_sha256"] = serde_json::to_value(fixture_sha256)?;
    summary["timing"] =
        timing_summary(&run.records, warmup, run.first_measured_start, run.last_end);
    summary["peak_rss_kb"] = serde_json::to_value(vm_status_kb(None, "VmHWM:"))?;
    summary["rss_kb"] = serde_json::to_value(vm_status_kb(None, "VmRSS:"))?;
    summary["pid"] = serde_json::to_value(std::process::id())?;
    write_json(&args.summary, &summary)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = DaemonConfig::load(&args.config).context("load config")?;
    let workload = Workload::load(&args.workload)?;
    workload.check_channels(cfg.channels)?;
    if cfg.runtime_mode != args.mode.runtime_mode() {
        bail!(
            "mode {} does not match runtime_mode {:?}",
            args.mode.as_str(),
            cfg.runtime_mode
        );
    }
    let fixture_sha256 = fixture_digest(&cfg, &workload)?;
    let run = match args.mode {
        Mode::Baseline => run_baseline(cfg, &workload)?,
        Mode::DaemonLive | Mode::DaemonSimulation => run_daemon(cfg, &workload)?,
    };
    write_records(&args.records, &run.records)?;
    let ticks = run.records.len();
    let nonempty = run
        .records
        .iter()
        .filter(|r| !r.channels.is_empty())
        .count();
    write_summary(&args, run, fixture_sha256, workload.warmup_ticks as usize)?;
    println!(
        "{}: {ticks} ticks, {nonempty} nonempty; summary {}",
        args.mode.as_str(),
        args.summary.display()
    );
    Ok(())
}
