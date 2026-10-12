#!/usr/bin/env python3
"""Linux release gate for the shipped archive, not the repository build tree."""

from dataclasses import dataclass, field, replace
import hashlib
import http.client
import itertools
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.request

CARGO = ["cargo", "+1.98.1"]
MODES = (("stub", []), ("corpus-ipc", ["--features", "corpus-ipc"]))
# Attempts to start the daemon when a freshly allocated loopback port was
# claimed by another process between allocation and the daemon's bind.
PORT_ATTEMPTS = 3

CONSUMER_MAIN = '''use brainstem_daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("config argument");
    let mut config = DaemonConfig::load(std::path::Path::new(&path))?;
    assert_eq!(config.runtime_mode, RuntimeMode::Simulation);
    let daemon = BrainstemDaemon::try_new(config.clone())?;
    assert!(daemon.restore_network().is_ok());
    config.runtime_mode = RuntimeMode::Live;
    assert!(BrainstemDaemon::try_new(config)?.restore_network().is_err());
    println!("external consumer: simulation restore OK; missing live checkpoint rejected");
    Ok(())
}
'''


@dataclass
class Gate:
    """Where evidence goes, how commands run, and what candidate is under test."""

    evidence: Path
    env: dict
    work: Path = field(default_factory=Path)
    version: str = ""
    sha: str = ""
    mode: str = ""
    timeout: int = 1800

    def record(self, entry):
        with (self.evidence / "commands.jsonl").open("a") as commands:
            commands.write(json.dumps(entry) + "\n")

    def run(self, label, args, cwd, expected=0):
        log = self.evidence / f"{label}.log"
        self.record({"label": label, "command": list(map(str, args)), "cwd": str(cwd),
                     "expected_exit": expected, "timeout_seconds": self.timeout,
                     "rustdocflags": self.env.get("RUSTDOCFLAGS", "")})
        print(f"[{label}] cwd={cwd} $ {shlex.join(map(str, args))}", flush=True)
        with log.open("w") as output:
            result = subprocess.run(
                args, cwd=cwd, env=self.env, stdout=output, stderr=subprocess.STDOUT,
                timeout=self.timeout, check=False,
            )
        text = log.read_text()
        self.record({"label": label, "exit": result.returncode})
        print(text[-4000:], flush=True)
        print(f"[{label}] exit={result.returncode}", flush=True)
        if result.returncode != expected:
            raise RuntimeError(f"{label}: expected exit {expected}, got {result.returncode}")
        return text

    def write_config(self, name, text):
        path = self.work / name
        path.write_text(text)
        shutil.copy2(path, self.evidence / name)
        return path


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def daemon_config():
    """Return (config text, control port) with three distinct loopback ports."""
    ports = set()
    while len(ports) < 3:
        ports.add(free_port())
    sub_port, pub_port, control_port = sorted(ports)
    text = f'''lif_count = 4
izh_count = 0
channels = 4
tick_rate_hz = 100
log_level = "info"
spine_sub_port = {sub_port}
spine_pub_port = {pub_port}
control_bind = "127.0.0.1:{control_port}"
model_path = "missing-checkpoint.json"
'''
    return text, control_port


