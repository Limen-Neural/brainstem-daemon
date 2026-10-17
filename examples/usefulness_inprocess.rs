// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Usefulness experiment: in-process daemon vs direct-engine baseline.
//!
//! See `experiments/usefulness/PROTOCOL.md`. Run from the repository root:
//!
//! ```text
//! cargo run --release --locked --features corpus-ipc --example usefulness_inprocess -- \
//!     --mode daemon-live --variant main --rep 0 --out <dir>
//! ```
//!
//! Modes: `daemon-live`, `daemon-simulation`, `baseline`.
//! Writes `<mode>-<variant>-r<rep>.jsonl` (one record per tick) and a matching
//! `.summary.json`. Wall-clock spike fields are not recorded (excluded from equality).

#[path = "usefulness_common/mod.rs"]
mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, bail};
use brainstem_daemon::ingress::{IngressPolicy, accept_ipc_json};
use brainstem_daemon::{
    BackendPair, BrainstemDaemon, ClassMetrics, DaemonConfig, HealthHandle, IngressPacket,
    MessageClass, ModelProvenance, RuntimeStats, SpikeEvent, SpikeSink, StimulusSource,
    restore_network,
};
use clap::{Parser, ValueEnum};
use neuromod::NeuroModulators;
use serde_json::{Value, json};

use common::{TickInput, Variant, Workload};

const LIVE_CONFIG: &str = "experiments/usefulness/config/live.toml";
const SIMULATION_CONFIG: &str = "experiments/usefulness/config/simulation.toml";

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
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

    fn config_path(self) -> &'static Path {
        match self {
            Self::DaemonSimulation => Path::new(SIMULATION_CONFIG),
            Self::DaemonLive | Self::Baseline => Path::new(LIVE_CONFIG),
        }
    }
}

/// Command-line arguments (the runner passes fixed values; nothing is security-relevant).
#[derive(Parser)]
#[command(about = "Usefulness experiment: in-process daemon vs direct-engine baseline")]
struct Args {
    #[arg(long, value_enum)]
    mode: Mode,
    #[arg(long, value_enum, default_value = "main")]
    variant: Variant,
    #[arg(long, default_value_t = 0)]
    rep: u32,
    #[arg(long)]
    out: PathBuf,
}

impl Args {
    fn stem(&self) -> String {
        format!(
            "{}-{}-r{}",
            self.mode.as_str(),
            self.variant.as_str(),
            self.rep
        )
    }
}

/// Per-tick timing and output, shared with the boxed source/sink because the
/// daemon does not return its backend after `run_for_ticks`.
#[derive(Default)]
struct Trace {
    polls: Vec<Instant>,
    emits: Vec<(Instant, Vec<u16>)>,
}

impl Trace {
    fn with_capacity(ticks: usize) -> Self {
        Self {
            polls: Vec::with_capacity(ticks),
            emits: Vec::with_capacity(ticks),
        }
    }
}

/// Source that encodes each workload tick as typed `corpus-ipc` frames (producer
/// side, untimed) and decodes them through the public ingress path (timed).
struct WorkloadSource {
    inputs: Vec<TickInput>,
    next: usize,
    policy: IngressPolicy,
    trace: Arc<Mutex<Trace>>,
}

/// Encoded frames for one tick: the stimulus batch plus an optional modulator snapshot.
struct Frames {
    stimulus: Vec<u8>,
    modulators: Option<Vec<u8>>,
}

fn encode_frames(tick: u64, input: &TickInput) -> Result<Frames> {
    let stimulus = common::stimulus_frame(tick, &input.values, input.valid_mask.as_deref())?;
    let modulators = input
        .modulators
        .map(|m| common::modulator_frame(tick as i64, m))
        .transpose()?;
    Ok(Frames {
        stimulus,
        modulators,
    })
}

fn decode_frames(frames: &Frames, policy: &IngressPolicy) -> Result<IngressPacket> {
    let now = common::now_ns();
    let mut packet = accept_ipc_json(&frames.stimulus, policy, now)
        .map_err(|e| anyhow::anyhow!("stimulus frame rejected: {e}"))?;
    if let Some(frame) = &frames.modulators {
        let mod_packet = accept_ipc_json(frame, policy, now)
            .map_err(|e| anyhow::anyhow!("modulator frame rejected: {e}"))?;
        packet.modulators = mod_packet.modulators;
    }
    Ok(packet)
}

impl StimulusSource for WorkloadSource {
    fn next_ingress(&mut self) -> Result<Option<IngressPacket>> {
        let Some(input) = self.inputs.get(self.next) else {
            bail!("workload exhausted at tick {}", self.next);
        };
        // Producer work: encode with a fresh timestamp (not part of daemon latency).
        let frames = encode_frames(self.next as u64, input)?;
        self.next += 1;
        self.trace.lock().unwrap().polls.push(Instant::now());
        decode_frames(&frames, &self.policy).map(Some)
    }

    fn initialize(&mut self, _model_path: Option<&str>) -> Result<()> {
        Ok(())
    }
}

