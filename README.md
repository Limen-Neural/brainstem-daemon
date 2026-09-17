# Brainstem Daemon

[![CI](https://github.com/Limen-Neural/brainstem-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/Limen-Neural/brainstem-daemon/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Crates.io](https://img.shields.io/crates/v/brainstem-daemon.svg)](https://crates.io/crates/brainstem-daemon)
[![docs.rs](https://docs.rs/brainstem-daemon/badge.svg)](https://docs.rs/brainstem-daemon)

Headless spiking neural-network runtime written in Rust.

> **Note**  
> Training / weight-optimization lives in the separate `plasticity-lab` project; `brainstem-daemon` is *inference-only*.

---

## Features

- Modular `neuromod::SpikingNetwork` core (CPU)
- Optional **ZeroMQ PUB/SUB** networking via `corpus-ipc`
- Headless **`brainstem-daemon`** binary for background execution

---

## Install

From crates.io (binary):

```bash
cargo install brainstem-daemon
# Optional ZeroMQ / corpus-ipc backend (needs a C/C++ toolchain; system libzmq is optional):
cargo install brainstem-daemon --features corpus-ipc
```

As a library dependency (crates.io, not a git pin):

```toml
brainstem-daemon = "0.1"
# Optional ZeroMQ backend:
brainstem-daemon = { version = "0.1", features = ["corpus-ipc"] }
```

The optional `corpus-ipc` feature depends on the published `corpus-ipc` crate (`0.1`, `features = ["zmq"]`). The default path uses the in-memory stub backend and does not need ZeroMQ.

## Building

Requires **Rust 1.97.1** (`rust-toolchain.toml`) for the default stub crate. Enabling `--features corpus-ipc` also compiles published `corpus-ipc` 0.1, whose MSRV is **1.98.1** — use that toolchain (or `cargo … --ignore-rust-version`) for the feature build.

```bash
# Release build, default stub backend (no libzmq)
cargo build --release --bin brainstem-daemon

# Optional ZeroMQ / corpus-ipc backend
cargo build --release --bin brainstem-daemon --features corpus-ipc
```

The binary will be located at `target/release/brainstem-daemon`. Feature flag vs backend vs which config keys apply is in [Backends (temporary)](#backends-temporary).

### Cargo profiles

This crate sets the following Cargo profiles in `Cargo.toml`, aligned with the
`neuromod` profile pattern:

| Profile | Command | Intent |
|---|---|---|
| `dev` | `cargo build` / `cargo run` | Fast compile, full debug info, overflow checks |
| `release` | `cargo build --release` | Optimized binary, thin LTO, debuginfo stripped |
| `release-with-debug` | `cargo build --profile release-with-debug` | Release optimizations with debug symbols kept, for `perf` / flamegraphs / `tracing` on the 1 kHz tick loop |
| `test` | `cargo test` | Debuggable tests with overflow checks |
| `bench` | `cargo bench` | Same optimization level as release |

```bash
cargo build                              # profile.dev
cargo build --release                    # profile.release
cargo build --profile release-with-debug # profile.release-with-debug
cargo test                               # profile.test
cargo bench                              # profile.bench (when benches exist)
```

---

## Configuration
`brainstem-daemon` expects a **TOML** file; default path: `~/.config/soma/daemon.toml` (override with `--config`).

```toml
# ~/.config/soma/daemon.toml

# Engine
lif_count      = 16        # LIF neurons
izh_count      = 5         # Izhikevich neurons
channels       = 16        # expected input channels
model_path     = "~/models/soma16.mem" # literal path; `~` is not expanded

# Runtime
tick_rate_hz   = 1000      # loop frequency
log_level      = "info"    # error|warn|info|debug|trace

# ZMQ (still required in TOML; no-ops under the default stub backend)
spine_sub_port = 5555      # stimuli in
spine_pub_port = 5556      # spikes out

# Service registry (optional; empty by default)
# Trading/mining-specific adapters are intentionally excluded from defaults.
[[services]]
name = "telemetry"
enabled = true

[[services]]
name = "critic-ipc"
enabled = true
```

### Backends (temporary)

Default Cargo features are empty (`default = []` in `Cargo.toml`). That path uses the in-memory **stub** backend (`StubStimulusSource` + `NoopSpikeSink`) and does **not** need ZeroMQ. The optional `corpus-ipc` feature (same as `--all-features` today) pulls `corpus-ipc` **from crates.io** (`0.1`, `features = ["zmq"]`) plus this crate's optional `zmq` dependency. Published `corpus-ipc` compiles libzmq via `zmq-sys` / `zeromq-src` (a C++ compiler is required; a system `libzmq` package is not). It does not vendor ZeroMQ as a git submodule.

`DaemonConfig` deserialization is **not** feature-gated: `spine_sub_port`, `spine_pub_port`, and `model_path` are still required in TOML even on the stub path (`services` is the only optional field, defaulting to empty). Effect at runtime depends on which backend is **wired**.

#### Feature truth table

| Cargo flags | Wired backend | `libzmq` | Binary (`brainstem-daemon`) | Library `BrainstemDaemon::new()` / `try_new()` |
|---|---|---|---|---|
| default / `--no-default-features` | stub | not required | no sockets; logs `🔌 Using stub backend` | stub |
| `--features corpus-ipc` | ZMQ / `corpus-ipc` | required | SUB via env, PUB on `spine_pub_port`; logs `📡 Using ZMQ corpus-ipc backend` | **still stub** |
| `--all-features` | same as `corpus-ipc` | required | same as `--features corpus-ipc` | **still stub** |

Enabling the feature does **not** change `BrainstemDaemon::new()` or `try_new()`. Those always inject `BackendPair::stub()`. Only `src/bin/brainstem_daemon.rs` constructs `ZmqStimulusSource` + `ZmqSpikeSink` when `corpus-ipc` is on.

Library users who want live ZMQ must build that pair themselves under `#[cfg(feature = "corpus-ipc")]` and pass it to `with_backend` / `try_with_backend`. Call `StimulusSource::initialize(...)` on the source first (as the binary does). Neither constructor nor `run` calls `initialize`; skipping it makes ingress fail with `ZmqIpcBackend not initialized`.

#### Config keys and env vars

| Setting | Stub (default binary / `::new()`) | `corpus-ipc` binary |
|---|---|---|
| `lif_count`, `izh_count`, `channels` | used (network dimensions) | used |
| `tick_rate_hz` | used | used |
| `log_level` | binary tracing init only; unused by `::new()` / `run` | binary tracing init only; unused by `::new()` / `run` |
| `services` | used (`ServiceRegistry`) | used |
| `spine_sub_port` | parsed, **no-op** | sets `CORPUS_IPC_ZMQ_READOUT_IPC` to `tcp://127.0.0.1:<port>` (also sets legacy `SPIKENAUT_ZMQ_READOUT_IPC` for compatibility) |
| `spine_pub_port` | parsed, **no-op** | binds ZMQ PUB `tcp://*:<port>` |
| `model_path` | parsed, **no-op** (`StubStimulusSource::initialize` ignores it) | passed literally to `initialize` (no `~` expansion); published `ZmqIpcBackend` currently ignores `_model_path` |

**Settings that only take effect with `corpus-ipc`** (the `brainstem-daemon` binary built `--features corpus-ipc`):

- `spine_sub_port` (drives `CORPUS_IPC_ZMQ_READOUT_IPC`)
- `spine_pub_port`
- `CORPUS_IPC_ZMQ_READOUT_IPC` (const `CORPUS_IPC_READOUT_ENV`; this is what published `ZmqIpcBackend::initialize` reads)

**Passed through / set, but currently unused by published `corpus-ipc` 0.1:**

- `model_path` (literal filesystem path; `~` is not expanded; passed to `initialize`, which names the argument `_model_path` and does not consume it)
- `SPIKENAUT_ZMQ_READOUT_IPC` (const `LEGACY_SPIKENAUT_READOUT_ENV`; the binary still sets this alongside `CORPUS_IPC_ZMQ_READOUT_IPC` for older tooling; published `corpus-ipc` does not read it)

Under stub those TOML keys are still parsed. The env vars are unset by the default binary. Nothing in this crate reads them without the `corpus-ipc` feature.

The stub backend is always safe for core library builds, tests, and simulation. Example (feature-independent):

```rust
use brainstem_daemon::{BrainstemDaemon, DaemonConfig, BackendPair};

let cfg: DaemonConfig = /* ... */;
let daemon = BrainstemDaemon::with_backend(cfg, BackendPair::stub());
```

> **Note (temporary):** `neuromod` is still a hard dependency for PR A.
> It will be made optional in a subsequent PR (see tracking issues #15-19).
> `corpus-ipc`/`zmq` are intentionally off-by-default during the decoupling phase
> (core builds and tests do not require libzmq).
>
> `neuromod` will be made optional later (see #15-19). This is tracked separately
> from the `corpus-ipc` temporary split.

### Docker (optional)

A `Dockerfile` is provided for reproducible Linux builds.

```bash
# Core build (no libzmq / stub backend only)
docker build --target core -t brainstem-daemon:core .

# Full build (with corpus-ipc + zmq)
docker build --target full -t brainstem-daemon:full .
```

Inside the container you can run the usual checks:
```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --no-default-features
cargo check --features corpus-ipc
cargo test --all-features
```

---

## Running (foreground)
```bash
target/release/brainstem-daemon            # uses default config
# or
brainstem-daemon --config /path/to/custom.toml
```

Stop it gracefully with `Ctrl-C` (SIGINT) on all platforms. On Unix, `kill` (SIGTERM, the default systemd stop signal) also breaks the tick loop, flushes the backend, and exits `0`.

---

## Systemd User Service (Fedora 43)

1. Copy unit file:

   ```ini
   # ~/.config/systemd/user/brainstem-daemon.service
   [Unit]
   Description=Soma Spiking Network Daemon
   After=network.target

   [Service]
   ExecStart=%h/.cargo/bin/brainstem-daemon --config %h/.config/soma/daemon.toml
   Restart=on-failure
   Environment=RUST_LOG=info

   [Install]
   WantedBy=default.target
   ```
2. Enable & start:
   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now brainstem-daemon
   ```

### SELinux

Needed only when the binary is built with `--features corpus-ipc` (ports `spine_sub_port` / `spine_pub_port`). The default stub backend opens no sockets.

```bash
sudo semanage port -a -t user_tcp_port_t -p tcp 5555
sudo semanage port -a -t user_tcp_port_t -p tcp 5556
sudo semanage fcontext -a -t user_home_t "$HOME/.config/soma(/.*)?"
restorecon -Rv ~/.config/soma
```

---

## Role and boundary matrix

`brainstem-daemon` is the **headless runtime process** for the Limen spiking-neural-network stack. It owns inference-time execution, stimulus ingestion, spike publication, and neuromodulator-driven network stepping. It does not own training, trading, mining, or hardware control.

| Concern | Owned by `brainstem-daemon` | Not owned |
|---|---|---|
| Purpose | Run `neuromod::SpikingNetwork` in a headless loop; ingest stimuli and publish spikes via a pluggable `BackendPair` (stub by default; `corpus-ipc` / ZeroMQ when that feature is enabled) | Training/weight optimization; hardware I/O; business logic (trading/mining) |
| Configuration | Load `DaemonConfig` from TOML; maintain a config-driven `ServiceRegistry` | Hardcoded service names; upstream `soma-engine` service names |
| Networking | Optional ZeroMQ PUB/SUB when built with `--features corpus-ipc`; `tokio` async runtime. Default stub opens no sockets | Direct exchange adapters; market-data feeds |
| Dependencies | `neuromod`, `tokio`, `serde`, `tracing`, `clap`; optional `corpus-ipc` + `zmq` behind the `corpus-ipc` feature (off by default) | Exchange/Mining-specific adapters; GPU drivers; weight-training frameworks |

### Relationship to other projects

- **`neuromod`** — core spiking-network library consumed by the daemon. The daemon configures dimensions and drives `SpikingNetwork::step` on every tick.
- **`limbic-critic`** — expected to send neuromodulator / critic signals over the `corpus-ipc` ingress channel when that feature is enabled. The daemon applies them but does not generate them. The default stub path does not open an ingress socket.
- **`silicon-bridge`** — consumes the daemon's outbound spike stream (ZeroMQ PUB) when the `corpus-ipc` feature is enabled. The daemon does not know what silicon-bridge does with the spikes. The default stub sink is a no-op.
- **`Spikenaut-Hardware`** — physical hardware coordination is out of scope; the daemon publishes logical spike events only.
- **`plasticity-lab`** — weight training and plasticity experiments live here, not in the daemon.

### Allowed dependencies

- `corpus-ipc` (optional Cargo feature `corpus-ipc`, off by default; pulls `zmq`)
- `neuromod`
- `tokio`, `serde`, `toml`, `tracing`, `clap`, `anyhow`, `dirs`

### Forbidden dependencies / domains

- Trading or mining exchange adapters
- Hardware-control / GPIO / firmware crates
- Weight-training / optimizer frameworks (e.g., gradient-descent, backprop tooling)

---

## Contributing

Local quality gate (fmt, clippy, stub vs optional `corpus-ipc` tests):
see [REVIEW.md](https://github.com/Limen-Neural/brainstem-daemon/blob/main/REVIEW.md).
GitHub Actions OS matrix and ZeroMQ skips:
[docs/ci.md](https://github.com/Limen-Neural/brainstem-daemon/blob/main/docs/ci.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE-2.0), at your option.

SPDX-License-Identifier: MIT OR Apache-2.0
