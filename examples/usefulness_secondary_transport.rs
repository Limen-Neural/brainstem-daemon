// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Secondary usefulness experiment: the feature-enabled binary over ZeroMQ.
//!
//! See `experiments/usefulness-secondary/PROTOCOL.md`. Linux only (reads `/proc`). Run
//! from the repository root after `cargo build --release --locked --features corpus-ipc`:
//!
//! ```text
//! cargo run --release --locked --features corpus-ipc --example usefulness_secondary_transport -- \
//!     --bin target/release/brainstem-daemon --rep 0 --out <dir>
//! ```
//!
//! Producer PUB (bound on `spine_sub_port`) → daemon SUB → tick → daemon PUB →
//! consumer SUB. Records raw send/receive times; matching is done by
//! `summarize.py` by order and time only (input batch IDs are not propagated).

#[path = "usefulness_secondary_common/mod.rs"]
mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use brainstem_daemon::DaemonConfig;
use clap::Parser;
use corpus_ipc::IpcMessage;
use serde_json::{Value, json};

use common::{TransportSpec, Workload};

const CONFIG: &str = "experiments/usefulness-secondary/config/transport-live.toml";

fn config_path() -> &'static Path {
    Path::new(CONFIG)
}
const READY_DEADLINE: Duration = Duration::from_secs(10);
const EXIT_BUDGET: Duration = Duration::from_secs(2);
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// Command-line arguments (the runner passes fixed values; nothing is security-relevant).
#[derive(Parser)]
#[command(about = "Secondary usefulness experiment: the feature-enabled binary over ZeroMQ")]
struct Args {
    /// Path to the `brainstem-daemon` binary built with `--features corpus-ipc`.
    #[arg(long)]
    bin: PathBuf,
    #[arg(long, default_value_t = 0)]
    rep: u32,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Clone)]
struct Received {
    at_ns: u64,
    batch_id: u64,
    channels: Vec<u16>,
}

struct Sent {
    probe: usize,
    channel: usize,
    at_ns: u64,
}

/// State shared between the main thread and the consumer thread.
#[derive(Clone)]
struct Shared {
    origin: Instant,
    stop: Arc<AtomicBool>,
    records: Arc<Mutex<Vec<Received>>>,
}

impl Shared {
    fn now_ns(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }

    fn any_batch(&self, pred: impl Fn(&Received) -> bool) -> bool {
        self.records.lock().unwrap().iter().any(pred)
    }
}

/// Kill and reap the daemon if the driver fails before a clean SIGTERM exit.
struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Decode one PUB frame into `(batch_id, sorted channels)` if it is a `Spikes` batch.
fn decode_spikes(bytes: &[u8]) -> Option<(u64, Vec<u16>)> {
    let Ok(IpcMessage::Spikes(batch)) = serde_json::from_slice::<IpcMessage>(bytes) else {
        return None;
    };
    let mut channels: Vec<u16> = batch.spikes.iter().map(|s| s.channel).collect();
    channels.sort_unstable();
    Some((batch.batch_id, channels))
}

/// Receive every queued frame; returns how many were not `Spikes` batches.
fn drain(sub: &zmq::Socket, shared: &Shared) -> u64 {
    let mut undecodable = 0;
    while let Ok(bytes) = sub.recv_bytes(zmq::DONTWAIT) {
        let at_ns = shared.now_ns();
        match decode_spikes(&bytes) {
            Some((batch_id, channels)) => shared.records.lock().unwrap().push(Received {
                at_ns,
                batch_id,
                channels,
            }),
            None => undecodable += 1,
        }
    }
    undecodable
}

fn subscriber(context: &zmq::Context, endpoint: &str) -> zmq::Result<zmq::Socket> {
    let sub = context.socket(zmq::SUB)?;
    sub.set_linger(0)?;
    sub.set_rcvhwm(100_000)?;
    sub.set_subscribe(b"")?;
    sub.connect(endpoint)?;
    Ok(sub)
}

