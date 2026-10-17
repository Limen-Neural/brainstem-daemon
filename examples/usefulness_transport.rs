// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Transport driver for the usefulness experiment (`experiments/usefulness`).
//!
//! Launches the `corpus-ipc` `brainstem-daemon` binary (`--daemon`) with the
//! `transport-live` configuration, binds an external ZeroMQ producer on the
//! daemon's SUB endpoint, subscribes to the daemon PUB, waits for `/readyz`,
//! primes the PUB/SUB handshake, sends the measured pulses with fresh
//! timestamps, collects every output frame, reads `/metrics`, `/health` and
//! the daemon RSS, then stops the daemon with SIGTERM.
//!
//! Requires `--features corpus-ipc`. Linux only (`/proc/<pid>/status`, `sh`).

mod usefulness_common;

use std::fs;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use brainstem_daemon::daemon::DaemonConfig;
use clap::Parser;
use serde_json::{Value, json};
use usefulness_common::frames::{Frame, Phase, collect_frames};
use usefulness_common::process::{DaemonProcess, http_get};
use usefulness_common::{Workload, percentile, stimulus_frame, vm_status_kb, write_json};

const SUBSCRIBE_DEADLINE: Duration = Duration::from_secs(5);
const PRIME_ATTEMPTS: usize = 200;
const PRIME_WAIT: Duration = Duration::from_millis(100);
const TAIL_WAIT: Duration = Duration::from_millis(250);
const SESSION_ID: &str = "usefulness-transport";

struct Pulse {
    index: usize,
    channel: usize,
    sent_at: Instant,
}

/// Transport usefulness experiment driver (real binary over ZeroMQ).
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// The `corpus-ipc` `brainstem-daemon` binary to launch.
    #[arg(long)]
    daemon: PathBuf,
    /// Daemon TOML configuration (`transport-live.toml`).
    config: PathBuf,
    /// Workload JSON from `experiments/usefulness/fixtures`.
    workload: PathBuf,
    /// Per-frame JSONL output; the daemon log goes next to it.
    records: PathBuf,
    /// Run summary JSON output.
    summary: PathBuf,
}

/// Everything the run needs that is derived from the config and workload.
struct Session {
    channels: usize,
    amplitude: f32,
    pulses: usize,
    spacing: Duration,
    expected: Vec<Vec<u16>>,
}

