# Brainstem Daemon

[![CI](https://github.com/Limen-Neural/brainstem-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/Limen-Neural/brainstem-daemon/actions/workflows/ci.yml)
[![Codecov](https://codecov.io/gh/Limen-Neural/brainstem-daemon/graph/badge.svg)](https://codecov.io/gh/Limen-Neural/brainstem-daemon)
[![Codacy](https://app.codacy.com/gh/Limen-Neural/brainstem-daemon/badge.svg)](https://app.codacy.com/gh/Limen-Neural/brainstem-daemon/dashboard)
[![Maintainability](https://qlty.sh/gh/Limen-Neural/projects/brainstem-daemon/maintainability.svg)](https://qlty.sh/gh/Limen-Neural/projects/brainstem-daemon)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Headless spiking neural-network runtime written in Rust.

The daemon runs inference. It does not train models or optimize weights.

---

## Features

- Modular `neuromod::SpikingNetwork` core (CPU)
- Live-mode restore of Distill sidecar `snn_model.json` before the tick loop (explicit `simulation` mode for blank networks)
- Optional **ZeroMQ PUB/SUB** networking via `corpus-ipc`
- Headless **`brainstem-daemon`** binary for background execution
- Distinct **liveness / readiness / degraded / fatal** health snapshots (library handle plus optional `control_bind` listener)

---

## Responsibility and failure domains

The daemon owns the CPU `SpikingNetwork` tick loop, checkpoint validation,
and runtime health. A stimulus producer supplies inputs and retains any
hardware-safety responsibilities; a spike consumer decides what to do with the
outputs. With the optional ZeroMQ backend, the registry `corpus-ipc` dependency
defines the wire schema. Neither a separate producer nor a consumer is needed
for the default stub quickstart.

```text
stimulus producer --> brainstem-daemon --> spike consumer
                      checkpoint restore, inference ticks, health
```

The repository's [CPU-only integration fixture](#local-zeromq-integration-smoke)
exercises this boundary without checking out a separate producer.

---

## Install

**0.3.0 is a release candidate.** Until crates.io publication is verified,
build or install from this checkout. No earlier crates.io release under the
`brainstem-daemon` name is documented.

```bash
git clone --depth 1 https://github.com/Limen-Neural/brainstem-daemon.git
cd brainstem-daemon

# Run from source after saving the configuration below.
cargo run --locked -- --config daemon.toml

# Or install the binary from the checkout (choose one backend).
cargo install --locked --path .
# Optional: build the ZeroMQ backend instead (requires a C/C++ toolchain).
cargo install --locked --path . --features corpus-ipc
```

For a separate Rust project with this repository checked out beside it, add
one of these lines under `[dependencies]`:

```toml
brainstem-daemon = { path = "../brainstem-daemon" }
# Optional instead: brainstem-daemon = { path = "../brainstem-daemon", features = ["corpus-ipc"] }
```

The package's [.cargo/config.toml](.cargo/config.toml) is used when this package
is the Cargo root. An external library consumer must configure the native
ZeroMQ compiler flags described under [Backends](#backends). The default stub
needs no ZeroMQ.

For maintainers, the [release qualification procedure](https://github.com/Limen-Neural/brainstem-daemon/blob/main/docs/ci.md#cargo-archive-release-qualification)
shows how to check the candidate archive before any publication.

## Building

Use the pinned **Rust 1.99.0** toolchain. The minimum version is synchronized
across `rust-toolchain.toml` `channel`, `Cargo.toml` `rust-version`,
`.github/workflows/ci.yml` `toolchain:`, and `Dockerfile` `FROM rust:`
(see the [MSRV pin rule](https://github.com/Limen-Neural/brainstem-daemon/blob/main/REVIEW.md#msrv-pin-rule)).
crates.io `corpus-ipc` 0.2.1 declares Rust 1.85, which this compiler accepts.

```bash
# Release build, default stub backend (no libzmq)
cargo build --locked --release --bin brainstem-daemon

# Optional ZeroMQ / corpus-ipc backend
cargo build --locked --release --bin brainstem-daemon --features corpus-ipc
```

The binary will be located at `target/release/brainstem-daemon`. Feature flag vs backend vs which config keys apply is in [Backends](#backends).

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
`brainstem-daemon` expects a **TOML** file. The default is
`soma/daemon.toml` under the platform's config directory (usually
`~/.config/soma/daemon.toml` on Linux). Save the example as `daemon.toml` in
the repository root and pass `--config daemon.toml`. The default stub
runs without a producer; the simulation config below uses a blank network.

```toml
# daemon.toml

# Engine
lif_count      = 16        # must match the checkpoint LIF count in live mode
izh_count      = 0         # live sidecars are LIF-only
channels       = 16        # must match checkpoint input width in live mode
model_path     = "tests/fixtures/live/snn_model.json"  # used only for restore in live mode; no ~ expansion
runtime_mode   = "simulation"  # choose explicitly; omitted means live

# Runtime
tick_rate_hz   = 1000      # loop frequency, inclusive range 1..=1_000_000 Hz
log_level      = "info"    # error|warn|info|debug|trace

# Optional process control surface (unset = no extra socket; historical default).
# Serves /livez, /readyz, /health, /metrics. See docs/health.md.
# control_bind   = "127.0.0.1:9464"

# ZMQ (still required in TOML; no-ops under the default stub backend)
spine_sub_port = 5555      # stimuli in
spine_pub_port = 5556      # spikes out
# Built-in PUB lifecycle (all optional; corpus-ipc only, no-ops under stub).
# spine_pub_bind_host        = "127.0.0.1"  # loopback default; use "0.0.0.0" to expose broadly
# spine_pub_sndhwm           = 1000         # explicit finite send high-water mark
# spine_pub_linger_ms        = 0            # finite LINGER: drop pending on close (bounded teardown)
# spine_pub_send_empty_batches = true       # send a frame every tick even with zero spikes

# Bounded ingress (optional; defaults shown). Each class has its own
# capacity and overflow policy so bulk telemetry cannot starve control.
[ingress]
sensory_capacity    = 64
sensory_policy      = "drop_oldest"
reward_capacity     = 8
reward_policy       = "coalesce"
control_capacity    = 16
control_policy      = "block_timeout"
telemetry_capacity  = 128
telemetry_policy    = "drop_oldest"
block_timeout_ms    = 5
max_payload_len     = 4096
```

### Configuration validity and migration

`DaemonConfig::load`, fallible daemon constructors, and `restore_network` use
the same `DaemonConfig::validate()` gate, before network allocation or checkpoint
reads. The binary validates before creating its runtime, initializing a backend,
or binding PUB/control sockets. Both timed and bounded (`run_for_ticks`) runs
require a validated constructor. Raw `toml::from_str` and Rust struct literals
do not validate automatically; library callers should call `validate()` **before
constructing a custom backend pair**, since the daemon cannot undo sockets or
other side effects the caller already created. Infallible `new`/`with_backend`
constructors panic on invalid configuration; prefer `try_new`/`try_with_backend`.

The supported dimensions are:

- `tick_rate_hz`: 1 through 1,000,000 inclusive, including for bounded runs.
- `lif_count + izh_count`: 1 through 65,535 inclusive (the existing spike-ID
  bound). Either count may be zero in simulation, but not both. Live mode remains
  LIF-only (`izh_count = 0`) and dimensions must match the checkpoint.
- `channels`: 1 through `ingress.max_payload_len` inclusive (4,096 by default;
  the ingress hard maximum is 1,048,576). A full-width stimulus must fit ingress.
- `(lif_count + izh_count) * channels`: at most `MAX_NETWORK_INPUTS` = 1,048,576
  weight/per-input-state slots. Checked addition and multiplication reject
  overflow without attempting a huge allocation. The product limit bounds the
  dense model, not just a single packet; it is not a byte-size limit.
- `control_bind`, when set: numeric IPv4 `IP:port` or IPv6 `[address]:port`.
  PUB bind hosts accept IP literals (IPv6 with or without brackets), DNS
  hostnames, or `*`; no URL scheme or port. Numeric-only names that are not
  valid IPs (such as `5555`) are rejected. Syntax is checked without DNS or
  socket I/O; address availability and hostname resolution can still fail at
  bind time. PUB option syntax and bounded-lifecycle settings are validated
  even under stub, but remain no-ops there and do not require libzmq.

Migration: configurations that previously reached late runtime failures (or
bypassed them in a bounded/simulation run) now fail at load/construction. Replace
zero dimensions with a real minimal network, reduce oversized dimensions, or
raise `ingress.max_payload_len` within its hard cap when a supported full-width
input exceeds the default. A minimal simulation uses `runtime_mode = "simulation"`,
`lif_count = 1`, `izh_count = 0`, and `channels = 1` with the other required keys
from the example above; `model_path` is still required but unused for restore.
The existing 16×16 live sidecar path remains supported.

`log_level` accepts only the five levels shown above; target directives belong
in `RUST_LOG`. When set, `RUST_LOG` takes precedence over the TOML level and may
use normal `tracing_subscriber` filter syntax (for example,
`brainstem_daemon=debug,info`). If `RUST_LOG` is invalid, startup prints one
diagnostic to stderr and safely falls back to the validated TOML level.

### Backends

Default Cargo features are empty (`default = []` in `Cargo.toml`). That path uses the in-memory **stub** backend (`StubStimulusSource` + `NoopSpikeSink`) and does **not** need ZeroMQ. The optional `corpus-ipc` feature (same as `--all-features` today) pulls `corpus-ipc` **from crates.io** (`0.2.1`, `features = ["zmq"]`) plus this crate's optional `zmq` dependency. Published `corpus-ipc` compiles libzmq via `zmq-sys` / `zeromq-src` (a C++ compiler is required; a system `libzmq` package is not). It does not vendor ZeroMQ as a git submodule.

`zeromq-src` 0.2.6 allocates libzmq queue chunks with `malloc` unless `HAVE_POSIX_MEMALIGN` is set. On x86-64-v3 GCC (Ubuntu 26.04) that misaligned `command_t` slot SIGSEGVs. When this package is the Cargo root, `.cargo/config.toml` passes `-DHAVE_POSIX_MEMALIGN -DZMQ_CACHELINE_SIZE=64` as `CFLAGS_<triple>` and `CXXFLAGS_<triple>` for x86_64 and aarch64 Linux gnu, Linux musl, and macOS. Any other target, including other Unix targets, does not get those defines; set `CFLAGS` and `CXXFLAGS` yourself there. The `cc` crate appends the listed keys to any `CFLAGS` / `CXXFLAGS` already set. A global `CFLAGS` entry is not used, because it would also apply on Windows, whose CRT has no `posix_memalign`. An external crate that depends on `brainstem-daemon` does not load this file, and this crate cannot pass the defines into `zeromq-src` (that native library is compiled first). Set the same defines in `CFLAGS` and `CXXFLAGS`, or in the matching `CFLAGS_<triple>` / `CXXFLAGS_<triple>` variables (hyphens written as underscores), before building `--features corpus-ipc`.

`DaemonConfig` deserialization is **not** feature-gated: `spine_sub_port`, `spine_pub_port`, and `model_path` are still required in TOML even on the stub path (`services`, `runtime_mode`, `ingress`, and `control_bind` are optional; `runtime_mode` defaults to `live`; `ingress` defaults to the bounded-queue table below; `control_bind` unset = no extra socket). Effect at runtime depends on which backend is **wired** and on `runtime_mode`.

#### Feature truth table

| Cargo flags | Wired backend | `libzmq` | Binary (`brainstem-daemon`) | Library `BrainstemDaemon::new()` / `try_new()` |
|---|---|---|---|---|
| default / `--no-default-features` | stub | not required | no backend sockets by default; `control_bind` opens the control listener; logs `🔌 Using stub backend` | stub |
| `--features corpus-ipc` | ZMQ / `corpus-ipc` | required | SUB via env, PUB on `spine_pub_bind_host:spine_pub_port` (loopback by default); logs `📡 Using ZMQ corpus-ipc backend` | **still stub** |
| `--all-features` | same as `corpus-ipc` | required | same as `--features corpus-ipc` | **still stub** |

Enabling the feature does **not** change `BrainstemDaemon::new()` or `try_new()`. Those always inject `BackendPair::stub()`. Only `src/bin/brainstem_daemon.rs` constructs `ZmqStimulusSource` + `ZmqSpikeSink` when `corpus-ipc` is on.

Library users who want live ZMQ must build that pair themselves under `#[cfg(feature = "corpus-ipc")]` and pass it to `with_backend` / `try_with_backend`. `BrainstemDaemon::run` / `run_with_restored_network` / `run_for_ticks` call `StimulusSource::initialize` (the binary no longer initializes first, so the pinned ZMQ backend is not reconnected). A failing `initialize` marks health **fatal** and never becomes ready.

Health snapshots, probe paths, and the transition table live in [`docs/health.md`](https://github.com/Limen-Neural/brainstem-daemon/blob/main/docs/health.md).

#### Config keys and env vars

| Setting | Stub (default binary / `::new()`) | `corpus-ipc` binary |
|---|---|---|
| `runtime_mode` | used (`live` restores a Spikenaut sidecar before ticks; `simulation` builds a blank network) | used (same gate; independent of ZMQ) |
| `lif_count`, `izh_count`, `channels` | used (checked against the checkpoint in live mode) | used |
| `tick_rate_hz` | used | used |
| `log_level` | validated by constructors (`::new` / `try_with_backend`); binary tracing default, overridden by a valid `RUST_LOG`; library `run` does not initialize tracing | same |
| `services` | used (`ServiceRegistry`) | used |
| `control_bind` | optional HTTP control surface; unset = no listener | same |
| `ingress` | used (bounded class queues in the tick loop; health reports aggregate fill) | used (same queues wrap backend packets before the network step) |
| `spine_sub_port` | parsed, **no-op** | sets `CORPUS_IPC_ZMQ_READOUT_IPC` to `tcp://127.0.0.1:<port>` (also sets legacy `SPIKENAUT_ZMQ_READOUT_IPC` for compatibility) |
| `spine_pub_port` | parsed, **no-op** | binds ZMQ PUB `tcp://<spine_pub_bind_host>:<port>` (loopback by default) |
| `spine_pub_bind_host` | parsed, **no-op** | PUB bind host; default `127.0.0.1` (loopback). Set `0.0.0.0` (or a specific interface) to opt in to broader exposure |
| `spine_pub_sndhwm` | parsed, **no-op** | explicit finite send high-water mark applied to the PUB socket before bind (default `1000`) |
| `spine_pub_linger_ms` | parsed, **no-op** | finite LINGER (ms) applied before bind so teardown/shutdown cannot block (default `0` = drop pending on close) |
| `spine_pub_send_empty_batches` | parsed, **no-op** | empty-batch policy; `true` (default) sends a frame every tick even with zero spikes, `false` suppresses empty batches at the sink |
| `model_path` | used in **live** mode (sidecar JSON); ignored in **simulation** (`StubStimulusSource::initialize` still ignores it) | same live/simulation gate, then passed literally to `initialize` (no `~` expansion); the ZMQ SUB source connects and ignores `_model_path` |

**Settings that only take effect with `corpus-ipc`** (the `brainstem-daemon` binary built `--features corpus-ipc`):

- `spine_sub_port` (drives `CORPUS_IPC_ZMQ_READOUT_IPC`)
- `spine_pub_port`
- `CORPUS_IPC_ZMQ_READOUT_IPC` (const `CORPUS_IPC_READOUT_ENV`; this is what `ZmqStimulusSource::initialize` reads when no explicit `connect` endpoint is set)

**Passed through / set, but unused by the ZMQ SUB source after connect:**

- `model_path` (literal filesystem path; `~` is not expanded; passed to `initialize`, which names the argument `_model_path` and does not consume it; live-mode restore consumes it before that handshake)
- `SPIKENAUT_ZMQ_READOUT_IPC` (const `LEGACY_SPIKENAUT_READOUT_ENV`; the binary still sets this alongside `CORPUS_IPC_ZMQ_READOUT_IPC` for older tooling)

Under stub those ZMQ TOML keys are still parsed. The env vars are unset by the default binary. Nothing in this crate reads them without the `corpus-ipc` feature.

##### PUB lifecycle, bind default, and loss semantics

The built-in `corpus-ipc` PUB egress is explicitly bounded. Before bind it sets a finite send high-water mark (`spine_pub_sndhwm`) and a finite LINGER (`spine_pub_linger_ms`, default `0` so close/teardown drops pending messages instead of blocking), then binds to `tcp://<spine_pub_bind_host>:<spine_pub_port>` — loopback (`127.0.0.1`) by default, so broader exposure such as `0.0.0.0` requires explicit configuration.

Configuration loading rejects `spine_pub_sndhwm <= 0` (zero disables the queue bound) and `spine_pub_linger_ms < 0` (infinite linger). A positive linger allows pending sends a finite grace period; expiry discards anything still pending. These are queue/lifecycle bounds, not delivery guarantees or a byte-level memory cap.

Empty-batch policy: `spine_pub_send_empty_batches` (default `true`) publishes an empty batch whenever the tick loop calls the sink with zero spikes, preserving per-tick / heartbeat frames on normal ticks. Rejected input or a failed tick may skip emission entirely; an empty frame is not a guaranteed clock or delivery acknowledgement. Set the key to `false` to suppress empty batches at the sink; nonempty batches still send.

The wire payload remains unversioned, externally tagged `corpus-ipc` JSON, including null `session_id` and `metadata`; no `wire_version` / `payload` envelope is added. For an empty batch at 2 ms:

```json
{"Spikes":{"session_id":null,"batch_id":2,"timestamp":2000000,"spikes":[],"metadata":null}}
```

Nonempty `spikes` entries retain `channel`, `time`, and `strength`.

`ZmqSpikeSink::stats()` exposes three **application-side** counters: `attempted` (`send()` was called, including failed calls), `suppressed` (an empty batch withheld before sending), and `failed` (`send()` returned an error, a subset of attempts). Attempts minus failures counts local API acceptance, **not subscriber receipt**. These sink counters are not exported by the control surface; runtime `emit_errors` counts sink `Err` returns, not lost PUB frames.

ZeroMQ PUB is best-effort: absent, disconnected, slow, or over-HWM subscribers can miss frames silently; pending frames can also be discarded on close/linger expiry. The publisher cannot measure those losses exactly and never reports them as measured subscriber drops. Consumers needing loss accounting must track received frames and expected batch progression themselves (allowing for deliberately suppressed/omitted ticks); reliable delivery requires a protocol with acknowledgements outside this PUB path.

ZMQ SUB ingress decodes unversioned JSON `IpcMessage` frames (`Stimuli` / `Neuromodulators`) through crates.io `corpus-ipc` 0.2.1 types. Width, schema token `corpus-ipc.stimulus.v1`, freshness, and future timestamps are rejected without stopping the tick loop. Modulation-only frames are drained in the same tick so they do not consume a sensory period.

### Runtime modes: simulation vs loaded Spikenaut

`runtime_mode` is independent of the stub vs ZMQ **backend**. Backends move stimuli and spikes. The network itself is restored **before** the tick loop:

```text
resolve model/checkpoint
        ↓
parse + source/schema gate (`source = "spikenaut_julia"`, optional `q88`/`encoder`)
        ↓
validate dimensions/input contract
        ↓
validate finite parameters (including f32 overflow)
        ↓
validate nonblank expected weights
        ↓
record provenance/hash/model identity
        ↓
construct/restore runtime `neuromod` 0.7.0 network (freeze R-STDP `reward_lr`)
        ↓
ONLY THEN start live ticks
```

| `runtime_mode` | Network at tick start | `model_path` |
|---|---|---|
| `live` (default) | Distill sidecar `snn_model.json` restored into `neuromod::SpikingNetwork`. Startup **fails closed** on a missing, corrupt, dimension-mismatched, non-finite, or blank artifact, and on any sidecar that carries `output_weights` (including explicit `null`). FPGA Q8.8 `.mem` dumps are rejected. | Required: a sidecar JSON file, a Hugging Face `config.json`, or a directory containing `snn_model.json` / `dataset/merged_v2/snn_model.json` |
| `simulation` | Blank `SpikingNetwork::with_dimensions(lif_count, izh_count, channels)` (zero input weights). Logs that this is **not** a loaded Spikenaut checkpoint. | Parsed but unused for restoration |

Live mode never falls back to `with_dimensions()` after a failed load. A successful live start logs `schema_id`, `model_id`, source path, SHA-256, encoder, source, and lineage.

The supported format is a nonblank LIF-only Distill JSON sidecar
(`source = "spikenaut_julia"`, `izh_count = 0`) with no `output_weights`
key. `neuromod` 0.7.0 has no Distill readout matrix, so a sidecar that
contains that key is rejected rather than losing its readout. The previously
documented Hugging Face `dataset/merged_v2/snn_model.json` export contained
that key; this quickstart needs no external download.

To start the bundled live fixture, change `runtime_mode` to `"live"` in
[`daemon.toml` above](#configuration), leave the 16 × 16 dimensions and
fixture path unchanged, and run from the repository root:

```bash
cargo run --locked -- --config daemon.toml
```

A relative `model_path` is resolved from the process working directory.
Stop with Ctrl-C. The [synthetic fixture](tests/fixtures/live/snn_model.json)
is versioned and untrained (SHA-256 and origin: [fixture manifest](tests/fixtures/live/MANIFEST.md)).
Run `cargo test --locked --test live_fixture` to exercise live start, input,
tick, spike output, and shutdown through the library. The packaged CLI gate
separately checks live start, `/readyz`, and graceful exit. The default CLI's
stub backend supplies no external input and discards spike output.

Live restore copies LIF weights, membrane, `last_spike`, `decay_rate`, and `threshold` (also seeding `base_threshold`) and sets `RmStdpConfig.reward_lr = 0` so dopamine-gated R-STDP cannot retrain the Distill matrix. `neuromod` 0.7.0 `SpikingNetwork::step` still assigns `decay_rate` from acetylcholine, blends `threshold` toward `0.05..=0.50`, and L1-renormalizes rows whose weights already sum above `1e-6`. Those are engine contracts; this crate does not fork `step`.

### Usefulness experiment

[`experiments/usefulness/`](experiments/usefulness/) holds a pre-registered,
repository-local experiment that compares the daemon with a direct-engine
baseline on a synthetic, untrained fixture (`PROTOCOL.md`, `run.sh`,
`summarize.py`, `REPORT.md`). It measures runtime correctness, latency,
throughput, and memory; it is not release qualification.

[`experiments/usefulness-secondary/`](experiments/usefulness-secondary/) is
an independent, secondary replication with its own fixture, workload,
drivers (`usefulness_secondary_*` examples), and
[report](experiments/usefulness-secondary/REPORT.md). It runs in-process and
through the real binary over ZeroMQ, and reaches the same decision. Run it
from a clean checkout with `experiments/usefulness-secondary/run.sh` (Linux,
needs `libzmq`).

### Local ZeroMQ integration smoke

CPU-only integration coverage (no GPU) lives in `tests/thalamic_brainstem_smoke.rs` and is gated on `--features corpus-ipc` so default stub tests never need `libzmq`.

The Thalamic fixture (`tests/fixtures/thalamic_producer.rs`) produces `IpcMessage::Stimuli(StimulusBatch)` from simulated telemetry. It does not import `neuromod` or own a `SpikingNetwork`. Brainstem restores a Distill sidecar JSON checkpoint before ticking, preserves `valid_mask` and `session_id` across the wire, and rejects incompatible schema/JSON loudly. A separate assertion keeps the fixture's safety flag healthy when Brainstem/transport is absent, and a later publish cannot clobber a thermal fault. Dropping the producer and constructing a new one (in-process restart) resets only that process-local safety flag. A child OS process (`thalamic_os_process_restart_does_not_leak_safety`) starts healthy, publishes a wire frame with no thermal/safety keys, and Brainstem `HealthSnapshot` still has no thermal/power fields.

```bash
CC=gcc CXX=g++ cargo test --locked --features corpus-ipc --test thalamic_brainstem_smoke
```

Simulation is the deliberate test/dev path for a blank network. Do not use it as a stand-in for production Spikenaut.

```toml
# Explicit blank network for local tests (not production Spikenaut)
runtime_mode = "simulation"
lif_count = 16
izh_count = 5
channels = 16
model_path = "/unused/in/simulation.json"
```

### Bounded ingress

Every in-process channel that can feed the tick loop goes through `BoundedIngress` (one queue per message class). OS `SIGINT`/`SIGTERM` stay out-of-band in `tokio::select!` so shutdown cannot sit behind bulk traffic.

| Class | Feeds the tick from | Default capacity | Overflow | Why |
|---|---|---|---|---|
| `control` | in-band control/safety envelopes | 16 | `block_timeout` (then reject) | Producer sees backpressure; not silently dropped |
| `reward` | `IngressPacket.modulators` | 8 (depth 0..=1) | `coalesce` | Neuromodulators are a snapshot; keep latest |
| `sensory` | `StimulusSource` stimuli | 64 | `drop_oldest` | Latest frames matter; losses are counted |
| `telemetry` | reserved bulk class | 128 | `drop_oldest` | Isolated so it cannot fill the control queue |

`coalesce` always stores at most one occupant. `block_timeout` waits up to `block_timeout_ms` (default 5) against a **single deadline** (spurious wakes do not restart the timer). Queue capacity is capped at `MAX_QUEUE_CAPACITY` (16384). `block_timeout_ms` is capped at `MAX_BLOCK_TIMEOUT_MS` (86400000). `max_payload_len` is capped at `MAX_PAYLOAD_LEN` (1048576); each packet's `stimuli` and `modulators` vectors are capped by the configured `max_payload_len` (default 4096). The tick loop uses `try_enqueue` when admitting a backend packet so the 1 kHz cadence never waits on itself. Empty backend placeholders (`Ok(None)` or empty `stimuli`) are **not** enqueued on the sensory queue, so they cannot evict in-process sensory. Non-empty `modulators` on the same packet are still admitted to the reward queue. In-band control is drained first and observed (logged) each tick; the network step has no control actuator this wave, and OS `SIGINT`/`SIGTERM` remain the live shutdown path.

Each lost or coalesced event increments exactly one of `rejected`, `dropped`, or `coalesced`. Snapshots also record `accepted`, `depth`, `high_water_mark`, `producer_waits`, and `producer_wait_ns` with a `class` label only. `BoundedIngress::shutdown()` unblocks waiters and refuses further enqueue.

**Non-finite values (`NaN`, `±Inf`)** in stimuli or modulators are rejected before `network.step` on every path (typed `corpus-ipc`, custom `StimulusSource`, and direct `enqueue` / `try_enqueue`). The daemon finite gates bump `rejected_batches` and emit rate-limited diagnostics with a stable key. Typed `corpus-ipc` parse and modulator rejects also warn from the source (`next_ingress` / `hold_or_reject`); those source warnings are not passed through the tick-loop limiter. A non-finite payload does not refresh `input_freshness`. A rejected frame that still carries previously held finite modulators can refresh freshness, because observation follows the finite payload rather than the `rejected` flag. Modulators are validated before any cached/hold state is updated; masked-out stimulus slots and short modulator vectors are still scanned fail-closed. A finite packet on the next tick is accepted normally. **Two tick outcomes:** a non-finite **backend** packet is dropped before enqueue. The tick then drains anything already queued and uses those stimuli, zero-padding a short vector; with no drained sensory input the stimulus buffer stays all zeros. A non-finite packet that surfaces only on the **drained** path (typical for direct enqueue) skips `network.step` and spike emission for that tick while the process stays live.

### Library quickstart

Load the same TOML configuration through the fallible API. From the process
working directory containing `daemon.toml`, a one-tick smoke run is:

```rust
use std::path::Path;
use brainstem_daemon::{BrainstemDaemon, DaemonConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = DaemonConfig::load(Path::new("daemon.toml"))?;
    let stats = BrainstemDaemon::try_new(config)?.run_for_ticks(1)?;
    assert_eq!(stats.ticks, 1);
    Ok(())
}
```

`try_new` always selects the stub. Use `try_with_backend` with a custom
`BackendPair` when you need real inputs or to collect output.

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
   StartLimitIntervalSec=60
   StartLimitBurst=3

   [Service]
   ExecStart=%h/.cargo/bin/brainstem-daemon --config %h/.config/soma/daemon.toml
   Restart=on-failure
   RestartSec=5s
   # Optional: overrides daemon.toml log_level; supports EnvFilter directives.
   Environment=RUST_LOG=info

   [Install]
   WantedBy=default.target
   ```
2. Enable & start:
   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now brainstem-daemon
   ```

Invalid configuration exits with status `1` and a field-specific diagnostic,
before health endpoints exist. This is **not** a special permanent-error exit
classification: `Restart=on-failure` also retries configuration failures.
For immediate startup failures, the unit above spaces retries by five seconds
and allows at most three starts within sixty seconds, then leaves the service failed.
The interval expiring does not itself schedule another retry. Inspect
`journalctl --user -u brainstem-daemon`, fix the configuration/checkpoint or bind
conflict, then run `systemctl --user reset-failed brainstem-daemon` and
`systemctl --user restart brainstem-daemon`. Restarting with the same invalid
configuration cannot repair a permanent startup error.

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

`brainstem-daemon` is the **headless runtime process** for the Limen spiking-neural-network stack. It owns inference-time execution, stimulus ingestion, spike publication, and neuromodulator-driven network stepping. It does not own training, trading, mining, or hardware control. The [responsibility and failure-domain diagram](#responsibility-and-failure-domains) is the short form of this table.

| Concern | Owned by `brainstem-daemon` | Not owned |
|---|---|---|
| Purpose | Run `neuromod::SpikingNetwork` in a headless loop; ingest stimuli and publish spikes via a pluggable `BackendPair` (stub by default; `corpus-ipc` / ZeroMQ when that feature is enabled) | Training/weight optimization; hardware I/O; business logic (trading/mining) |
| Configuration | Load `DaemonConfig` from TOML; maintain a config-driven `ServiceRegistry`; restore a validated Spikenaut sidecar (or an explicit simulation blank network) before ticks | Hardcoded service names; upstream `soma-engine` service names; silent fallback to a blank `with_dimensions()` network in live mode |
| Networking | Optional ZeroMQ PUB/SUB when built with `--features corpus-ipc`; `tokio` async runtime. Default stub opens no sockets unless `control_bind` is configured | Direct exchange adapters; market-data feeds |
| Dependencies | `neuromod`, `tokio`, `serde`, `tracing`, `clap`; optional `corpus-ipc` + `zmq` behind the `corpus-ipc` feature (off by default) | Exchange/Mining-specific adapters; GPU drivers; weight-training frameworks |

### Integration contracts

- **`neuromod`** — crates.io **0.7.0** (`neuromod = "0.7.0"`; Cargo's pre-1.0 range stays on 0.7.z). Live mode restores Distill sidecar LIF weights/state into this crate's `SpikingNetwork` (no in-tree fork of `stdp_config` / per-LIF `eligibility`) and then drives `SpikingNetwork::step` on every tick (`step` remains the thread-local RNG wrapper; `step_with_rng` is unused here). Simulation mode constructs a blank network from configured dimensions. The optional 4-float ingress tail is dopamine, serotonin, acetylcholine, norepinephrine (`cortisol` / `tempo` / `aux_dopamine` are gone).
- **Checkpoints:** the daemon restores supported Distill sidecars. Model
  training and FPGA export are outside this crate.
- **I/O:** with the optional ZeroMQ backend, external producers send
  stimuli or modulator frames and consumers receive the spike stream.
  See [Config keys and env vars](#config-keys-and-env-vars) and
  [PUB lifecycle, bind default, and loss semantics](#pub-lifecycle-bind-default-and-loss-semantics).
  Consumers own any downstream hardware interaction.

### Allowed dependencies

- `corpus-ipc` (optional Cargo feature `corpus-ipc`, off by default; pulls `zmq`)
- `neuromod`
- `tokio`, `serde`, `serde_json`, `toml`, `tracing`, `clap`, `anyhow`, `dirs`, `sha2`

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
