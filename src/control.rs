// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! Optional control surface for health snapshots and low-cardinality metrics.
//!
//! This repository previously had no HTTP/metrics server. A single listener is
//! started from `BrainstemDaemon::run` when `control_bind` is set. Do not add
//! a second server alongside this one.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tracing::{info, warn};

use crate::health::{HealthHandle, HealthSnapshot};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const MAX_CONTROL_CONNS: usize = 32;

pub async fn serve(
    addr: SocketAddr,
    health: HealthHandle,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind control surface on {addr}"))?;
    serve_listener(listener, health, shutdown).await
}

pub async fn serve_listener(
    listener: TcpListener,
    health: HealthHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let bound = listener
        .local_addr()
        .context("control listener has no local address")?;
    info!(%bound, "control surface listening (/livez /readyz /health /metrics)");

    if shutdown_signaled(&shutdown) {
        return Ok(());
    }

    let slots = Arc::new(Semaphore::new(MAX_CONTROL_CONNS));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if should_stop_control(changed, &shutdown) {
                    break;
                }
            }
            accepted = listener.accept() => {
                if !spawn_accepted(accepted, &slots, &health) {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            }
        }
    }

    Ok(())
}

fn shutdown_signaled(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

fn should_stop_control(
    changed: Result<(), watch::error::RecvError>,
    shutdown: &watch::Receiver<bool>,
) -> bool {
    changed.is_err() || shutdown_signaled(shutdown)
}

fn spawn_accepted(
    accepted: std::io::Result<(TcpStream, std::net::SocketAddr)>,
    slots: &Arc<Semaphore>,
    health: &HealthHandle,
) -> bool {
    match accepted {
        Ok((stream, _)) => {
            spawn_control_conn(stream, slots, health);
            true
        }
        Err(e) => {
            warn!("control accept failed: {e}");
            false
        }
    }
}

fn spawn_control_conn(stream: TcpStream, slots: &Arc<Semaphore>, health: &HealthHandle) {
    match slots.clone().try_acquire_owned() {
        Ok(permit) => spawn_served_conn(stream, permit, health.clone()),
        Err(_) => spawn_busy_conn(stream),
    }
}

fn spawn_served_conn(stream: TcpStream, permit: OwnedSemaphorePermit, health: HealthHandle) {
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(e) = handle_connection(stream, &health).await {
            warn!("control connection failed: {e}");
        }
    });
}

fn spawn_busy_conn(stream: TcpStream) {
    tokio::spawn(async move {
        let mut stream = stream;
        if let Err(e) = write_http_response(&mut stream, &busy_http()).await {
            warn!("control busy reply failed: {e}");
        }
    });
}

async fn handle_connection(mut stream: TcpStream, health: &HealthHandle) -> Result<()> {
    let req = read_http_request(&mut stream).await?;
    write_http_response(&mut stream, &control_response(&req, health)).await
}

fn control_response(req: &str, health: &HealthHandle) -> Vec<u8> {
    match health.try_snapshot() {
        Some(snapshot) => render_http(req, &snapshot),
        None => busy_http(),
    }
}

fn busy_http() -> Vec<u8> {
    http_response(503, "text/plain; charset=utf-8", b"busy\n")
}

async fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut buf = [0u8; 1024];
    let n = timed_io(
        "control read timed out",
        read_request_line(stream, &mut buf),
    )
    .await?;
    Ok(std::str::from_utf8(&buf[..n]).unwrap_or("").to_owned())
}

async fn write_http_response(stream: &mut TcpStream, response: &[u8]) -> Result<()> {
    timed_io("control write timed out", async {
        stream.write_all(response).await?;
        stream.flush().await?;
        Ok(())
    })
    .await
}

async fn timed_io<T, F>(timeout_msg: &'static str, fut: F) -> Result<T>
where
    F: Future<Output = std::io::Result<T>>,
{
    tokio::time::timeout(IO_TIMEOUT, fut)
        .await
        .context(timeout_msg)?
        .context("control I/O failed")
}

async fn read_request_line(stream: &mut TcpStream, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = stream.read(&mut buf[total..]).await?;
        total += n;
        if request_line_complete(n, &buf[..total]) {
            break;
        }
    }
    Ok(total)
}

fn request_line_complete(read: usize, buf: &[u8]) -> bool {
    read == 0 || buf.contains(&b'\n')
}

pub(crate) fn render_http(request: &str, snap: &HealthSnapshot) -> Vec<u8> {
    match parse_get_path(request) {
        ParseResult::Get(path) => {
            let (status, content_type, body) = route(path, snap);
            http_response(status, content_type, &body)
        }
        ParseResult::NotGet => {
            http_response(405, "text/plain; charset=utf-8", b"method not allowed\n")
        }
        ParseResult::Invalid => http_response(400, "text/plain; charset=utf-8", b"bad request\n"),
    }
}

fn route(path: &str, snap: &HealthSnapshot) -> (u16, &'static str, Vec<u8>) {
    match path {
        "/livez" | "/healthz/live" => probe(snap.live, b"live\n", b"not live\n"),
        "/readyz" | "/healthz/ready" => probe(snap.ready, b"ready\n", b"not ready\n"),
        "/health" => (
            200,
            "application/json",
            serde_json::to_vec(snap).unwrap_or_else(|_| b"{}".to_vec()),
        ),
        "/metrics" => (
            200,
            "text/plain; version=0.0.4",
            snap.prometheus_text().into_bytes(),
        ),
        _ => (404, "text/plain; charset=utf-8", b"not found\n".to_vec()),
    }
}

