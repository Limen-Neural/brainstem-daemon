// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! CPU-only Linux qualification of the binary's real PUB and signal lifecycle.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let root = std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from,
        );
        let path = root.join(format!("pub-lifecycle-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Reap even if readiness, reception, or an assertion fails.
struct DaemonProcess(Option<Child>);

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ready(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    if stream
        .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.starts_with("HTTP/1.1 200")
}

fn subscriber(context: &zmq::Context, endpoint: &str) -> zmq::Socket {
    let sub = context.socket(zmq::SUB).unwrap();
    sub.set_linger(0).unwrap();
    sub.set_rcvhwm(1).unwrap();
    sub.set_subscribe(b"").unwrap();
    sub.connect(endpoint).unwrap();
    sub
}

#[test]
fn binary_pub_signals_exit_cleanly_and_release_ports() {
    let dir = TempDir::new();
    // Catch a missing signal handler, unbounded teardown, or a PUB/config
    // wiring regression. No subscriber is required for a successful send.
    for (signal, with_subscribers) in [("-TERM", false), ("-TERM", true), ("-INT", true)] {
        // Reserve distinct ports until just before spawning. ZMQ requires us
        // to release these reservations before it binds its own socket.
        let reservations: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addresses: Vec<_> = reservations
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        let endpoint = format!("tcp://{}", addresses[0]);
        let config_path = dir.0.join("daemon.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"runtime_mode = "simulation"
tick_rate_hz = 1000
log_level = "info"
lif_count = 4
izh_count = 0
channels = 4
model_path = "unused-in-simulation.json"
spine_pub_port = {}
spine_sub_port = {}
control_bind = "{}"
spine_pub_sndhwm = 2
spine_pub_linger_ms = 25
"#,
                addresses[0].port(),
                addresses[1].port(),
                addresses[2],
            ),
        )
        .unwrap();
        drop(reservations);
        let mut process = DaemonProcess(Some(
            Command::new(env!("CARGO_BIN_EXE_brainstem-daemon"))
                .arg("--config")
                .arg(&config_path)
                .env("RUST_LOG", "info")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let child = process.0.as_mut().unwrap();
        let deadline = Instant::now() + DEADLINE;
        while !ready(addresses[2]) {
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited at startup"
            );
            assert!(Instant::now() < deadline, "daemon never became ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        // On Linux 127/8 is loopback. A wildcard bind would occupy this
        // second address too; the default PUB must bind only 127.0.0.1.
        let _other_interface = TcpListener::bind(("127.0.0.2", addresses[0].port()))
            .expect("default PUB must not bind all interfaces");

        let context = zmq::Context::new();
        let subscribers = with_subscribers.then(|| {
            let active = subscriber(&context, &endpoint);
            let stalled = subscriber(&context, &endpoint);
            // A received frame establishes PUB/SUB readiness, rather than
            // assuming a fixed sleep is enough for the slow-joiner handshake.
            assert!(active.poll(zmq::POLLIN, 5000).unwrap() > 0);
            let frame = active.recv_bytes(zmq::DONTWAIT).unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(payload.as_object().unwrap().len(), 1);
            assert_eq!(payload["Spikes"]["spikes"], serde_json::json!([]));
            assert_eq!(payload["Spikes"]["session_id"], serde_json::Value::Null);
            assert_eq!(payload["Spikes"]["metadata"], serde_json::Value::Null);
            assert!(payload["Spikes"]["batch_id"].is_u64());
            assert!(payload["Spikes"]["timestamp"].is_u64());
            // Poll without consuming: prove the non-reading subscriber is
            // actually connected before requesting shutdown under pressure.
            assert!(stalled.poll(zmq::POLLIN, 5000).unwrap() > 0);
            // Keep the daemon ticking with the stalled SUB attached, rather
            // than shutting down immediately after the first handshake frame.
            let deadline = Instant::now() + Duration::from_secs(5);
            for _ in 0..32 {
                assert!(Instant::now() < deadline, "PUB stopped progressing");
                assert!(active.poll(zmq::POLLIN, 1000).unwrap() > 0);
                active.recv_bytes(zmq::DONTWAIT).unwrap();
            }
            (active, stalled)
        });

        assert!(
            Command::new("kill")
                .arg(signal)
                .arg(child.id().to_string())
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + DEADLINE;
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "{signal}: shutdown exceeded deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = process.0.take().unwrap().wait_with_output().unwrap();
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(output.status.success(), "{signal}: {logs}");
        assert!(logs.contains("Termination signal received"), "{logs}");
        drop(subscribers);
        drop(context);
        // Clean exit must release the PUB listener, not only stop ticking.
        let rebound = zmq::Context::new().socket(zmq::PUB).unwrap();
        rebound.set_linger(0).unwrap();
        rebound
            .bind(&endpoint)
            .expect("PUB port released after exit");
        TcpListener::bind(addresses[2]).expect("control port released after exit");
    }
}