fn spawn_receiver(sub: zmq::Socket, shared: Shared) -> JoinHandle<Result<u64>> {
    std::thread::spawn(move || {
        let mut undecodable = 0u64;
        while !shared.stop.load(Ordering::Acquire) {
            if sub.poll(zmq::POLLIN, 5)? > 0 {
                undecodable += drain(&sub, &shared);
            }
        }
        Ok(undecodable)
    })
}

fn readyz_ok(control: &str) -> bool {
    common::http_get(control, "/readyz").is_ok_and(|r| r.starts_with("HTTP/1.1 200"))
}

fn wait_ready(child: &mut Child, control: &str) -> Result<u64> {
    let start = Instant::now();
    while !readyz_ok(control) {
        if let Some(status) = child.try_wait()? {
            bail!("daemon exited during startup: {status}");
        }
        if start.elapsed() > READY_DEADLINE {
            bail!("daemon not ready within {READY_DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(start.elapsed().as_millis() as u64)
}

/// Poll `done` every millisecond until it holds or `deadline` passes.
fn wait_for(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

fn sleep_until(target: Instant) {
    let spin_below = Duration::from_micros(500);
    while let Some(left) = target.checked_duration_since(Instant::now()) {
        if left > spin_below {
            std::thread::sleep(left - Duration::from_micros(300));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn one_hot(channels: usize, channel: usize, value: f32) -> Vec<f32> {
    let mut values = vec![0.0; channels];
    values[channel] = value;
    values
}

fn wait_for_exit(child: &mut Child) -> Result<Option<ExitStatus>> {
    let start = Instant::now();
    while start.elapsed() <= EXIT_DEADLINE {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(None)
}

/// Outcome of the SIGTERM shutdown.
struct ExitInfo {
    sigterm_sent: bool,
    status: Option<ExitStatus>,
    exit_ms: f64,
}

impl ExitInfo {
    fn json(&self, log: &str) -> Value {
        json!({
            "sigterm_sent": self.sigterm_sent,
            "exit_status_code": self.status.and_then(|s| s.code()),
            "exit_success": self.status.is_some_and(|s| s.success()),
            "exit_ms": self.exit_ms,
            "exit_within_budget": self.status.is_some()
                && self.exit_ms <= EXIT_BUDGET.as_secs_f64() * 1e3,
            "logged_termination": log.contains("Termination signal received"),
        })
    }
}

/// One running experiment: producer PUB, daemon child, consumer thread.
struct Session {
    spec: TransportSpec,
    channels: usize,
    control: String,
    pub_endpoint: String,
    producer: zmq::Socket,
    daemon: Daemon,
    pid: u32,
    shared: Shared,
}

fn spawn_daemon(bin: &Path, log_path: &Path) -> Result<Child> {
    let log = std::fs::File::create(log_path)?;
    Command::new(bin)
        .arg("--config")
        .arg(CONFIG)
        .env("NO_COLOR", "1")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("failed to spawn {}", bin.display()))
}

impl Session {
    fn stimulus(&self, batch_id: u64, channel: usize) -> Result<Vec<u8>> {
        let values = one_hot(self.channels, channel, self.spec.probe_value);
        common::stimulus_frame(batch_id, &values, Some(&vec![true; self.channels]))
    }

    /// Send one priming frame and wait up to one interval for the expected output.
    fn prime_once(&self, frame: u64, expected: &[u16]) -> Result<bool> {
        let bytes = self.stimulus(1_000_000 + frame, self.spec.priming_channel)?;
        self.producer.send(bytes, 0)?;
        let interval = Duration::from_millis(self.spec.priming_interval_ms);
        Ok(wait_for(interval, || {
            self.shared.any_batch(|r| r.channels == expected)
        }))
    }

    /// Repeat a priming frame until the expected `{2c, 2c+1}` output arrives, so
    /// the daemon SUB -> producer PUB slow-joiner handshake is complete. Excluded.
    fn prime(&self) -> Result<u64> {
        let c = self.spec.priming_channel as u16;
        let expected = [2 * c, 2 * c + 1];
        let timeout = Duration::from_millis(self.spec.priming_timeout_ms);
        let start = Instant::now();
        let mut frames = 0u64;
        while !self.prime_once(frames, &expected)? {
            frames += 1;
            if start.elapsed() > timeout {
                bail!("priming: no {expected:?} output after {frames} frames");
            }
        }
        Ok(frames + 1)
    }

    /// Send the measured probes on a fixed schedule; returns sends and the window end.
    fn probes(&self) -> Result<(Vec<Sent>, u64)> {
        let start = Instant::now() + Duration::from_millis(5);
        let interval = Duration::from_millis(self.spec.probe_interval_ms);
        let mut sends = Vec::with_capacity(self.spec.probes);
        for probe in 0..self.spec.probes {
            sleep_until(start + interval * probe as u32);
            let channel = probe % self.channels;
            let bytes = self.stimulus(probe as u64, channel)?;
            let at_ns = self.shared.now_ns();
            self.producer.send(bytes, 0)?;
            sends.push(Sent {
                probe,
                channel,
                at_ns,
            });
        }
        sleep_until(start + interval * self.spec.probes as u32);
        Ok((sends, self.shared.now_ns()))
    }

    fn terminate(&mut self) -> Result<ExitInfo> {
        let started = Instant::now();
        let sigterm_sent = Command::new("kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status()?
            .success();
        let status = wait_for_exit(self.daemon.0.as_mut().expect("daemon running"))?;
        let exit_ms = started.elapsed().as_secs_f64() * 1e3;
        if status.is_some() {
            self.daemon.0 = None;
        }
        Ok(ExitInfo {
            sigterm_sent,
            status,
            exit_ms,
        })
    }
}

/// Measurement window in driver-relative ns.
struct Window {
    start_ns: u64,
    end_ns: u64,
}

impl Window {
    fn phase(&self, at_ns: u64) -> &'static str {
        match at_ns {
            t if t < self.start_ns => "priming",
            t if t <= self.end_ns => "measure",
            _ => "tail",
        }
    }
}

fn write_jsonl(path: &Path, records: impl Iterator<Item = Value>) -> Result<()> {
    let mut out = common::jsonl_writer(path)?;
    for record in records {
        serde_json::to_writer(&mut out, &record)?;
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

fn write_raw(prefix: &Path, sends: &[Sent], records: &[Received], window: &Window) -> Result<()> {
    let with = |suffix: &str| PathBuf::from(format!("{}.{suffix}", prefix.display()));
    write_jsonl(
        &with("sends.jsonl"),
        sends
            .iter()
            .map(|s| json!({"probe": s.probe, "channel": s.channel, "send_ns": s.at_ns})),
    )?;
    write_jsonl(
        &with("recv.jsonl"),
        records.iter().map(|r| {
            json!({"recv_ns": r.at_ns, "batch_id": r.batch_id, "channels": r.channels,
                   "phase": window.phase(r.at_ns)})
        }),
    )
}

/// Control-surface evidence captured just before SIGTERM.
struct Evidence {
    metrics: String,
    health: String,
    vm_hwm_kb: Option<u64>,
    vm_rss_kb: Option<u64>,
}

fn capture_evidence(control: &str, pid: u32) -> Evidence {
    let get = |path| common::http_get(control, path).unwrap_or_else(|e| format!("error: {e}"));
    let (vm_hwm_kb, vm_rss_kb) = common::proc_memory_kb(&format!("/proc/{pid}/status"));
    Evidence {
        metrics: get("/metrics"),
        health: get("/health"),
        vm_hwm_kb,
        vm_rss_kb,
    }
}

fn start_session(args: &Args, spec: TransportSpec, channels: usize) -> Result<(Session, u64)> {
    let cfg = DaemonConfig::load(config_path())?;
    if cfg.tick_rate_hz != spec.tick_rate_hz {
        bail!("config tick_rate_hz != workload transport tick_rate_hz");
    }
    let control = cfg
        .control_bind
        .clone()
        .context("transport config needs control_bind")?;
    let shared = Shared {
        origin: Instant::now(),
        stop: Arc::new(AtomicBool::new(false)),
        records: Arc::default(),
    };
    let context = zmq::Context::new();
    let producer = context.socket(zmq::PUB)?;
    producer.set_linger(0)?;
    producer.bind(&format!("tcp://127.0.0.1:{}", cfg.spine_sub_port))?;
    let log_path = args.out.join(format!("transport-r{}.daemon.txt", args.rep));
    let mut child = spawn_daemon(&args.bin, &log_path)?;
    let pid = child.id();
    let ready = wait_ready(&mut child, &control);
    let daemon = Daemon(Some(child));
    let ready_ms = ready?;
    let session = Session {
        spec,
        channels,
        control,
        pub_endpoint: format!("tcp://127.0.0.1:{}", cfg.spine_pub_port),
        producer,
        daemon,
        pid,
        shared,
    };
    Ok((session, ready_ms))
}

/// Everything one repetition measured, before it is written out.
struct Measured {
    ready_ms: u64,
    priming_frames: u64,
    sends: Vec<Sent>,
    window: Window,
    records: Vec<Received>,
    undecodable: u64,
    evidence: Evidence,
    exit: ExitInfo,
}

/// Subscribe, prime, send the probes, capture evidence, and shut down via SIGTERM.
fn measure(mut session: Session, ready_ms: u64) -> Result<Measured> {
    let context = zmq::Context::new();
    let sub = subscriber(&context, &session.pub_endpoint)?;
    let receiver = spawn_receiver(sub, session.shared.clone());
    // A received (possibly empty) Spikes batch proves the consumer subscription works.
    if !wait_for(READY_DEADLINE, || session.shared.any_batch(|_| true)) {
        bail!("no Spikes batch on consumer within {READY_DEADLINE:?}");
    }
    let priming_frames = session.prime()?;
    std::thread::sleep(Duration::from_millis(session.spec.settle_ms));
    let start_ns = session.shared.now_ns();
    let (sends, end_ns) = session.probes()?;
    let evidence = capture_evidence(&session.control, session.pid);
    let exit = session.terminate()?;
    session.shared.stop.store(true, Ordering::Release);
    let undecodable = receiver.join().expect("receiver thread panicked")?;
    let records = session.shared.records.lock().unwrap().clone();
    Ok(Measured {
        ready_ms,
        priming_frames,
        sends,
        window: Window { start_ns, end_ns },
        records,
        undecodable,
        evidence,
        exit,
    })
}

fn summary_json(args: &Args, workload: &Workload, m: &Measured, daemon_log: &str) -> Result<Value> {
    let mut summary = json!({
        "mode": "transport-live",
        "rep": args.rep,
        "config": CONFIG,
        "binary": args.bin.display().to_string(),
        "workload_version": workload.workload_version,
        "fixture_sha256": common::file_sha256(common::FIXTURE_PATH)?,
        "ready_ms": m.ready_ms,
        "priming_frames": m.priming_frames,
        "probes_sent": m.sends.len(),
        "measure_start_ns": m.window.start_ns,
        "measure_end_ns": m.window.end_ns,
        "batches_received_total": m.records.len(),
        "undecodable_frames": m.undecodable,
        "daemon_vm_hwm_kb": m.evidence.vm_hwm_kb,
        "daemon_vm_rss_kb": m.evidence.vm_rss_kb,
    });
    if let (Some(dst), Value::Object(src)) = (summary.as_object_mut(), m.exit.json(daemon_log)) {
        dst.extend(src);
    }
    Ok(summary)
}

fn write_outputs(args: &Args, workload: &Workload, m: &Measured) -> Result<()> {
    let prefix = args.out.join(format!("transport-r{}", args.rep));
    let with = |suffix: &str| PathBuf::from(format!("{}.{suffix}", prefix.display()));
    write_raw(&prefix, &m.sends, &m.records, &m.window)?;
    std::fs::write(with("metrics.txt"), &m.evidence.metrics)?;
    std::fs::write(with("health.txt"), &m.evidence.health)?;
    let daemon_log = std::fs::read_to_string(with("daemon.txt")).unwrap_or_default();
    common::write_json(
        &with("summary.json"),
        &summary_json(args, workload, m, &daemon_log)?,
    )
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let workload = Workload::load()?;
    let (session, ready_ms) = start_session(&args, workload.transport.clone(), workload.channels)?;
    let measured = measure(session, ready_ms)?;
    write_outputs(&args, &workload, &measured)
}
