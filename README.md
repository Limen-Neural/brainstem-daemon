# Brainstem Daemon

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

High-performance spiking neural-network runtime written in Rust.

> **Note**  
> Training / weight-optimization lives in the separate `plasticity-lab` project; `brainstem-daemon` is *inference-only*.

---

## Features
- Modular `neuromod::SpikingNetwork` core (CPU; SIMD ready)
- High-frequency networking over **ZeroMQ PUB/SUB** via `corpus-ipc`
- Headless **`soma-daemon`** binary for background execution

---

## Building
```bash
# Release build (includes soma-daemon)
cargo build --release --bin soma-daemon
```
The binary will be located at `target/release/soma-daemon`.

---

## Configuration
`soma-daemon` expects a **TOML** file; default path: `~/.config/soma/daemon.toml` (override with `--config`).

```toml
# ~/.config/soma/daemon.toml

# Engine
lif_count      = 16        # LIF neurons
izh_count      = 5         # Izhikevich neurons
channels       = 16        # expected input channels
model_path     = "~/models/soma16.mem" # weights/thresholds

# Runtime
tick_rate_hz   = 1000      # loop frequency
log_level      = "info"    # error|warn|info|debug|trace

# ZMQ
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

`corpus-ipc` / ZeroMQ is currently an **optional** feature (`corpus-ipc`). When the feature is disabled (the default during this temporary decoupling phase), an in-memory stub backend is used instead.

Only the following settings are specific to the ZMQ backend:
- `spine_sub_port`
- `spine_pub_port`
- `SPIKENAUT_ZMQ_READOUT_IPC` (or `CORPUS_IPC_ZMQ_READOUT_IPC`)

When using the stub backend these have no effect.

The stub backend is always safe to use for core library builds/tests and simulation runs.
Example of constructing a daemon with the stub backend (feature-independent):

```rust
use brainstem_daemon::{BrainstemDaemon, DaemonConfig, BackendPair};

let cfg: DaemonConfig = /* ... */;
let daemon = BrainstemDaemon::with_backend(cfg, BackendPair::stub());
```

> **Note (temporary):** `neuromod` is still a hard dependency for PR A.
> It will be made optional in a subsequent PR (see tracking issues #15-19).

---

## Running (foreground)
```bash
target/release/soma-daemon            # uses default config
# or
soma-daemon --config /path/to/custom.toml
```

---

## Systemd User Service (Fedora 43)
1. Copy unit file:
   ```ini
   # ~/.config/systemd/user/soma-daemon.service
   [Unit]
   Description=Soma Spiking Network Daemon
   After=network.target

   [Service]
   ExecStart=%h/.cargo/bin/soma-daemon --config %h/.config/soma/daemon.toml
   Restart=on-failure
   Environment=RUST_LOG=info

   [Install]
   WantedBy=default.target
   ```
2. Enable & start:
   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now soma-daemon
   ```

### SELinux
```bash
sudo semanage port -a -t user_tcp_port_t -p tcp 5555
sudo semanage port -a -t user_tcp_port_t -p tcp 5556
sudo semanage fcontext -a -t user_home_t "~/.config/soma(/.*)?"
restorecon -Rv ~/.config/soma
```

---

## Role and boundary matrix

`brainstem-daemon` is the **headless runtime process** for the Limen spiking-neural-network stack. It owns inference-time execution, stimulus ingestion, spike publication, and neuromodulator-driven network stepping. It does not own training, trading, mining, or hardware control.

| Concern | Owned by `brainstem-daemon` | Not owned |
|---|---|---|
| Purpose | Run `neuromod::SpikingNetwork` in a headless loop; ingest stimuli via `corpus-ipc`; publish spikes via ZeroMQ | Training/weight optimization; hardware I/O; business logic (trading/mining) |
| Configuration | Load `DaemonConfig` from TOML; maintain a config-driven `ServiceRegistry` | Hardcoded service names; upstream `soma-engine` service names |
| Networking | ZeroMQ PUB/SUB; `tokio` async runtime | Direct exchange adapters; market-data feeds |
| Dependencies | `corpus-ipc`, `neuromod`, `tokio`, `zmq`, `serde`, `tracing`, `clap` | Exchange/Mining-specific adapters; GPU drivers; weight-training frameworks |

### Relationship to other projects

- **`neuromod`** — core spiking-network library consumed by the daemon. The daemon configures dimensions and drives `SpikingNetwork::step` on every tick.
- **`limbic-critic`** — expected to send neuromodulator / critic signals over the `corpus-ipc` ingress channel. The daemon applies them but does not generate them.
- **`silicon-bridge`** — consumes the daemon's outbound spike stream (ZeroMQ PUB) for downstream tasks. The daemon does not know what silicon-bridge does with the spikes.
- **`Spikenaut-Hardware`** — physical hardware coordination is out of scope; the daemon publishes logical spike events only.
- **`plasticity-lab`** — weight training and plasticity experiments live here, not in the daemon.

### Allowed dependencies

- `corpus-ipc` (with `zmq` feature)
- `neuromod`
- `tokio`, `zmq`, `serde`, `toml`, `tracing`, `clap`, `anyhow`, `dirs`

### Forbidden dependencies / domains

- Trading or mining exchange adapters
- Hardware-control / GPIO / firmware crates
- Weight-training / optimizer frameworks (e.g., gradient-descent, backprop tooling)

---

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE-2.0), at your option.

SPDX-License-Identifier: MIT OR Apache-2.0
