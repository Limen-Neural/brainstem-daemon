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
import tempfile
import time
import tomllib
import urllib.request

CARGO = ["cargo", "+1.99.0"]
# Same defines as `.cargo/config.toml`. The external consumer is a different
# Cargo root, so it does not load that file; zeromq-src is built before this
# crate and cannot inherit a flag from this crate's build script.
LIBZMQ_ALIGN_DEFINES = "-DHAVE_POSIX_MEMALIGN -DZMQ_CACHELINE_SIZE=64"
MODES = (("stub", []), ("corpus-ipc", ["--features", "corpus-ipc"]))
# Attempts to start the daemon when a freshly allocated loopback port was
# claimed by another process between allocation and the daemon's bind.
PORT_ATTEMPTS = 3
# Target/Rust flags that could mask a packaged build failure if inherited.
INHERITED_BUILD_ENV = ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                       "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS", "RUST_LOG")
REPOSITORY_ONLY = (".github/", ".devin/", ".agents/", "docs/", "target/",
                   "experiments/usefulness/results/")
SIMULATION_HEADER = 'runtime_mode = "simulation"\n'

CONSUMER_MANIFEST = '''[package]
name = "package-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
brainstem-daemon = {{ path = {path}, features = {features} }}
'''

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


def require(condition, message):
    """Fail the gate with message unless condition holds."""
    if not condition:
        raise RuntimeError(message)


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
    # Sidecar for a live-mode start; None means the simulation config.
    fixture: Path = None

    def record(self, entry):
        with (self.evidence / "commands.jsonl").open("a") as commands:
            commands.write(json.dumps(entry) + "\n")

    def run(self, label, args, cwd, expected=0):
        self.record({"label": label, "command": list(map(str, args)), "cwd": str(cwd),
                     "expected_exit": expected, "timeout_seconds": self.timeout,
                     "rustdocflags": self.env.get("RUSTDOCFLAGS", "")})
        print(f"[{label}] cwd={cwd} $ {shlex.join(map(str, args))}", flush=True)
        returncode, text = self.capture(label, args, cwd)
        self.record({"label": label, "exit": returncode})
        print(text[-4000:], flush=True)
        print(f"[{label}] exit={returncode}", flush=True)
        require(returncode == expected, f"{label}: expected exit {expected}, got {returncode}")
        return text

    def capture(self, label, args, cwd):
        """Run args in cwd with combined output in the label's log."""
        log = self.evidence / f"{label}.log"
        with log.open("w") as output:
            result = subprocess.run(
                args, cwd=cwd, env=self.env, stdout=output, stderr=subprocess.STDOUT,
                timeout=self.timeout, check=False,
            )
        return result.returncode, log.read_text()

    def write_config(self, name, text):
        path = self.work / name
        path.write_text(text)
        shutil.copy2(path, self.evidence / name)
        return path


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def daemon_config(model_path="missing-checkpoint.json", size=4):
    """Return (config text, control port) with three distinct loopback ports."""
    ports = set()
    while len(ports) < 3:
        ports.add(free_port())
    sub_port, pub_port, control_port = sorted(ports)
    text = f'''lif_count = {size}
izh_count = 0
channels = {size}
tick_rate_hz = 100
log_level = "info"
spine_sub_port = {sub_port}
spine_pub_port = {pub_port}
control_bind = "127.0.0.1:{control_port}"
model_path = {json.dumps(str(model_path))}
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
        require(time.monotonic() < deadline, f"{label}: never became ready")
        time.sleep(0.05)
    return False


def kill(process):
    """Kill the daemon and reap it."""
    process.kill()
    process.wait()


def ready_or_kill(process, control_port, label):
    """Wait for readiness, killing the daemon if waiting itself fails."""
    try:
        return wait_ready(process, control_port, label)
    except BaseException:
        kill(process)
        raise


def lost_port_race(log, attempt, start):
    """Return whether this attempt's own output shows a failed bind with attempts left."""
    output = Path(log.name).read_bytes()[start:].decode(errors="replace").lower()
    return attempt < PORT_ATTEMPTS and "address already in use" in output


def attempt_config(gate, binary, label, attempt):
    """Write and record a fresh config: simulation, or live on gate.fixture if set."""
    config_text, control_port = daemon_config(*(() if gate.fixture is None else (gate.fixture, 16)))
    header = SIMULATION_HEADER if gate.fixture is None else ""
    simulation = gate.write_config(f"{label}-attempt{attempt}.toml", header + config_text)
    gate.record({"label": label, "attempt": attempt, "command": [str(binary), "--config", str(simulation)],
                 "cwd": str(gate.work)})
    return simulation, control_port


def spawn_daemon(gate, binary, config, log):
    """Start the installed daemon on config with combined output in log."""
    # shell=False, so spaces and other characters in a temp path are literal.
    # The evidence directory from argv shares this Gate, which makes intrafile
    # taint treat these paths as controlled; they are paths we just created.
    return subprocess.Popen(
        [os.fsdecode(binary), "--config", os.fsdecode(config)],  # nosemgrep: dangerous-subprocess-use-tainted-env-args
        cwd=gate.work, env=gate.env, stdout=log, stderr=subprocess.STDOUT,
    )


def start_ready_daemon(gate, binary, label, log):
    """Spawn the simulation daemon and return it and its config once ready."""
    # A premature exit fails the gate unless this attempt's own log shows a
    # lost port race. Only then are new ports tried, and each attempt keeps
    # its own config file and log section.
    for attempt in itertools.count(1):
        simulation, control_port = attempt_config(gate, binary, label, attempt)
        log.write(f"[attempt {attempt}] config={simulation.name}\n")
        log.flush()
        start = Path(log.name).stat().st_size
        process = spawn_daemon(gate, binary, simulation, log)
        if ready_or_kill(process, control_port, label):
            return process, simulation
        # wait_ready returned only because the daemon already exited.
        require(lost_port_race(log, attempt, start), f"{label}: exited before readiness ({process.returncode})")
        print(f"[{label}] port race on attempt {attempt}; retrying with new ports", flush=True)


def stop_gracefully(process, stop_signal, label):
    """Send stop_signal and require a successful exit within ten seconds."""
    try:
        process.send_signal(stop_signal)
        require(process.wait(timeout=10) == 0, f"{label}: did not shut down successfully")
    finally:
        if process.poll() is None:
            kill(process)


def check_graceful_shutdown(gate, binary, stop_signal):
    # Observe readiness, not merely survival after an arbitrary sleep. Exercise
    # both Unix shutdown signals with bounded startup and teardown.
    label = f"{gate.mode}-simulation-{stop_signal.name}"
    with (gate.evidence / f"{label}.log").open("w") as log:
        process, simulation = start_ready_daemon(gate, binary, label, log)
        stop_gracefully(process, stop_signal, label)
        log.write(f"\nQUALIFIED: /readyz=200; {stop_signal.name}; graceful exit=0\n")
    print(f"[{label}] /readyz=200; graceful exit=0", flush=True)
    return simulation


def reject_live_checkpoint(gate, binary, config_text, invalid):
    """Require the live daemon to exit 1 with a checkpoint-specific error."""
    live = gate.write_config(f"{gate.mode}-live-{invalid}.toml", config_text)
    text = gate.run(f"{gate.mode}-live-{invalid}", [binary, "--config", live], gate.work, expected=1)
    require("invalid or missing Spikenaut checkpoint" in text, "live rejection did not report checkpoint failure")


def check_live_rejection(gate, binary, config_text):
    # Live is the default. Both missing and corrupt checkpoints must fail before
    # runtime startup; a successful exit or a hang must not pass this check.
    checkpoint = gate.work / "missing-checkpoint.json"
    reject_live_checkpoint(gate, binary, config_text, "missing")
    checkpoint.write_text("not a checkpoint")
    reject_live_checkpoint(gate, binary, config_text, "corrupt")
    checkpoint.unlink()


def check_live_fixture(gate, binary, package):
    """Require a live-mode start on the packaged nonblank fixture to become ready and exit 0."""
    fixture = package / "tests" / "fixtures" / "live" / "snn_model.json"
    require(fixture.is_file(), "packaged live fixture is missing")
    label = f"{gate.mode}-live-fixture"
    with (gate.evidence / f"{label}.log").open("w") as log:
        process, _ = start_ready_daemon(replace(gate, fixture=fixture), binary, label, log)
        stop_gracefully(process, signal.SIGTERM, label)
        log.write("\nQUALIFIED: live fixture; /readyz=200; SIGTERM; graceful exit=0\n")
    print(f"[{label}] live restore; /readyz=200; graceful exit=0", flush=True)


def check_cli(gate, binary):
    """Require the installed CLI's options and the packaged version."""
    help_text = gate.run(f"{gate.mode}-help", [binary, "--help"], gate.work)
    require("--config" in help_text and "--version" in help_text, "installed CLI is missing expected options")
    actual_version = gate.run(f"{gate.mode}-version", [binary, "--version"], gate.work)
    require(actual_version.strip() == f"brainstem-daemon {gate.version}",
            "installed version does not match the package")


def smoke(gate, binary, package):
    gate = replace(gate, timeout=10)
    check_cli(gate, binary)
    check_graceful_shutdown(gate, binary, signal.SIGINT)
    simulation = check_graceful_shutdown(gate, binary, signal.SIGTERM)
    # Live is the default: the same ports and model_path without runtime_mode.
    check_live_rejection(gate, binary, simulation.read_text().removeprefix(SIMULATION_HEADER))
    check_live_fixture(gate, binary, package)
    return simulation


def write_consumer(gate, package, features):
    """Write a tiny external crate that depends on the extracted package."""
    consumer = gate.work / f"consumer-{gate.mode}"
    (consumer / "src").mkdir(parents=True)
    (consumer / "Cargo.toml").write_text(CONSUMER_MANIFEST.format(
        path=json.dumps(str(package)), features=json.dumps(features[1:])))
    (consumer / "src" / "main.rs").write_text(CONSUMER_MAIN)
    return consumer


def run_consumer(gate, package, features, simulation):
    mode = gate.mode
    consumer = write_consumer(gate, package, features)
    shutil.copytree(consumer, gate.evidence / f"consumer-{mode}")
    gate.run(f"{mode}-consumer-lock", CARGO + ["generate-lockfile"], consumer)
    shutil.copy2(consumer / "Cargo.lock", gate.evidence / f"consumer-{mode}" / "Cargo.lock")
    gate.run(f"{mode}-consumer", CARGO + ["run", "--locked", "--", str(simulation)], consumer)


def build_and_document(gate, package, features):
    """Build and test the extracted package, then deny rustdoc warnings."""
    gate.run(f"{gate.mode}-build", CARGO + ["build", "--locked"] + features, package)
    gate.run(f"{gate.mode}-test", CARGO + ["test", "--locked"] + features, package)
    docs = replace(gate, env=dict(gate.env, RUSTDOCFLAGS="-D warnings"))
    docs.run(f"{gate.mode}-rustdoc", CARGO + ["doc", "--locked", "--no-deps"] + features, package)


def install(gate, package, features):
    """Install the extracted package into an isolated root; return the binary."""
    root = gate.work / f"install-{gate.mode}"
    gate.run(f"{gate.mode}-install",
             CARGO + ["install", "--locked", "--path", str(package), "--root", str(root)] + features, gate.work)
    return root / "bin" / "brainstem-daemon"


def append_libzmq_align_defines(env):
    """Keep caller compiler flags and add the libzmq alignment defines."""
    for key in ("CFLAGS", "CXXFLAGS"):
        env[key] = " ".join(part for part in (env.get(key, ""), LIBZMQ_ALIGN_DEFINES) if part)


def qualify_mode(gate, package, mode, features):
    env = dict(gate.env)
    # Append, so a caller-supplied CFLAGS/CXXFLAGS is not replaced. The
    # extracted package also loads `.cargo/config.toml`; the consumer crate
    # does not, and needs these variables in the process environment.
    if mode == "corpus-ipc":
        append_libzmq_align_defines(env)
    gate = replace(gate, mode=mode, env=env)
    # Never reuse checkout artifacts, or artifacts from the other backend.
    gate.env["CARGO_TARGET_DIR"] = str(gate.work / f"target-{mode}")
    build_and_document(gate, package, features)
    simulation = smoke(gate, install(gate, package, features), package)
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
    require(not missing, f"package is missing required files: {sorted(missing)}")
    require(not any(path.startswith(REPOSITORY_ONLY) for path in listed),
            "package includes repository-only or generated files")


def package_archive(gate, repo, package_name):
    """Check the package list, then build and retain the .crate."""
    required = required_files(gate, repo)
    check_package_list(gate, repo, required)
    gate.run("package", CARGO + ["package", "--locked"], repo)
    archive = gate.work / "target" / "package" / f"{package_name}.crate"
    shutil.copy2(archive, gate.evidence / archive.name)
    return archive, required


def check_vcs_identity(gate, package):
    """Require the archive's embedded Git identity to be the clean candidate SHA."""
    git = json.loads((package / ".cargo_vcs_info.json").read_text())["git"]
    require(git["sha1"] == gate.sha and not git.get("dirty", False),
            "archive VCS identity does not match the clean candidate SHA")


def extract_archive(gate, archive, package_name, required):
    extracted = gate.work / "extracted"
    extracted.mkdir()
    # The "data" extraction filter keeps every member and link inside extracted.
    shutil.unpack_archive(archive, extracted, "gztar", filter="data")
    package = extracted / package_name
    check_vcs_identity(gate, package)
    require(all((package / path).is_file() for path in required), "extracted archive is missing required files")
    return package


def candidate_env():
    env = os.environ.copy()
    for key in INHERITED_BUILD_ENV:
        env.pop(key, None)
    env.update(CC="gcc", CXX="g++", CARGO_TERM_COLOR="never")
    return env


def candidate_sha(gate, repo):
    sha = gate.run("git-sha", ["git", "rev-parse", "HEAD"], repo).strip()
    dirty = gate.run("git-status", ["git", "status", "--porcelain", "--untracked-files=no"], repo)
    require(not dirty.strip(), "qualification requires a clean committed checkout")
    gate.run("rustc", ["rustc", "+1.99.0", "--version", "--verbose"], repo)
    gate.run("cargo", CARGO + ["--version"], repo)
    return sha


def candidate_package(gate, repo):
    """Record the manifest version on the gate and return the package's archive name."""
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())
    gate.version = manifest["package"]["version"]
    return f"{manifest['package']['name']}-{gate.version}"


def write_identity(evidence, identity):
    (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")


def publish_dry_run(gate, repo):
    # Use the clean source checkout for Cargo's publish path, which itself
    # packages and verifies it. Nothing is uploaded; no bypass flags are used.
    gate.env["CARGO_TARGET_DIR"] = str(gate.work / "publish-target")
    gate.run("publish-dry-run", CARGO + ["publish", "--dry-run", "--locked"], repo)


def qualify(evidence):
    repo = Path(__file__).resolve().parents[2]
    gate = Gate(evidence, candidate_env())
    sha = gate.sha = candidate_sha(gate, repo)
    package_name = candidate_package(gate, repo)
    identity = {"sha": sha, "package": package_name, "status": "running"}
    write_identity(evidence, identity)

    with tempfile.TemporaryDirectory(prefix="brainstem-package-") as temporary:
        gate.work = Path(temporary)
        gate.env["CARGO_TARGET_DIR"] = str(gate.work / "target")
        archive, required = package_archive(gate, repo, package_name)
        identity["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        package = extract_archive(gate, archive, package_name, required)
        write_identity(evidence, identity)

        for mode, features in MODES:
            qualify_mode(gate, package, mode, features)

        publish_dry_run(gate, repo)
        identity["status"] = "passed"
        write_identity(evidence, identity)
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