struct TraceSink {
    trace: Arc<Mutex<Trace>>,
}

impl SpikeSink for TraceSink {
    fn emit(&mut self, spikes: &[SpikeEvent], _batch_time: std::time::Duration) -> Result<()> {
        let at = Instant::now();
        let channels = spikes.iter().map(|s| s.channel).collect();
        self.trace.lock().unwrap().emits.push((at, channels));
        Ok(())
    }
}

fn class_json(m: ClassMetrics) -> Value {
    json!({
        "accepted": m.accepted,
        "rejected": m.rejected,
        "dropped": m.dropped,
        "coalesced": m.coalesced,
        "shutdown_refused": m.shutdown_refused,
        "lost_or_coalesced": m.lost_or_coalesced(),
        "high_water_mark": m.high_water_mark,
    })
}

fn provenance_json(p: &ModelProvenance) -> Value {
    json!({
        "schema_id": p.schema_id,
        "source_path": p.source_path.display().to_string(),
        "content_sha256": p.content_sha256,
        "model_id": p.model_id,
    })
}

fn stats_json(stats: &RuntimeStats) -> Value {
    json!({
        "ticks": stats.ticks,
        "accepted_batches": stats.accepted_batches,
        "rejected_batches": stats.rejected_batches,
        "receive_errors": stats.receive_errors,
        "emit_errors": stats.emit_errors,
        "dropped_spikes": stats.dropped_spikes,
        "diagnostic_emissions": stats.diagnostic_emissions,
        "suppressed_diagnostics": stats.suppressed_diagnostics,
    })
}

fn ingress_json(ingress: &brainstem_daemon::BoundedIngress) -> Value {
    let metrics = ingress.metrics();
    json!({
        "sensory": class_json(metrics.class(MessageClass::Sensory)),
        "reward": class_json(metrics.class(MessageClass::Reward)),
        "control": class_json(metrics.class(MessageClass::Control)),
        "telemetry": class_json(metrics.class(MessageClass::Telemetry)),
    })
}

struct RunOutput {
    trace: Trace,
    extra: Value,
}

fn trace_backend(cfg: &DaemonConfig, inputs: Vec<TickInput>) -> (BackendPair, Arc<Mutex<Trace>>) {
    let trace = Arc::new(Mutex::new(Trace::with_capacity(inputs.len())));
    let source = WorkloadSource {
        inputs,
        next: 0,
        policy: IngressPolicy::new(cfg.channels, Some(std::time::Duration::from_secs(1))),
        trace: Arc::clone(&trace),
    };
    let sink = TraceSink {
        trace: Arc::clone(&trace),
    };
    let pair = BackendPair {
        source: Box::new(source),
        sink: Box::new(sink),
    };
    (pair, trace)
}

fn daemon_details(stats: &RuntimeStats, health: &HealthHandle, ingress: Value) -> Result<Value> {
    Ok(json!({
        "runtime_stats": stats_json(stats),
        "checkpoint": stats.loaded_checkpoint.as_ref().map(provenance_json),
        "ingress": ingress,
        "health": serde_json::to_value(health.snapshot())?,
    }))
}

fn run_daemon(cfg: DaemonConfig, inputs: Vec<TickInput>) -> Result<RunOutput> {
    let ticks = inputs.len() as u64;
    let (pair, trace) = trace_backend(&cfg, inputs);
    let daemon = BrainstemDaemon::try_with_backend(cfg, pair)?;
    let health = daemon.health();
    let ingress = daemon.ingress();
    let stats = daemon.run_for_ticks(ticks)?;
    let extra = daemon_details(&stats, &health, ingress_json(&ingress))?;
    let trace = Arc::try_unwrap(trace)
        .map_err(|_| anyhow::anyhow!("trace still shared after run"))?
        .into_inner()
        .unwrap();
    Ok(RunOutput { trace, extra })
}

/// Same decoding the daemon documents: mask -> 0, width pad/truncate, and the
/// positional DA/cortisol/ACh/tempo -> DA/5-HT/ACh/NE mapping (defaults when absent).
fn baseline_inputs(input: &TickInput, channels: usize) -> (Vec<f32>, NeuroModulators) {
    let mut stimuli = vec![0.0; channels];
    let upto = input.values.len().min(channels);
    stimuli[..upto].copy_from_slice(&input.values[..upto]);
    let mask = input.valid_mask.as_deref().unwrap_or(&[]);
    for (slot, _) in stimuli.iter_mut().zip(mask).filter(|(_, valid)| !**valid) {
        *slot = 0.0;
    }
    let modulators = input
        .modulators
        .map(|m| NeuroModulators {
            dopamine: m.dopamine,
            serotonin: m.cortisol,
            acetylcholine: m.acetylcholine,
            norepinephrine: m.tempo,
        })
        .unwrap_or_default();
    (stimuli, modulators)
}

/// Convert spike IDs to `u16` channels, counting IDs that do not fit.
fn spike_channels(spikes: Vec<usize>, dropped: &mut u64) -> Vec<u16> {
    let total = spikes.len();
    let channels: Vec<u16> = spikes
        .into_iter()
        .filter_map(|id| u16::try_from(id).ok())
        .collect();
    *dropped += (total - channels.len()) as u64;
    channels
}

