// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Raul Montoya Cardenas

//! CPU-only Linux qualification of the binary's real PUB and signal lifecycle.
#![cfg(target_os = "linux")]

#[path = "fixtures/temp_dir.rs"]
mod temp_dir;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use temp_dir::TempDir;

const DEADLINE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const HTTP_TIMEOUT: Duration = Duration::from_millis(100);

/// Loopback ports for one daemon run.
struct Ports {
    publish: SocketAddr,
    subscribe: SocketAddr,
    control: SocketAddr,
}

impl Ports {
    /// Reserve distinct ports, then release them for the daemon: ZMQ requires
    /// the reservations to be gone before it binds its own socket.
    fn reserve() -> Self {
        let reservations: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let address = |index: usize| reservations[index].local_addr().unwrap();
        Self {
            publish: address(0),
            subscribe: address(1),
            control: address(2),
        }
    }

    fn pub_endpoint(&self) -> String {
        format!("tcp://{}", self.publish)
    }
}

// Reap even if readiness, reception, or an assertion fails.
struct DaemonProcess(Option<Child>);

impl DaemonProcess {
    fn spawn(config_path: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_brainstem-daemon"))
            .arg("--config")
            .arg(config_path)
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(Some(child))
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }

    fn wait_ready(&mut self, control: SocketAddr) {
        let child = self.child();
        wait_until("daemon never became ready", || {
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited at startup"
            );
            ready(control)
        });
    }

    fn signal(&mut self, signal: &str) {
        let pid = self.child().id().to_string();
        assert!(
            Command::new("kill")
                .arg(signal)
                .arg(pid)
                .status()
                .unwrap()
                .success()
        );
    }

    /// Bounded wait for a successful exit that logged the signal handler.
    fn assert_clean_exit(mut self, signal: &str) {
        let child = self.child();
        wait_until(&format!("{signal}: shutdown exceeded deadline"), || {
            child.try_wait().unwrap().is_some()
        });
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(output.status.success(), "{signal}: {logs}");
        assert!(logs.contains("Termination signal received"), "{logs}");
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Poll until `done` holds, failing with `what` once [`DEADLINE`] passes.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn ready(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HTTP_TIMEOUT) else {
        return false;
    };
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(HTTP_TIMEOUT)).unwrap();
    let mut response = String::new();
    stream
        .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_ok()
        && stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
}

fn write_config(dir: &TempDir, ports: &Ports) -> PathBuf {
    let config_path = dir.path().join("daemon.toml");
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
            ports.publish.port(),
            ports.subscribe.port(),
            ports.control,
        ),
    )
    .unwrap();
    config_path
}

fn subscriber(context: &zmq::Context, endpoint: &str) -> zmq::Socket {
    let sub = context.socket(zmq::SUB).unwrap();
    sub.set_linger(0).unwrap();
    sub.set_rcvhwm(1).unwrap();
    sub.set_subscribe(b"").unwrap();
    sub.connect(endpoint).unwrap();
    sub
}

fn recv_frame(sub: &zmq::Socket, timeout_ms: i64) -> Vec<u8> {
    assert!(
        sub.poll(zmq::POLLIN, timeout_ms).unwrap() > 0,
        "no PUB frame within {timeout_ms} ms"
    );
    sub.recv_bytes(zmq::DONTWAIT).unwrap()
}

/// The default binary publishes the unversioned, empty `Spikes` object.
fn assert_empty_spikes_frame(frame: &[u8]) {
    let payload: serde_json::Value = serde_json::from_slice(frame).unwrap();
    assert_eq!(payload.as_object().unwrap().len(), 1);
    let spikes = &payload["Spikes"];
    assert_eq!(spikes["spikes"], serde_json::json!([]));
    assert_eq!(spikes["session_id"], serde_json::Value::Null);
    assert_eq!(spikes["metadata"], serde_json::Value::Null);
    assert!(spikes["batch_id"].is_u64());
    assert!(spikes["timestamp"].is_u64());
}

/// Attach an active and a stalled SUB and keep the daemon ticking under them.
fn attach_subscribers(context: &zmq::Context, endpoint: &str) -> (zmq::Socket, zmq::Socket) {
    let active = subscriber(context, endpoint);
    let stalled = subscriber(context, endpoint);
    // A received frame establishes PUB/SUB readiness, rather than assuming a
    // fixed sleep is enough for the slow-joiner handshake.
    assert_empty_spikes_frame(&recv_frame(&active, 5000));
    // Poll without consuming: prove the non-reading subscriber is actually
    // connected before requesting shutdown under pressure.
    assert!(stalled.poll(zmq::POLLIN, 5000).unwrap() > 0);
    // Keep the daemon ticking with the stalled SUB attached, rather than
    // shutting down immediately after the first handshake frame.
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..32 {
        assert!(Instant::now() < deadline, "PUB stopped progressing");
        recv_frame(&active, 1000);
    }
    (active, stalled)
}

/// On Linux 127/8 is loopback. A wildcard bind would occupy this second
/// address too; the default PUB must bind only 127.0.0.1.
fn assert_pub_bound_to_loopback_only(ports: &Ports) {
    TcpListener::bind(("127.0.0.2", ports.publish.port()))
        .expect("default PUB must not bind all interfaces");
}

/// Clean exit must release the PUB listener, not only stop ticking.
fn assert_ports_released(ports: &Ports) {
    let rebound = zmq::Context::new().socket(zmq::PUB).unwrap();
    rebound.set_linger(0).unwrap();
    rebound
        .bind(&ports.pub_endpoint())
        .expect("PUB port released after exit");
    TcpListener::bind(ports.control).expect("control port released after exit");
}

/// Start the daemon, deliver `signal`, and verify a bounded, clean exit.
fn assert_signal_lifecycle(dir: &TempDir, signal: &str, with_subscribers: bool) {
    let ports = Ports::reserve();
    let mut daemon = DaemonProcess::spawn(&write_config(dir, &ports));
    daemon.wait_ready(ports.control);
    assert_pub_bound_to_loopback_only(&ports);

    let context = zmq::Context::new();
    let subscribers = with_subscribers.then(|| attach_subscribers(&context, &ports.pub_endpoint()));
    daemon.signal(signal);
    daemon.assert_clean_exit(signal);
    drop(subscribers);
    drop(context);
    assert_ports_released(&ports);
}

#[test]
fn binary_pub_signals_exit_cleanly_and_release_ports() {
    let dir = TempDir::new("pub-lifecycle");
    // Catch a missing signal handler, unbounded teardown, or a PUB/config
    // wiring regression. No subscriber is required for a successful send.
    for (signal, with_subscribers) in [("-TERM", false), ("-TERM", true), ("-INT", true)] {
        assert_signal_lifecycle(&dir, signal, with_subscribers);
    }
}