fn probe(ok: bool, yes: &'static [u8], no: &'static [u8]) -> (u16, &'static str, Vec<u8>) {
    if ok {
        (200, "text/plain; charset=utf-8", yes.to_vec())
    } else {
        (503, "text/plain; charset=utf-8", no.to_vec())
    }
}

enum ParseResult<'a> {
    Get(&'a str),
    NotGet,
    Invalid,
}

fn parse_get_path(request: &str) -> ParseResult<'_> {
    let Some(line) = request.lines().next() else {
        return ParseResult::Invalid;
    };
    parse_request_line(line)
}

fn parse_request_line(line: &str) -> ParseResult<'_> {
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return ParseResult::Invalid;
    };
    if !method.eq_ignore_ascii_case("GET") {
        return ParseResult::NotGet;
    }
    ParseResult::Get(path_without_query(target))
}

fn path_without_query(target: &str) -> &str {
    target.split('?').next().unwrap_or(target)
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut out = Vec::new();
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{
        CheckpointIdentity, FakeClock, HealthEvent, HealthHandle, HealthLimits, HealthMachine,
        ReasonCode,
    };

    fn ready_snap() -> HealthSnapshot {
        let clock = FakeClock::default();
        let mut machine = HealthMachine::new(clock, HealthLimits::default());
        machine.apply(HealthEvent::ProcessStarted);
        machine.apply(HealthEvent::InitializationCompleted);
        machine.apply(HealthEvent::CheckpointValidated {
            identity: CheckpointIdentity {
                id: "soma16".into(),
                digest: None,
            },
        });
        machine.apply(HealthEvent::IngressObserved);
        machine.snapshot()
    }

    #[test]
    fn livez_and_readyz_use_status_codes() {
        let snap = ready_snap();
        let live = String::from_utf8(render_http("GET /livez HTTP/1.1\r\n\r\n", &snap)).unwrap();
        assert!(live.starts_with("HTTP/1.1 200 OK"));
        let ready = String::from_utf8(render_http("GET /readyz HTTP/1.1\r\n\r\n", &snap)).unwrap();
        assert!(ready.starts_with("HTTP/1.1 200 OK"));

        let starting = HealthMachine::new(FakeClock::default(), HealthLimits::default());
        let mut starting_m = starting;
        starting_m.apply(HealthEvent::ProcessStarted);
        let starting = starting_m.snapshot();
        let live =
            String::from_utf8(render_http("GET /livez HTTP/1.1\r\n\r\n", &starting)).unwrap();
        assert!(live.starts_with("HTTP/1.1 200 OK"));
        let ready =
            String::from_utf8(render_http("GET /readyz HTTP/1.1\r\n\r\n", &starting)).unwrap();
        assert!(ready.starts_with("HTTP/1.1 503"));
        assert!(!starting.ready);
        assert!(starting.reasons.contains(&ReasonCode::Starting));
    }

    #[test]
    fn health_json_is_always_200() {
        let mut machine = HealthMachine::new(FakeClock::default(), HealthLimits::default());
        machine.apply(HealthEvent::ProcessStarted);
        let snap = machine.snapshot();
        let raw = String::from_utf8(render_http("GET /health HTTP/1.1\r\n\r\n", &snap)).unwrap();
        assert!(raw.starts_with("HTTP/1.1 200 OK"));
        assert!(raw.contains("\"live\":true"));
        assert!(raw.contains("\"ready\":false"));
    }

    #[test]
    fn unknown_path_and_method() {
        let snap = ready_snap();
        let not_found =
            String::from_utf8(render_http("GET /nope HTTP/1.1\r\n\r\n", &snap)).unwrap();
        assert!(not_found.starts_with("HTTP/1.1 404"));
        let bad_method =
            String::from_utf8(render_http("POST /health HTTP/1.1\r\n\r\n", &snap)).unwrap();
        assert!(bad_method.starts_with("HTTP/1.1 405"));
    }

    #[tokio::test]
    async fn livez_is_200_while_readyz_is_503_before_checkpoint() {
        let health = HealthHandle::started(HealthLimits::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        let server = tokio::spawn(async move { serve_listener(listener, health, rx).await });

        let live = http_get(addr, "/livez").await;
        assert!(live.contains("HTTP/1.1 200"), "{live}");
        let ready = http_get(addr, "/readyz").await;
        assert!(ready.contains("HTTP/1.1 503"), "{ready}");
        let body = http_get(addr, "/health").await;
        assert!(body.contains("\"live\":true"));
        assert!(body.contains("\"ready\":false"));

        let _ = tx.send(true);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn saturated_control_writes_503() {
        let slots = Arc::new(Semaphore::new(0));
        let health = HealthHandle::started(HealthLimits::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        spawn_control_conn(server, &slots, &health);
        let mut buf = vec![0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let raw = String::from_utf8_lossy(&buf[..n]);
        assert!(raw.contains("HTTP/1.1 503"), "{raw}");
        assert!(raw.contains("busy"), "{raw}");
        assert_eq!(slots.available_permits(), 0);
    }

    #[test]
    fn control_response_is_503_when_snapshot_would_block() {
        let health = HealthHandle::started(HealthLimits::default());
        let _guard = health.lock_write_for_test();
        let raw =
            String::from_utf8(control_response("GET /livez HTTP/1.1\r\n\r\n", &health)).unwrap();
        assert!(raw.starts_with("HTTP/1.1 503"));
        assert!(raw.contains("busy"));
    }

    async fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 2048];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}