fn run_baseline(cfg: DaemonConfig, inputs: Vec<TickInput>) -> Result<RunOutput> {
    // Shared loader: the same public restore path the daemon uses (reward_lr = 0).
    let (mut network, provenance) = restore_network(&cfg)?;
    let decoded: Vec<_> = inputs
        .iter()
        .map(|i| baseline_inputs(i, cfg.channels))
        .collect();
    let mut trace = Trace::with_capacity(inputs.len());
    let mut dropped = 0u64;
    for (stimuli, modulators) in &decoded {
        let poll = Instant::now();
        let channels = spike_channels(network.step(stimuli, modulators)?, &mut dropped);
        let at = Instant::now();
        trace.polls.push(poll);
        trace.emits.push((at, channels));
    }
    let extra = json!({
        "baseline_steps": decoded.len(),
        "dropped_spikes": dropped,
        "checkpoint": provenance_json(&provenance),
    });
    Ok(RunOutput { trace, extra })
}

/// Everything the output writer needs besides the trace.
struct RunContext {
    args: Args,
    workload: Workload,
    inputs: Vec<TickInput>,
    started: Instant,
}

fn write_trace(path: &Path, trace: &Trace, inputs: &[TickInput]) -> Result<()> {
    if trace.polls.len() != trace.emits.len() {
        bail!(
            "trace mismatch: {} polls vs {} emits",
            trace.polls.len(),
            trace.emits.len()
        );
    }
    let mut jsonl = common::jsonl_writer(path)?;
    let records = trace.polls.iter().zip(&trace.emits).zip(inputs);
    for (tick, ((poll, (emit, channels)), input)) in records.enumerate() {
        let record = json!({
            "tick": tick,
            "segment": input.segment,
            "channels": channels,
            "latency_ns": emit.duration_since(*poll).as_nanos() as u64,
        });
        serde_json::to_writer(&mut jsonl, &record)?;
        writeln!(jsonl)?;
    }
    jsonl.flush()?;
    Ok(())
}

/// Post-warm-up span from the first measured poll to the last emit, in ns.
fn measured_span_ns(trace: &Trace, warm: usize) -> u64 {
    match (trace.polls.get(warm), trace.emits.last()) {
        (Some(start), Some((end, _))) => end.duration_since(*start).as_nanos() as u64,
        _ => 0,
    }
}

fn summary_json(ctx: &RunContext, trace: &Trace, extra: Value) -> Result<Value> {
    let warm = ctx.workload.warmup_ticks();
    let measured_ns = measured_span_ns(trace, warm);
    let measured_ticks = trace.emits.len().saturating_sub(warm);
    let ticks_per_s = match measured_ns {
        0 => 0.0,
        ns => measured_ticks as f64 * 1e9 / ns as f64,
    };
    let (vm_hwm_kb, vm_rss_kb) = common::proc_memory_kb("/proc/self/status");
    Ok(json!({
        "mode": ctx.args.mode.as_str(),
        "variant": ctx.args.variant.as_str(),
        "rep": ctx.args.rep,
        "config": ctx.args.mode.config_path().display().to_string(),
        "workload_version": ctx.workload.workload_version,
        "fixture_sha256": common::file_sha256(common::FIXTURE_PATH)?,
        "ticks_requested": ctx.inputs.len(),
        "ticks_recorded": trace.emits.len(),
        "warmup_ticks_excluded": warm,
        "measured_ticks": measured_ticks,
        "measured_span_ns": measured_ns,
        "ticks_per_s": ticks_per_s,
        "process_wall_ns": ctx.started.elapsed().as_nanos() as u64,
        "vm_hwm_kb": vm_hwm_kb,
        "vm_rss_kb": vm_rss_kb,
        "details": extra,
    }))
}

fn write_outputs(ctx: &RunContext, output: RunOutput) -> Result<()> {
    let stem = ctx.args.stem();
    let out = &ctx.args.out;
    write_trace(
        &out.join(format!("{stem}.jsonl")),
        &output.trace,
        &ctx.inputs,
    )?;
    let summary = summary_json(ctx, &output.trace, output.extra)?;
    common::write_json(&out.join(format!("{stem}.summary.json")), &summary)
}

fn main() -> Result<()> {
    let started = Instant::now();
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let workload = Workload::load()?;
    let config_path = args.mode.config_path();
    let cfg = DaemonConfig::load(config_path)?;
    if cfg.channels != workload.channels {
        bail!(
            "config channels {} != workload channels {}",
            cfg.channels,
            workload.channels
        );
    }
    let inputs = workload.expand(args.variant);
    let output = match args.mode {
        Mode::DaemonLive | Mode::DaemonSimulation => run_daemon(cfg, inputs.clone())?,
        Mode::Baseline => run_baseline(cfg, inputs.clone())?,
    };
    let ctx = RunContext {
        args,
        workload,
        inputs,
        started,
    };
    write_outputs(&ctx, output)
}
