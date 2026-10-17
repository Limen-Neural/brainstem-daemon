// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Transport driver for the usefulness experiment (`experiments/usefulness`).
//!
//! Launches the `corpus-ipc` `brainstem-daemon` binary built next to this
//! example (`target/<profile>/brainstem-daemon`) with the `transport-live`
//! configuration, binds an external ZeroMQ producer on the daemon's SUB
//! endpoint, subscribes to the daemon PUB, waits for `/readyz`, primes the
//! PUB/SUB handshake, sends the measured pulses with fresh timestamps,
//! collects every output frame, reads `/metrics`, `/health` and the daemon
//! RSS, then stops the daemon with SIGTERM.
//!
//! Usage: `usefulness_transport <config.toml> <workload.json> <out.jsonl> <summary.json>`
//!
//! Requires `--features corpus-ipc`. Linux only (`/proc/<pid>/status`, `sh`).

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use brainstem_daemon::daemon::DaemonConfig;
use brainstem_daemon::ingress::STIMULUS_SCHEMA;
use corpus_ipc::{BatchMetadata, IpcMessage, StimulusBatch};
use serde::Deserialize;
use serde_json::{Value, json};

const READY_DEADLINE: Duration = Duration::from_secs(10);
const EXIT_DEADLINE: Duration = Duration::from_secs(10);
const SUBSCRIBE_DEADLINE: Duration = Duration::from_secs(5);
const PRIME_ATTEMPTS: usize = 200;
const PRIME_WAIT: Duration = Duration::from_millis(100);
const TAIL_WAIT: Duration = Duration::from_millis(250);
const HTTP_TIMEOUT: Duration = Duration::from_millis(200);
const SESSION_ID: &str = "usefulness-transport";

#[derive(Debug, Deserialize)]
struct Transport {
    pulse_spacing_ticks: u64,
    pulses: u64,
    amplitude: f32,
    expected_by_channel: HashMap<String, Vec<u16>>,
}

#[derive(Debug, Deserialize)]
struct Workload {
    fixture_sha256: String,
    channels: usize,
    transport: Transport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Subscribe,
    Prime,
    Measure,
    Tail,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Subscribe => "subscribe",
            Self::Prime => "prime",
            Self::Measure => "measure",
            Self::Tail => "tail",
        }
    }

    /// Frames that count toward the measurement (measured pulses and their tail).
    fn measured(self) -> bool {
        matches!(self, Self::Measure | Self::Tail)
    }
}

#[derive(Debug)]
struct Frame {
    phase: Phase,
    recv: Instant,
    channels: Vec<u16>,
    batch_id: u64,
    timestamp: u64,
}

impl Frame {
    fn measured_nonempty(&self) -> bool {
        self.phase.measured() && !self.channels.is_empty()
    }
}

struct Pulse {
    index: usize,
    channel: usize,
    sent_at: Instant,
}

struct Args {
    config: PathBuf,
    workload: PathBuf,
    records: PathBuf,
    summary: PathBuf,
}

fn parse_args() -> Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        bail!(
            "usage: {} <config.toml> <workload.json> <out.jsonl> <summary.json>",
            args[0]
        );
    }
    Ok(Args {
        config: PathBuf::from(&args[1]),
        workload: PathBuf::from(&args[2]),
        records: PathBuf::from(&args[3]),
        summary: PathBuf::from(&args[4]),
    })
}

/// The daemon binary built alongside this example:
/// `target/<profile>/examples/usefulness_transport` → `target/<profile>/brainstem-daemon`.
fn daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .context("example path has no target directory")?;
    let binary = profile_dir.join("brainstem-daemon");
    if !binary.is_file() {
        bail!(
            "daemon binary not found at {} (build with --features corpus-ipc --bin brainstem-daemon)",
            binary.display()
        );
    }
    Ok(binary)
}

struct Daemon {
    child: Option<Child>,
    spawned_at: Instant,
}

