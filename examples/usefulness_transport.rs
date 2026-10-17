// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Transport driver for the usefulness experiment (`experiments/usefulness`).
//!
//! Launches the `corpus-ipc` `brainstem-daemon` binary with the
//! `transport-live` configuration, binds an external ZeroMQ producer on the
//! daemon's SUB endpoint, subscribes to the daemon PUB, waits for `/readyz`,
//! primes the PUB/SUB handshake, sends the measured pulses with fresh
//! timestamps, collects every output frame, reads `/metrics`, `/health` and
//! the daemon RSS, then stops the daemon with SIGTERM.
//!
//! Usage: `usefulness_transport <daemon-binary> <config.toml> <workload.json> <out.jsonl> <summary.json>`
//!
//! Requires `--features corpus-ipc`. Linux only (`/proc/<pid>/status`, `kill`).

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

#[derive(Debug)]
struct Frame {
    phase: &'static str,
    recv: Instant,
    channels: Vec<u16>,
    batch_id: u64,
    timestamp: u64,
}

struct Daemon {
    child: Option<Child>,
    spawned_at: Instant,
}

impl Daemon {
    fn spawn(binary: &Path, config: &Path, log_path: &Path) -> Result<Self> {
        let log = fs::File::create(log_path)
            .with_context(|| format!("create daemon log {}", log_path.display()))?;
        let child = Command::new(binary)
            .arg("--config")
            .arg(config)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn {}", binary.display()))?;
        Ok(Self {
            child: Some(child),
            spawned_at: Instant::now(),
        })
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    fn exited(&mut self) -> Result<bool> {
        Ok(self
            .child
            .as_mut()
            .context("daemon already reaped")?
            .try_wait()?
            .is_some())
    }

    fn wait_ready(&mut self, control: SocketAddr) -> Result<Duration> {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if self.exited()? {
                bail!("daemon exited before becoming ready");
            }
            if http_get(control, "/readyz")?.is_some_and(|(status, _)| status == 200) {
                return Ok(self.spawned_at.elapsed());
            }
            if Instant::now() > deadline {
                bail!("daemon never reported ready on {control}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate(&mut self) -> Result<(Duration, Option<i32>)> {
        let pid = self.pid().to_string();
        let sent = Instant::now();
        let status = Command::new("kill").arg("-TERM").arg(&pid).status()?;
        if !status.success() {
            bail!("kill -TERM {pid} failed: {status}");
        }
        let deadline = sent + EXIT_DEADLINE;
        loop {
            if self.exited()? {
                break;
            }
            if Instant::now() > deadline {
                bail!("daemon did not exit within {EXIT_DEADLINE:?} after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let exit = sent.elapsed();
        let output = self.child.take().context("daemon already reaped")?.wait()?;
        Ok((exit, output.code()))
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

fn http_get(address: SocketAddr, path: &str) -> Result<Option<(u16, String)>> {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HTTP_TIMEOUT) else {
        return Ok(None);
    };
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Ok(None);
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return Ok(None);
    }
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Ok(Some((status, body)))
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

/// Drain every frame available within `wait`, appending to `frames`.
fn collect_frames(
    sub: &zmq::Socket,
    wait: Duration,
    phase: &'static str,
    frames: &mut Vec<Frame>,
) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let timeout = i64::try_from(remaining.as_millis().max(1))?;
        if sub.poll(zmq::POLLIN, timeout)? == 0 {
            return Ok(());
        }
        let bytes = sub.recv_bytes(zmq::DONTWAIT)?;
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        bail!(
            "usage: {} <daemon-binary> <config.toml> <workload.json> <out.jsonl> <summary.json>",
            args[0]
        );
    }
    let binary = PathBuf::from(&args[1]);
    let config_path = PathBuf::from(&args[2]);
    let workload_path = PathBuf::from(&args[3]);
    let records_path = PathBuf::from(&args[4]);
    let summary_path = PathBuf::from(&args[5]);
    let log_path = records_path.with_extension("daemon.log");

    let cfg = DaemonConfig::load(&config_path).context("load config")?;
    let workload: Workload =
        serde_json::from_str(&fs::read_to_string(&workload_path).context("read workload")?)
            .context("parse workload")?;
    if cfg.channels != workload.channels {
        bail!(
            "config channels {} != workload channels {}",
            cfg.channels,
            workload.channels
        );
    }
    let control: SocketAddr = cfg
        .control_bind
        .as_deref()
        .context("transport config needs control_bind")?
        .parse()?;
    let spacing = Duration::from_millis(
        1_000 * workload.transport.pulse_spacing_ticks / u64::from(cfg.tick_rate_hz),
    );
    let expected: Vec<Vec<u16>> = (0..cfg.channels)
        .map(|c| {
            workload
                .transport
                .expected_by_channel
                .get(&c.to_string())
                .cloned()
                .with_context(|| format!("expected_by_channel missing channel {c}"))
        })
        .collect::<Result<_>>()?;

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

    let mut daemon = Daemon::spawn(&binary, &config_path, &log_path)?;
    let pid = daemon.pid();
    let ready_after = daemon.wait_ready(control)?;
    let started = daemon.spawned_at;

    let mut frames: Vec<Frame> = Vec::with_capacity(16_384);
    // Subscription is confirmed by a received frame (empty allowed), never by
    // readiness or a local send count.
    collect_frames(&sub, SUBSCRIBE_DEADLINE, "subscribe", &mut frames)?;
    if frames.is_empty() {
        bail!("no PUB frame received within {SUBSCRIBE_DEADLINE:?}; subscription not confirmed");
    }

    // Prime until the expected nonempty output for channel 0 arrives.
    let mut batch_id = 1u64;
    let mut prime_attempts = 0usize;
    let mut primed = false;
    while prime_attempts < PRIME_ATTEMPTS {
        prime_attempts += 1;
        producer.send(
            pulse_frame(cfg.channels, 0, workload.transport.amplitude, batch_id)?,
            0,
        )?;
        batch_id += 1;
        let before = frames.len();
        collect_frames(&sub, PRIME_WAIT, "prime", &mut frames)?;
        if frames[before..].iter().any(|f| f.channels == expected[0]) {
            primed = true;
            break;
        }
    }
    if !primed {
        bail!("priming failed: no expected nonempty output after {prime_attempts} attempts");
    }
    // Let the priming response settle, then drain so measurement starts clean.
    collect_frames(&sub, spacing * 2, "prime", &mut frames)?;

    let mut sends: Vec<(Instant, usize)> = Vec::with_capacity(workload.transport.pulses as usize);
    let measure_start = Instant::now();
    for k in 0..workload.transport.pulses as usize {
        let channel = k % cfg.channels;
        let payload = pulse_frame(
            cfg.channels,
            channel,
            workload.transport.amplitude,
            batch_id,
        )?;
        batch_id += 1;
        let at = Instant::now();
        producer.send(payload, 0)?;
        sends.push((at, channel));
        let next = measure_start + spacing * (k as u32 + 1);
        collect_frames(
            &sub,
            next.saturating_duration_since(Instant::now()),
            "measure",
            &mut frames,
        )?;
    }
    collect_frames(&sub, TAIL_WAIT, "tail", &mut frames)?;

    let metrics = http_get(control, "/metrics")?;
    let health = http_get(control, "/health")?;
    let rss_kb = vm_status_kb(pid, "VmRSS:");
    let peak_rss_kb = vm_status_kb(pid, "VmHWM:");
    let (exit_after, exit_code) = daemon.terminate()?;
    let daemon_log = fs::read_to_string(&log_path).unwrap_or_default();

    // Correlate by order only: the i-th nonempty measured frame answers the i-th pulse.
    let measured_nonempty: Vec<&Frame> = frames
        .iter()
        .filter(|f| matches!(f.phase, "measure" | "tail") && !f.channels.is_empty())
        .collect();
    let mut latencies_ns = Vec::new();
    let mut matched = 0u64;
    let mut mismatched = 0u64;
    let mut per_pulse = Vec::new();
    for (i, (sent_at, channel)) in sends.iter().enumerate() {
        match measured_nonempty.get(i) {
            Some(frame) => {
                let ok = frame.channels == expected[*channel];
                if ok {
                    matched += 1;
                } else {
                    mismatched += 1;
                }
                let latency = frame.recv.saturating_duration_since(*sent_at).as_nanos() as u64;
                latencies_ns.push(latency);
                per_pulse.push(serde_json::json!({
                    "pulse": i, "channel": channel, "matched": ok,
                    "latency_ns": latency, "received": frame.channels,
                }));
            }
            None => per_pulse.push(serde_json::json!({
                "pulse": i, "channel": channel, "matched": false, "latency_ns": null,
            })),
        }
    }
    let extra_nonempty = measured_nonempty.len().saturating_sub(sends.len()) as u64;
    let lost = sends.len() as u64 - matched - mismatched;
    let mut sorted = latencies_ns.clone();
    sorted.sort_unstable();

    let file = fs::File::create(&records_path)?;
    let mut out = BufWriter::new(file);
    let send_index: HashMap<usize, (Instant, usize)> = sends.iter().copied().enumerate().collect();
    let mut nonempty_seen = 0usize;
    for (seq, frame) in frames.iter().enumerate() {
        let mut pulse = serde_json::Value::Null;
        let mut send_ns = serde_json::Value::Null;
        if matches!(frame.phase, "measure" | "tail") && !frame.channels.is_empty() {
            if let Some((sent_at, _)) = send_index.get(&nonempty_seen) {
                pulse = serde_json::json!(nonempty_seen);
                send_ns = serde_json::json!(sent_at.duration_since(started).as_nanos() as u64);
            }
            nonempty_seen += 1;
        }
        let record = serde_json::json!({
            "seq": seq,
            "phase": frame.phase,
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

    let summary = serde_json::json!({
        "mode": "transport-live",
        "config": config_path.display().to_string(),
        "workload": workload_path.display().to_string(),
        "fixture_sha256": workload.fixture_sha256,
        "daemon_binary": binary.display().to_string(),
        "daemon_pid": pid,
        "ready_after_ns": ready_after.as_nanos() as u64,
        "subscribe_frames": frames.iter().filter(|f| f.phase == "subscribe").count(),
        "prime_attempts": prime_attempts,
        "prime_frames": frames.iter().filter(|f| f.phase == "prime").count(),
        "pulse_spacing_ns": spacing.as_nanos() as u64,
        "pulses_sent": sends.len(),
        "frames_received_total": frames.len(),
        "measured_frames_total": frames.iter().filter(|f| matches!(f.phase, "measure" | "tail")).count(),
        "measured_frames_empty": frames.iter().filter(|f| matches!(f.phase, "measure" | "tail") && f.channels.is_empty()).count(),
        "measured_frames_nonempty": measured_nonempty.len(),
        "matched": matched,
        "mismatched": mismatched,
        "lost": lost,
        "extra_nonempty": extra_nonempty,
        "latency_ns": {
            "count": sorted.len(),
            "p50": percentile(&sorted, 0.50),
            "p99": percentile(&sorted, 0.99),
            "max": sorted.last().copied(),
            "min": sorted.first().copied(),
        },
        "per_pulse": per_pulse,
        "daemon_rss_kb": rss_kb,
        "daemon_peak_rss_kb": peak_rss_kb,
        "metrics_status": metrics.as_ref().map(|(s, _)| *s),
        "metrics_text": metrics.as_ref().map(|(_, b)| b.clone()),
        "health_status": health.as_ref().map(|(s, _)| *s),
        "health_json": health
            .as_ref()
            .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b).ok()),
        "sigterm_exit_ns": exit_after.as_nanos() as u64,
        "exit_code": exit_code,
        "termination_logged": daemon_log.contains("Termination signal received"),
        "daemon_log": log_path.display().to_string(),
    });
    fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary)? + "\n",
    )?;
    println!(
        "transport-live: sent {} matched {matched} mismatched {mismatched} lost {lost} extra {extra_nonempty}; exit {exit_code:?} after {exit_after:?}",
        sends.len()
    );
    Ok(())
}