impl Session {
    fn new(cfg: &DaemonConfig, workload: &Workload) -> Result<Self> {
        workload.check_channels(cfg.channels)?;
        let transport = &workload.transport;
        let spacing_ns =
            1_000_000_000 * transport.pulse_spacing_ticks / u64::from(cfg.tick_rate_hz);
        if spacing_ns == 0 {
            bail!("pulse spacing rounds to zero at {} Hz", cfg.tick_rate_hz);
        }
        let expected = (0..cfg.channels)
            .map(|c| {
                transport
                    .expected_by_channel
                    .get(&c.to_string())
                    .cloned()
                    .with_context(|| format!("expected_by_channel missing channel {c}"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            channels: cfg.channels,
            amplitude: transport.amplitude,
            pulses: usize::try_from(transport.pulses)?,
            spacing: Duration::from_nanos(spacing_ns),
            expected,
        })
    }

    fn frame(&self, channel: usize, batch_id: u64) -> Result<Vec<u8>> {
        let mut values = vec![0.0f32; self.channels];
        values[channel] = self.amplitude;
        stimulus_frame(
            SESSION_ID,
            batch_id,
            values,
            Some(vec![true; self.channels]),
        )
    }
}

struct Sockets {
    producer: zmq::Socket,
    sub: zmq::Socket,
}

/// Producer PUB bound where the daemon SUB connects; consumer SUB on the daemon PUB.
fn open_sockets(cfg: &DaemonConfig) -> Result<Sockets> {
    let context = zmq::Context::new();
    let producer = context.socket(zmq::PUB)?;
    producer.set_linger(0)?;
    producer.set_sndhwm(1000)?;
    producer.bind(&format!("tcp://127.0.0.1:{}", cfg.spine_sub_port))?;
    let sub = context.socket(zmq::SUB)?;
    sub.set_linger(0)?;
    sub.set_rcvhwm(65_536)?;
    sub.set_subscribe(b"")?;
    sub.connect(&format!("tcp://127.0.0.1:{}", cfg.spine_pub_port))?;
    Ok(Sockets { producer, sub })
}

/// A received `Spikes` frame (empty allowed) proves the subscription;
/// readiness and local send counts do not.
fn confirm_subscription(sub: &zmq::Socket, frames: &mut Vec<Frame>) -> Result<()> {
    collect_frames(sub, SUBSCRIBE_DEADLINE, Phase::Subscribe, frames)?;
    if frames.is_empty() {
        bail!("no PUB frame received within {SUBSCRIBE_DEADLINE:?}; subscription not confirmed");
    }
    Ok(())
}

/// Send channel-0 pulses until the expected nonempty output arrives.
/// Returns the attempts used; priming frames are excluded from measurement.
fn prime(
    session: &Session,
    sockets: &Sockets,
    frames: &mut Vec<Frame>,
    batch_id: &mut u64,
) -> Result<usize> {
    for attempt in 1..=PRIME_ATTEMPTS {
        sockets.producer.send(session.frame(0, *batch_id)?, 0)?;
        *batch_id += 1;
        let before = frames.len();
        collect_frames(&sockets.sub, PRIME_WAIT, Phase::Prime, frames)?;
        if frames[before..]
            .iter()
            .any(|f| f.channels == session.expected[0])
        {
            // Let the response settle, then drain so measurement starts clean.
            collect_frames(&sockets.sub, session.spacing * 2, Phase::Prime, frames)?;
            return Ok(attempt);
        }
    }
    bail!("priming failed: no expected nonempty output after {PRIME_ATTEMPTS} attempts")
}

/// Send the measured pulses on a fixed cadence, collecting output between sends.
fn measure(
    session: &Session,
    sockets: &Sockets,
    frames: &mut Vec<Frame>,
    batch_id: &mut u64,
) -> Result<Vec<Pulse>> {
    let mut pulses = Vec::with_capacity(session.pulses);
    let start = Instant::now();
    for index in 0..session.pulses {
        let channel = index % session.channels;
        let payload = session.frame(channel, *batch_id)?;
        *batch_id += 1;
        let sent_at = Instant::now();
        sockets.producer.send(payload, 0)?;
        pulses.push(Pulse {
            index,
            channel,
            sent_at,
        });
        let next = start + session.spacing * u32::try_from(index + 1)?;
        let wait = next.saturating_duration_since(Instant::now());
        collect_frames(&sockets.sub, wait, Phase::Measure, frames)?;
    }
    collect_frames(&sockets.sub, TAIL_WAIT, Phase::Tail, frames)?;
    Ok(pulses)
}

#[derive(Default)]
struct Outcome {
    latencies_ns: Vec<u64>,
    matched: u64,
    mismatched: u64,
    per_pulse: Vec<Value>,
}

impl Outcome {
    fn answered(&mut self, pulse: &Pulse, frame: &Frame, want: &[u16]) {
        let matched = frame.channels == want;
        if matched {
            self.matched += 1;
        } else {
            self.mismatched += 1;
        }
        let latency = frame
            .recv
            .saturating_duration_since(pulse.sent_at)
            .as_nanos() as u64;
        self.latencies_ns.push(latency);
        self.per_pulse.push(json!({
            "pulse": pulse.index, "channel": pulse.channel, "matched": matched,
            "latency_ns": latency, "received": frame.channels,
        }));
    }

    fn unanswered(&mut self, pulse: &Pulse) {
        self.per_pulse.push(json!({
            "pulse": pulse.index, "channel": pulse.channel, "matched": false, "latency_ns": null,
        }));
    }
}

/// Correlate by order only: the i-th nonempty measured frame answers the i-th
/// pulse (input batch ids are not propagated by the daemon).
fn correlate(pulses: &[Pulse], frames: &[Frame], expected: &[Vec<u16>]) -> (Outcome, usize) {
    let nonempty: Vec<&Frame> = frames.iter().filter(|f| f.measured_nonempty()).collect();
    let mut outcome = Outcome::default();
    for pulse in pulses {
        match nonempty.get(pulse.index) {
            Some(frame) => outcome.answered(pulse, frame, &expected[pulse.channel]),
            None => outcome.unanswered(pulse),
        }
    }
    (outcome, nonempty.len())
}

/// Which pulse each measured nonempty frame answers, by order.
fn pulse_assignment(frames: &[Frame], pulses: usize) -> Vec<Option<usize>> {
    let mut seen = 0usize;
    frames
        .iter()
        .map(|frame| {
            if !frame.measured_nonempty() {
                return None;
            }
            let index = seen;
            seen += 1;
            (index < pulses).then_some(index)
        })
        .collect()
}

fn write_frames(path: &Path, frames: &[Frame], pulses: &[Pulse], started: Instant) -> Result<()> {
    let assignment = pulse_assignment(frames, pulses.len());
    let file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    for (seq, (frame, pulse)) in frames.iter().zip(assignment).enumerate() {
        let send_ns = pulse.map(|i| pulses[i].sent_at.duration_since(started).as_nanos() as u64);
        let record = json!({
            "seq": seq,
            "phase": frame.phase.as_str(),
            "recv_ns": frame.recv.duration_since(started).as_nanos() as u64,
            "channels": frame.channels,
            "batch_id": frame.batch_id,
            "timestamp": frame.timestamp,
            "pulse": pulse,
            "send_ns": send_ns,
        });
        writeln!(out, "{record}")?;
    }
    out.flush()?;
    Ok(())
}

/// Control-surface and `/proc` observations taken before shutdown.
struct Probes {
    metrics: Option<(u16, String)>,
    health: Option<(u16, String)>,
    rss_kb: Option<u64>,
    peak_rss_kb: Option<u64>,
}

fn probe_daemon(control: SocketAddr, pid: u32) -> Result<Probes> {
    Ok(Probes {
        metrics: http_get(control, "/metrics")?,
        health: http_get(control, "/health")?,
        rss_kb: vm_status_kb(Some(pid), "VmRSS:"),
        peak_rss_kb: vm_status_kb(Some(pid), "VmHWM:"),
    })
}

/// Loaded inputs plus the running, ready daemon and the sockets around it.
struct Setup {
    args: Args,
    workload: Workload,
    session: Session,
    control: SocketAddr,
    log_path: PathBuf,
    sockets: Sockets,
    daemon: DaemonProcess,
    ready_after: Duration,
}

impl Setup {
    fn new(args: Args) -> Result<Self> {
        let cfg = DaemonConfig::load(&args.config).context("load config")?;
        let workload = Workload::load(&args.workload)?;
        let session = Session::new(&cfg, &workload)?;
        let control: SocketAddr = cfg
            .control_bind
            .as_deref()
            .context("transport config needs control_bind")?
            .parse()?;
        let log_path = args.records.with_extension("daemon.log");
        let sockets = open_sockets(&cfg)?;
        let mut daemon = DaemonProcess::spawn(&args.daemon, &args.config, &log_path)?;
        let ready_after = daemon.wait_ready(control)?;
        Ok(Self {
            args,
            workload,
            session,
            control,
            log_path,
            sockets,
            daemon,
            ready_after,
        })
    }
}

/// What one run produced, from subscription through SIGTERM.
struct Capture {
    pid: u32,
    frames: Vec<Frame>,
    pulses: Vec<Pulse>,
    prime_attempts: usize,
    probes: Probes,
    exit: (Duration, Option<i32>),
    termination_logged: bool,
}

fn capture(setup: &mut Setup) -> Result<Capture> {
    let pid = setup.daemon.pid();
    let mut frames: Vec<Frame> = Vec::with_capacity(16_384);
    confirm_subscription(&setup.sockets.sub, &mut frames)?;
    let mut batch_id = 1u64;
    let prime_attempts = prime(&setup.session, &setup.sockets, &mut frames, &mut batch_id)?;
    let pulses = measure(&setup.session, &setup.sockets, &mut frames, &mut batch_id)?;
    let probes = probe_daemon(setup.control, pid)?;
    let exit = setup.daemon.terminate()?;
    let daemon_log = fs::read_to_string(&setup.log_path).unwrap_or_default();
    Ok(Capture {
        pid,
        frames,
        pulses,
        prime_attempts,
        probes,
        exit,
        termination_logged: daemon_log.contains("Termination signal received"),
    })
}

fn count_frames(frames: &[Frame], pred: impl Fn(&Frame) -> bool) -> usize {
    frames.iter().filter(|f| pred(f)).count()
}

fn latency_json(latencies_ns: &[u64]) -> Value {
    let mut sorted = latencies_ns.to_vec();
    sorted.sort_unstable();
    json!({
        "count": sorted.len(),
        "p50": percentile(&sorted, 0.50),
        "p99": percentile(&sorted, 0.99),
        "max": sorted.last().copied(),
        "min": sorted.first().copied(),
    })
}

fn summary_json(setup: &Setup, capture: &Capture, outcome: &Outcome, nonempty: usize) -> Value {
    let frames = &capture.frames;
    let sent = capture.pulses.len() as u64;
    let probes = &capture.probes;
    json!({
        "mode": "transport-live",
        "config": setup.args.config.display().to_string(),
        "workload": setup.args.workload.display().to_string(),
        "fixture_sha256": setup.workload.fixture_sha256,
        "daemon_binary": setup.args.daemon.display().to_string(),
        "daemon_pid": capture.pid,
        "ready_after_ns": setup.ready_after.as_nanos() as u64,
        "subscribe_frames": count_frames(frames, |f| f.phase == Phase::Subscribe),
        "prime_attempts": capture.prime_attempts,
        "prime_frames": count_frames(frames, |f| f.phase == Phase::Prime),
        "pulse_spacing_ns": setup.session.spacing.as_nanos() as u64,
        "pulses_sent": sent,
        "frames_received_total": frames.len(),
        "measured_frames_total": count_frames(frames, |f| f.phase.measured()),
        "measured_frames_empty": count_frames(frames, |f| f.phase.measured() && f.channels.is_empty()),
        "measured_frames_nonempty": nonempty,
        "matched": outcome.matched,
        "mismatched": outcome.mismatched,
        "lost": sent - outcome.matched - outcome.mismatched,
        "extra_nonempty": nonempty.saturating_sub(capture.pulses.len()) as u64,
        "latency_ns": latency_json(&outcome.latencies_ns),
        "per_pulse": outcome.per_pulse,
        "daemon_rss_kb": probes.rss_kb,
        "daemon_peak_rss_kb": probes.peak_rss_kb,
        "metrics_status": probes.metrics.as_ref().map(|(s, _)| *s),
        "metrics_text": probes.metrics.as_ref().map(|(_, b)| b.clone()),
        "health_status": probes.health.as_ref().map(|(s, _)| *s),
        "health_json": probes.health.as_ref().and_then(|(_, b)| serde_json::from_str::<Value>(b).ok()),
        "sigterm_exit_ns": capture.exit.0.as_nanos() as u64,
        "exit_code": capture.exit.1,
        "termination_logged": capture.termination_logged,
        "daemon_log": setup.log_path.display().to_string(),
    })
}

fn main() -> Result<()> {
    let mut setup = Setup::new(Args::parse())?;
    let capture = capture(&mut setup)?;
    let (outcome, nonempty) = correlate(&capture.pulses, &capture.frames, &setup.session.expected);
    write_frames(
        &setup.args.records,
        &capture.frames,
        &capture.pulses,
        setup.daemon.spawned_at,
    )?;
    let summary = summary_json(&setup, &capture, &outcome, nonempty);
    write_json(&setup.args.summary, &summary)?;
    println!(
        "transport-live: sent {} matched {} mismatched {} lost {}; exit {:?} after {:?}",
        capture.pulses.len(),
        outcome.matched,
        outcome.mismatched,
        summary["lost"],
        capture.exit.1,
        capture.exit.0
    );
    Ok(())
}