def is_ready(control_port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{control_port}/readyz", timeout=0.5) as response:
            return response.status == 200 and response.read() == b"ready\n"
    except (OSError, http.client.HTTPException):
        return False


def wait_ready(process, control_port, label):
    """Return True once /readyz answers, False if the daemon exited first."""
    deadline = time.monotonic() + 10
    while process.poll() is None:
        if is_ready(control_port):
            return True
        if time.monotonic() >= deadline:
            raise RuntimeError(f"{label}: never became ready")
        time.sleep(0.05)
    return False


def lost_port_race(log, attempt, start):
    """True when this attempt's own output shows a failed bind and attempts remain."""
    output = Path(log.name).read_bytes()[start:].decode(errors="replace").lower()
    return attempt < PORT_ATTEMPTS and "address already in use" in output


def start_ready_daemon(gate, binary, label, log):
    """Spawn the simulation daemon and return it and its config once ready.

    A premature exit fails the gate unless this attempt's log shows the daemon
    lost a race for one of its freshly allocated ports; only then are new ports
    tried. Each attempt has its own config file and log section.
    """
    for attempt in itertools.count(1):
        config_text, control_port = daemon_config()
        simulation = gate.write_config(f"{label}-attempt{attempt}.toml", 'runtime_mode = "simulation"\n' + config_text)
        gate.record({"label": label, "attempt": attempt, "command": [str(binary), "--config", str(simulation)],
                     "cwd": str(gate.work)})
        log.write(f"[attempt {attempt}] config={simulation.name}\n")
        log.flush()
        start = Path(log.name).stat().st_size
        process = subprocess.Popen(
            [binary, "--config", simulation], cwd=gate.work, env=gate.env,
            stdout=log, stderr=subprocess.STDOUT,
        )
        try:
            if wait_ready(process, control_port, label):
                return process, simulation
        except BaseException:
            process.kill()
            process.wait()
            raise
        # wait_ready returned only because the daemon already exited.
        if not lost_port_race(log, attempt, start):
            raise RuntimeError(f"{label}: exited before readiness ({process.returncode})")
        print(f"[{label}] port race on attempt {attempt}; retrying with new ports", flush=True)


def check_graceful_shutdown(gate, binary, stop_signal):
    # Observe readiness, not merely survival after an arbitrary sleep. Exercise
    # both Unix shutdown signals with bounded startup and teardown.
    label = f"{gate.mode}-simulation-{stop_signal.name}"
    with (gate.evidence / f"{label}.log").open("w") as log:
        process, simulation = start_ready_daemon(gate, binary, label, log)
        try:
            process.send_signal(stop_signal)
            if process.wait(timeout=10) != 0:
                raise RuntimeError(f"{label}: did not shut down successfully")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        log.write(f"\nQUALIFIED: /readyz=200; {stop_signal.name}; graceful exit=0\n")
    print(f"[{label}] /readyz=200; graceful exit=0", flush=True)
    return simulation


def check_live_rejection(gate, binary, config_text):
    # Live is the default. Both missing and corrupt checkpoints must fail before
    # runtime startup; a successful exit or a hang must not pass this check.
    checkpoint = gate.work / "missing-checkpoint.json"
    for invalid in ("missing", "corrupt"):
        if invalid == "corrupt":
            checkpoint.write_text("not a checkpoint")
        live = gate.write_config(f"{gate.mode}-live-{invalid}.toml", config_text)
        text = gate.run(f"{gate.mode}-live-{invalid}", [binary, "--config", live], gate.work, expected=1)
        if "invalid or missing Spikenaut checkpoint" not in text:
            raise RuntimeError("live rejection did not report checkpoint failure")
    checkpoint.unlink()


def smoke(gate, binary):
    gate = replace(gate, timeout=10)
    help_text = gate.run(f"{gate.mode}-help", [binary, "--help"], gate.work)
    if "--config" not in help_text or "--version" not in help_text:
        raise RuntimeError("installed CLI is missing expected options")
    actual_version = gate.run(f"{gate.mode}-version", [binary, "--version"], gate.work)
    if actual_version.strip() != f"brainstem-daemon {gate.version}":
        raise RuntimeError("installed version does not match the package")
    for stop_signal in (signal.SIGINT, signal.SIGTERM):
        simulation = check_graceful_shutdown(gate, binary, stop_signal)
    # Live is the default: the same ports and model_path without runtime_mode.
    check_live_rejection(gate, binary, simulation.read_text().removeprefix('runtime_mode = "simulation"\n'))
    return simulation


def run_consumer(gate, package, features, simulation):
    mode = gate.mode
    consumer = gate.work / f"consumer-{mode}"
    (consumer / "src").mkdir(parents=True)
    (consumer / "Cargo.toml").write_text(f'''[package]
name = "package-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
brainstem-daemon = {{ path = {json.dumps(str(package))}, features = {json.dumps(features[1:] if features else [])} }}
''')
    (consumer / "src" / "main.rs").write_text(CONSUMER_MAIN)
    shutil.copytree(consumer, gate.evidence / f"consumer-{mode}")
    gate.run(f"{mode}-consumer-lock", CARGO + ["generate-lockfile"], consumer)
    shutil.copy2(consumer / "Cargo.lock", gate.evidence / f"consumer-{mode}" / "Cargo.lock")
    gate.run(f"{mode}-consumer", CARGO + ["run", "--locked", "--", str(simulation)], consumer)


def qualify_mode(gate, package, mode, features):
    gate = replace(gate, mode=mode)
    # Never reuse checkout artifacts, or artifacts from the other backend.
    gate.env["CARGO_TARGET_DIR"] = str(gate.work / f"target-{mode}")
    for command in ("build", "test"):
        gate.run(f"{mode}-{command}", CARGO + [command, "--locked"] + features, package)
    docs = replace(gate, env=dict(gate.env, RUSTDOCFLAGS="-D warnings"))
    docs.run(f"{mode}-rustdoc", CARGO + ["doc", "--locked", "--no-deps"] + features, package)
    root = gate.work / f"install-{mode}"
    gate.run(f"{mode}-install", CARGO + ["install", "--locked", "--path", str(package), "--root", str(root)] + features, gate.work)
    simulation = smoke(gate, root / "bin" / "brainstem-daemon")
    run_consumer(gate, package, features, simulation)


def required_files(gate, repo):
    tracked = gate.run("git-sources", ["git", "ls-files", "src", "tests"], repo)
    return set(tracked.splitlines()) | {
        "Cargo.toml", "Cargo.toml.orig", "Cargo.lock", "README.md",
        "LICENSE-MIT", "LICENSE-APACHE-2.0", "rust-toolchain.toml", ".cargo_vcs_info.json",
    }


def check_package_list(gate, repo, required):
    contents = gate.run("package-list", CARGO + ["package", "--locked", "--list"], repo)
    listed = set(contents.splitlines())
    missing = required - listed
    if missing:
        raise RuntimeError(f"package is missing required files: {sorted(missing)}")
    if any(path.startswith((".github/", ".devin/", "docs/", "target/")) for path in listed):
        raise RuntimeError("package includes repository-only or generated files")


def extract_archive(gate, archive, package_name, required):
    extracted = gate.work / "extracted"
    extracted.mkdir()
    with tarfile.open(archive) as tar:
        tar.extractall(extracted, filter="data")
    package = extracted / package_name
    vcs = json.loads((package / ".cargo_vcs_info.json").read_text())
    if vcs["git"]["sha1"] != gate.sha or vcs["git"].get("dirty", False):
        raise RuntimeError("archive VCS identity does not match the clean candidate SHA")
    if not all((package / path).is_file() for path in required):
        raise RuntimeError("extracted archive is missing required files")
    return package


def candidate_env():
    env = os.environ.copy()
    # Do not inherit target/Rust flags that could mask a packaged build failure.
    for key in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS", "RUST_LOG"):
        env.pop(key, None)
    env.update(CC="gcc", CXX="g++", CARGO_TERM_COLOR="never")
    return env


def candidate_sha(gate, repo):
    sha = gate.run("git-sha", ["git", "rev-parse", "HEAD"], repo).strip()
    dirty = gate.run("git-status", ["git", "status", "--porcelain", "--untracked-files=no"], repo)
    if dirty.strip():
        raise RuntimeError("qualification requires a clean committed checkout")
    gate.run("rustc", ["rustc", "+1.98.1", "--version", "--verbose"], repo)
    gate.run("cargo", CARGO + ["--version"], repo)
    return sha


def qualify(evidence):
    repo = Path(__file__).resolve().parents[2]
    gate = Gate(evidence, candidate_env())
    sha = gate.sha = candidate_sha(gate, repo)
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())
    gate.version = manifest["package"]["version"]
    package_name = f"{manifest['package']['name']}-{gate.version}"
    identity = {"sha": sha, "package": package_name, "status": "running"}

    def write_identity():
        (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")

    write_identity()

    with tempfile.TemporaryDirectory(prefix="brainstem-package-") as temporary:
        gate.work = Path(temporary)
        gate.env["CARGO_TARGET_DIR"] = str(gate.work / "target")
        required = required_files(gate, repo)
        check_package_list(gate, repo, required)
        gate.run("package", CARGO + ["package", "--locked"], repo)
        archive = gate.work / "target" / "package" / f"{package_name}.crate"
        shutil.copy2(archive, evidence / archive.name)
        identity["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        package = extract_archive(gate, archive, package_name, required)
        write_identity()

        for mode, features in MODES:
            qualify_mode(gate, package, mode, features)

        # Use the clean source checkout for Cargo's publish path, which itself
        # packages and verifies it. Nothing is uploaded; no bypass flags are used.
        gate.env["CARGO_TARGET_DIR"] = str(gate.work / "publish-target")
        gate.run("publish-dry-run", CARGO + ["publish", "--dry-run", "--locked"], repo)
        identity["status"] = "passed"
        write_identity()
    print(f"PASS: packaged release qualification for {sha}", flush=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: qualify-package.py EVIDENCE_DIRECTORY (must be empty)")
    evidence = Path(sys.argv[1]).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    if any(evidence.iterdir()):
        sys.exit("evidence directory must be empty; do not mix candidate runs")
    try:
        qualify(evidence)
    except Exception as error:
        (evidence / "failure.txt").write_text(f"{type(error).__name__}: {error}\n")
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
