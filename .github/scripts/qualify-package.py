#!/usr/bin/env python3
"""Linux release gate for the shipped archive, not the repository build tree."""

import hashlib
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
import urllib.error
import urllib.request


def run(label, args, cwd, evidence, env, expected=0, timeout=1800):
    log = evidence / f"{label}.log"
    record = {"label": label, "command": list(map(str, args)), "cwd": str(cwd),
              "expected_exit": expected, "timeout_seconds": timeout,
              "rustdocflags": env.get("RUSTDOCFLAGS", "")}
    with (evidence / "commands.jsonl").open("a") as commands:
        commands.write(json.dumps(record) + "\n")
    print(f"[{label}] cwd={cwd} $ {shlex.join(map(str, args))}", flush=True)
    with log.open("w") as output:
        result = subprocess.run(
            args, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT,
            timeout=timeout, check=False,
        )
    text = log.read_text()
    with (evidence / "commands.jsonl").open("a") as commands:
        commands.write(json.dumps({"label": label, "exit": result.returncode}) + "\n")
    print(text[-4000:], flush=True)
    print(f"[{label}] exit={result.returncode}", flush=True)
    if result.returncode != expected:
        raise RuntimeError(f"{label}: expected exit {expected}, got {result.returncode}")
    return text


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def smoke(binary, mode, version, work, evidence, env):
    help_text = run(f"{mode}-help", [binary, "--help"], work, evidence, env, timeout=10)
    if "--config" not in help_text or "--version" not in help_text:
        raise RuntimeError("installed CLI is missing expected options")
    actual_version = run(f"{mode}-version", [binary, "--version"], work, evidence, env, timeout=10)
    if actual_version.strip() != f"brainstem-daemon {version}":
        raise RuntimeError("installed version does not match the package")

    # Allocate distinct ports; no external publisher is needed for simulation.
    ports = set()
    while len(ports) < 3:
        ports.add(free_port())
    sub_port, pub_port, control_port = sorted(ports)
    config_text = f'''lif_count = 4
izh_count = 0
channels = 4
tick_rate_hz = 100
log_level = "info"
spine_sub_port = {sub_port}
spine_pub_port = {pub_port}
control_bind = "127.0.0.1:{control_port}"
model_path = "missing-checkpoint.json"
'''
    simulation = work / f"{mode}-simulation.toml"
    simulation.write_text('runtime_mode = "simulation"\n' + config_text)
    shutil.copy2(simulation, evidence / simulation.name)
    # Observe readiness, not merely survival after an arbitrary sleep. Exercise
    # both Unix shutdown signals with bounded startup and teardown.
    for stop_signal in (signal.SIGINT, signal.SIGTERM):
        label = f"{mode}-simulation-{stop_signal.name}"
        with (evidence / f"{label}.log").open("w") as log:
            process = subprocess.Popen(
                [binary, "--config", simulation], cwd=work, env=env,
                stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 10
                while True:
                    if process.poll() is not None:
                        raise RuntimeError(f"{label}: exited before readiness ({process.returncode})")
                    try:
                        with urllib.request.urlopen(
                            f"http://127.0.0.1:{control_port}/readyz", timeout=0.5,
                        ) as response:
                            if response.status == 200 and response.read() == b"ready\n":
                                break
                    except (urllib.error.URLError, TimeoutError):
                        pass
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"{label}: never became ready")
                    time.sleep(0.05)
                process.send_signal(stop_signal)
                if process.wait(timeout=10) != 0:
                    raise RuntimeError(f"{label}: did not shut down successfully")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
            log.write(f"\nQUALIFIED: /readyz=200; {stop_signal.name}; graceful exit=0\n")
        print(f"[{label}] /readyz=200; graceful exit=0", flush=True)

    # Live is the default. Both missing and corrupt checkpoints must fail before
    # runtime startup; a successful exit or a hang must not pass this check.
    for invalid in ("missing", "corrupt"):
        checkpoint = work / "missing-checkpoint.json"
        if invalid == "corrupt":
            checkpoint.write_text("not a checkpoint")
        live = work / f"{mode}-live-{invalid}.toml"
        live.write_text(config_text)
        shutil.copy2(live, evidence / live.name)
        text = run(
            f"{mode}-live-{invalid}", [binary, "--config", live], work,
            evidence, env, expected=1, timeout=10,
        )
        if "invalid or missing Spikenaut checkpoint" not in text:
            raise RuntimeError("live rejection did not report checkpoint failure")
    checkpoint.unlink()


