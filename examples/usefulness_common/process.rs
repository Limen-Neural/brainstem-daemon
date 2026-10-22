// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! The daemon child process and its HTTP control-surface probes.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const READY_DEADLINE: Duration = Duration::from_secs(10);
const EXIT_DEADLINE: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_millis(200);

/// A spawned `brainstem-daemon`, killed on drop if still running.
pub struct DaemonProcess {
    child: Option<Child>,
    pub spawned_at: Instant,
}

impl DaemonProcess {
    pub fn spawn(binary: &Path, config: &Path, log_path: &Path) -> Result<Self> {
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

    pub fn pid(&self) -> u32 {
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

    /// Poll `/readyz` until it answers 200; returns the time since spawn.
    pub fn wait_ready(&mut self, control: SocketAddr) -> Result<Duration> {
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

    /// Send SIGTERM and wait for exit; returns (time to exit, exit code).
    pub fn terminate(&mut self) -> Result<(Duration, Option<i32>)> {
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

impl Drop for DaemonProcess {
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

/// One bounded HTTP/1.1 GET; `None` when the daemon is not answering.
pub fn http_get(address: SocketAddr, path: &str) -> Result<Option<(u16, String)>> {
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