impl Daemon {
    fn spawn(binary: &Path, config: &Path, log_path: &Path) -> Result<Self> {
        // tracing-subscriber writes to stdout; capture both streams in one file.
        let log = fs::File::create(log_path)
            .with_context(|| format!("create daemon log {}", log_path.display()))?;
        let log_err = log.try_clone()?;
        let child = Command::new(binary)
            .arg("--config")
            .arg(config)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .with_context(|| format!("spawn {}", binary.display()))?;
        Ok(Self {
            child: Some(child),
            spawned_at: Instant::now(),
        })
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    fn exited(&mut self) -> Result<bool> {
        Ok(self
            .child
            .as_mut()
            .context("daemon already reaped")?
            .try_wait()?
            .is_some())
    }

    fn ready(&mut self, control: SocketAddr) -> Result<bool> {
        if self.exited()? {
            bail!("daemon exited before becoming ready");
        }
        Ok(http_get(control, "/readyz")?.is_some_and(|(status, _)| status == 200))
    }

    fn wait_ready(&mut self, control: SocketAddr) -> Result<Duration> {
        let deadline = Instant::now() + READY_DEADLINE;
        while !self.ready(control)? {
            if Instant::now() > deadline {
                bail!("daemon never reported ready on {control}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(self.spawned_at.elapsed())
    }

    /// SIGTERM through the shell builtin (minimal images have no `kill`
    /// binary). The pid is a positional parameter, never part of the script.
    fn send_sigterm(&self) -> Result<()> {
        let pid = self.pid().to_string();
        let status = Command::new("sh")
            .args(["-c", r#"kill -TERM "$1""#, "sigterm", &pid])
            .status()
            .context("run sh kill")?;
        if !status.success() {
            bail!("kill -TERM {pid} failed: {status}");
        }
        Ok(())
    }

    fn terminate(&mut self) -> Result<(Duration, Option<i32>)> {
        let sent = Instant::now();
        self.send_sigterm()?;
        let deadline = sent + EXIT_DEADLINE;
        while !self.exited()? {
            if Instant::now() > deadline {
                bail!("daemon did not exit within {EXIT_DEADLINE:?} after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let exit = sent.elapsed();
        let status = self.child.take().context("daemon already reaped")?.wait()?;
        Ok((exit, status.code()))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn parse_http(response: &str) -> (u16, String) {
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map_or_else(String::default, |(_, body)| body.to_string());
    (status, body)
}

fn http_get(address: SocketAddr, path: &str) -> Result<Option<(u16, String)>> {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HTTP_TIMEOUT) else {
        return Ok(None);
    };
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut raw = Vec::with_capacity(4096);
    let exchanged = stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.read_to_end(&mut raw));
    if exchanged.is_err() {
        return Ok(None);
    }
    Ok(Some(parse_http(&String::from_utf8_lossy(&raw))))
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn pulse_frame(channels: usize, channel: usize, amplitude: f32, batch_id: u64) -> Result<Vec<u8>> {
    let mut values = vec![0.0f32; channels];
    values[channel] = amplitude;
    let mut custom = HashMap::new();
    custom.insert("schema".to_string(), STIMULUS_SCHEMA.to_string());
    let batch = StimulusBatch {
        session_id: Some(SESSION_ID.to_string()),
        batch_id,
        timestamp: now_ns(),
        values,
        valid_mask: Some(vec![true; channels]),
        metadata: Some(BatchMetadata {
            processing_latency_ns: None,
            source: Some("usefulness-transport-driver".to_string()),
            custom,
        }),
    };
    Ok(serde_json::to_vec(&IpcMessage::Stimuli(batch))?)
}

fn parse_spikes(bytes: &[u8]) -> Result<(Vec<u16>, u64, u64)> {
    match serde_json::from_slice::<IpcMessage>(bytes).context("decode PUB frame")? {
        IpcMessage::Spikes(batch) => {
            let mut channels: Vec<u16> = batch.spikes.iter().map(|s| s.channel).collect();
            channels.sort_unstable();
            Ok((channels, batch.batch_id, batch.timestamp))
        }
        other => bail!("unexpected PUB frame variant: {other:?}"),
    }
}

/// One frame if any arrives before `deadline`, else `None`.
fn recv_until(sub: &zmq::Socket, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    let timeout = i64::try_from(remaining.as_millis().max(1))?;
    if sub.poll(zmq::POLLIN, timeout)? == 0 {
        return Ok(None);
    }
    Ok(Some(sub.recv_bytes(zmq::DONTWAIT)?))
}

/// Drain every frame available within `wait`, appending to `frames`.
fn collect_frames(
    sub: &zmq::Socket,
    wait: Duration,
    phase: Phase,
    frames: &mut Vec<Frame>,
) -> Result<()> {
    let deadline = Instant::now() + wait;
    while let Some(bytes) = recv_until(sub, deadline)? {
        let recv = Instant::now();
        let (channels, batch_id, timestamp) = parse_spikes(&bytes)?;
        frames.push(Frame {
            phase,
            recv,
            channels,
            batch_id,
            timestamp,
        });
    }
    Ok(())
}

fn vm_status_kb(pid: u32, key: &str) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted.get(rank).copied()
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
        if cfg.channels != workload.channels {
            bail!(
                "config channels {} != workload channels {}",
                cfg.channels,
                workload.channels
            );
        }
        let transport = &workload.transport;
        let spacing_ms = 1_000 * transport.pulse_spacing_ticks / u64::from(cfg.tick_rate_hz);
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
            spacing: Duration::from_millis(spacing_ms),
            expected,
        })
    }

    fn frame(&self, channel: usize, batch_id: u64) -> Result<Vec<u8>> {
        pulse_frame(self.channels, channel, self.amplitude, batch_id)
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
        rss_kb: vm_status_kb(pid, "VmRSS:"),
        peak_rss_kb: vm_status_kb(pid, "VmHWM:"),
    })
}

/// Everything the summary reports on.
struct Report<'a> {
    args: &'a Args,
    workload: &'a Workload,
    session: &'a Session,
    daemon_binary: &'a Path,
    log_path: &'a Path,
    pid: u32,
    ready_after: Duration,
    prime_attempts: usize,
    frames: &'a [Frame],
    pulses: &'a [Pulse],
    outcome: &'a Outcome,
    nonempty: usize,
    probes: &'a Probes,
    exit: (Duration, Option<i32>),
    termination_logged: bool,
}

fn count_frames(frames: &[Frame], pred: impl Fn(&Frame) -> bool) -> usize {
    frames.iter().filter(|f| pred(f)).count()
}

fn summary_json(r: &Report<'_>) -> Result<Value> {
    let mut sorted = r.outcome.latencies_ns.clone();
    sorted.sort_unstable();
    let sent = r.pulses.len() as u64;
    let lost = sent - r.outcome.matched - r.outcome.mismatched;
    let extra_nonempty = r.nonempty.saturating_sub(r.pulses.len()) as u64;
    let health_json = r
        .probes
        .health
        .as_ref()
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok());
    Ok(json!({
        "mode": "transport-live",
        "config": r.args.config.display().to_string(),
        "workload": r.args.workload.display().to_string(),
        "fixture_sha256": r.workload.fixture_sha256,
        "daemon_binary": r.daemon_binary.display().to_string(),
        "daemon_pid": r.pid,
        "ready_after_ns": r.ready_after.as_nanos() as u64,
        "subscribe_frames": count_frames(r.frames, |f| f.phase == Phase::Subscribe),
        "prime_attempts": r.prime_attempts,
        "prime_frames": count_frames(r.frames, |f| f.phase == Phase::Prime),
        "pulse_spacing_ns": r.session.spacing.as_nanos() as u64,
        "pulses_sent": sent,
        "frames_received_total": r.frames.len(),
        "measured_frames_total": count_frames(r.frames, |f| f.phase.measured()),
        "measured_frames_empty": count_frames(r.frames, |f| f.phase.measured() && f.channels.is_empty()),
        "measured_frames_nonempty": r.nonempty,
        "matched": r.outcome.matched,
        "mismatched": r.outcome.mismatched,
        "lost": lost,
        "extra_nonempty": extra_nonempty,
        "latency_ns": {
            "count": sorted.len(),
            "p50": percentile(&sorted, 0.50),
            "p99": percentile(&sorted, 0.99),
            "max": sorted.last().copied(),
            "min": sorted.first().copied(),
        },
        "per_pulse": r.outcome.per_pulse,
        "daemon_rss_kb": r.probes.rss_kb,
        "daemon_peak_rss_kb": r.probes.peak_rss_kb,
        "metrics_status": r.probes.metrics.as_ref().map(|(s, _)| *s),
        "metrics_text": r.probes.metrics.as_ref().map(|(_, b)| b.clone()),
        "health_status": r.probes.health.as_ref().map(|(s, _)| *s),
        "health_json": health_json,
        "sigterm_exit_ns": r.exit.0.as_nanos() as u64,
        "exit_code": r.exit.1,
        "termination_logged": r.termination_logged,
        "daemon_log": r.log_path.display().to_string(),
    }))
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let cfg = DaemonConfig::load(&args.config).context("load config")?;
    let workload: Workload =
        serde_json::from_str(&fs::read_to_string(&args.workload).context("read workload")?)
            .context("parse workload")?;
    let session = Session::new(&cfg, &workload)?;
    let control: SocketAddr = cfg
        .control_bind
        .as_deref()
        .context("transport config needs control_bind")?
        .parse()?;
    let binary = daemon_binary()?;
    let log_path = args.records.with_extension("daemon.log");

    let sockets = open_sockets(&cfg)?;
    let mut daemon = Daemon::spawn(&binary, &args.config, &log_path)?;
    let pid = daemon.pid();
    let ready_after = daemon.wait_ready(control)?;
    let started = daemon.spawned_at;

    let mut frames: Vec<Frame> = Vec::with_capacity(16_384);
    confirm_subscription(&sockets.sub, &mut frames)?;
    let mut batch_id = 1u64;
    let prime_attempts = prime(&session, &sockets, &mut frames, &mut batch_id)?;
    let pulses = measure(&session, &sockets, &mut frames, &mut batch_id)?;

    let probes = probe_daemon(control, pid)?;
    let exit = daemon.terminate()?;
    let daemon_log = fs::read_to_string(&log_path).unwrap_or_default();
    let (outcome, nonempty) = correlate(&pulses, &frames, &session.expected);
    write_frames(&args.records, &frames, &pulses, started)?;

    let report = Report {
        args: &args,
        workload: &workload,
        session: &session,
        daemon_binary: &binary,
        log_path: &log_path,
        pid,
        ready_after,
        prime_attempts,
        frames: &frames,
        pulses: &pulses,
        outcome: &outcome,
        nonempty,
        probes: &probes,
        exit,
        termination_logged: daemon_log.contains("Termination signal received"),
    };
    let summary = summary_json(&report)?;
    fs::write(
        &args.summary,
        serde_json::to_string_pretty(&summary)? + "\n",
    )
    .with_context(|| format!("write {}", args.summary.display()))?;
    println!(
        "transport-live: sent {} matched {} mismatched {} lost {}; exit {:?} after {:?}",
        pulses.len(),
        outcome.matched,
        outcome.mismatched,
        summary["lost"],
        exit.1,
        exit.0
    );
    Ok(())
}