def qualify(evidence):
    repo = Path(__file__).resolve().parents[2]
    env = os.environ.copy()
    # Do not inherit target/Rust flags that could mask a packaged build failure.
    for key in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS", "RUST_LOG"):
        env.pop(key, None)
    env.update(CC="gcc", CXX="g++", CARGO_TERM_COLOR="never")
    cargo = ["cargo", "+1.98.1"]
    sha = run("git-sha", ["git", "rev-parse", "HEAD"], repo, evidence, env).strip()
    dirty = run("git-status", ["git", "status", "--porcelain", "--untracked-files=no"], repo, evidence, env)
    if dirty.strip():
        raise RuntimeError("qualification requires a clean committed checkout")
    run("rustc", ["rustc", "+1.98.1", "--version", "--verbose"], repo, evidence, env)
    run("cargo", cargo + ["--version"], repo, evidence, env)
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())
    package_name = f"{manifest['package']['name']}-{manifest['package']['version']}"
    identity = {"sha": sha, "package": package_name, "status": "running"}
    (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")

    with tempfile.TemporaryDirectory(prefix="brainstem-package-") as temporary:
        work = Path(temporary)
        env["CARGO_TARGET_DIR"] = str(work / "target")
        contents = run("package-list", cargo + ["package", "--locked", "--list"], repo, evidence, env)
        tracked = run("git-sources", ["git", "ls-files", "src", "tests"], repo, evidence, env)
        required = set(tracked.splitlines()) | {
            "Cargo.toml", "Cargo.toml.orig", "Cargo.lock", "README.md",
            "LICENSE-MIT", "LICENSE-APACHE-2.0", "rust-toolchain.toml", ".cargo_vcs_info.json",
        }
        listed = set(contents.splitlines())
        missing = required - listed
        if missing:
            raise RuntimeError(f"package is missing required files: {sorted(missing)}")
        if any(path.startswith((".github/", ".devin/", "docs/", "target/")) for path in listed):
            raise RuntimeError("package includes repository-only or generated files")
        run("package", cargo + ["package", "--locked"], repo, evidence, env)
        archive = work / "target" / "package" / f"{package_name}.crate"
        shutil.copy2(archive, evidence / archive.name)
        identity["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        extracted = work / "extracted"
        extracted.mkdir()
        with tarfile.open(archive) as tar:
            tar.extractall(extracted, filter="data")
        package = extracted / package_name
        vcs = json.loads((package / ".cargo_vcs_info.json").read_text())
        if vcs["git"]["sha1"] != sha or vcs["git"].get("dirty", False):
            raise RuntimeError("archive VCS identity does not match the clean candidate SHA")
        if not all((package / path).is_file() for path in required):
            raise RuntimeError("extracted archive is missing required files")
        (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")

        for mode, features in (("stub", []), ("corpus-ipc", ["--features", "corpus-ipc"])):
            # Never reuse checkout artifacts, or artifacts from the other backend.
            env["CARGO_TARGET_DIR"] = str(work / f"target-{mode}")
            for command in ("build", "test"):
                run(f"{mode}-{command}", cargo + [command, "--locked"] + features, package, evidence, env)
            docs_env = dict(env, RUSTDOCFLAGS="-D warnings")
            run(f"{mode}-rustdoc", cargo + ["doc", "--locked", "--no-deps"] + features, package, evidence, docs_env)
            root = work / f"install-{mode}"
            run(
                f"{mode}-install", cargo + ["install", "--locked", "--path", str(package), "--root", str(root)] + features,
                work, evidence, env,
            )
            smoke(root / "bin" / "brainstem-daemon", mode, manifest["package"]["version"], work, evidence, env)
            consumer = work / f"consumer-{mode}"
            (consumer / "src").mkdir(parents=True)
            (consumer / "Cargo.toml").write_text(f'''[package]
name = "package-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
brainstem-daemon = {{ path = {json.dumps(str(package))}, features = {json.dumps(features[1:] if features else [])} }}
''')
            (consumer / "src" / "main.rs").write_text('''use brainstem_daemon::{BrainstemDaemon, DaemonConfig, RuntimeMode};

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
''')
            shutil.copytree(consumer, evidence / f"consumer-{mode}")
            run(f"{mode}-consumer-lock", cargo + ["generate-lockfile"], consumer, evidence, env)
            shutil.copy2(consumer / "Cargo.lock", evidence / f"consumer-{mode}" / "Cargo.lock")
            run(
                f"{mode}-consumer", cargo + ["run", "--locked", "--", str(work / f"{mode}-simulation.toml")],
                consumer, evidence, env,
            )

        # Use the clean source checkout for Cargo's publish path, which itself
        # packages and verifies it. Nothing is uploaded; no bypass flags are used.
        env["CARGO_TARGET_DIR"] = str(work / "publish-target")
        run("publish-dry-run", cargo + ["publish", "--dry-run", "--locked"], repo, evidence, env)
        identity["status"] = "passed"
        (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
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
