# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Pre-registered, reproducible usefulness experiment under
  `experiments/usefulness/` comparing the daemon with a direct-engine baseline
  on a synthetic untrained fixture, with raw results and a report (#70).
- Secondary usefulness experiment under `experiments/usefulness-secondary/`
  (#70, #96). It is an independent replication with its own pre-registered
  protocol, versioned synthetic fixture, typed `corpus-ipc` workload,
  `corpus-ipc`-gated `usefulness_secondary_*` example drivers (in-process and
  over ZeroMQ), a locked runner, a stdlib summarizer with a run-inventory
  gate, and raw results and a report on Rust 1.99.0 / `neuromod` 0.7.0.
- CPU-only Linux qualification of the real PUB binary lifecycle under SIGTERM
  and SIGINT, including absent/stalled subscribers, clean exit and port release
  (#68). Strengthened socket tests cover finite-linger teardown, failed-send
  accounting and exact empty/nonempty `corpus-ipc` wire compatibility.

### Changed

- Align MSRV and toolchain pins to **Rust 1.99.0** (`Cargo.toml`,
  `rust-toolchain.toml`, CI, package qualification, `Dockerfile`,
  `AGENTS.md`, `README.md`, `.devin/blueprint.yaml`) so the daemon matches
  the Limen-Neural stack pin on axon-encoder, synaptic-wiring, and
  corpus-ipc main. Building this crate, including from crates.io, requires
  Rust 1.99.0; a 1.98.x toolchain is rejected. crates.io `corpus-ipc` 0.1.0
  still declares Rust 1.98.1 (#98). The usefulness runner and its reproduction steps use the same
  pin. Committed result files stay on the Rust 1.98.1 run that produced
  them. Static libzmq from `zeromq-src` 0.2.6 is built with
  `HAVE_POSIX_MEMALIGN` and a 64-byte cache line so `command_t` copies stay
  aligned under x86-64-v3 GCC (Ubuntu 26.04); without that, `vmovdqa`
  SIGSEGVs in the queue. `.cargo/config.toml` passes those defines, appended
  to any existing `CFLAGS` / `CXXFLAGS`, only when this package is the Cargo
  root (a checkout, an extracted crate, or `cargo install`) and only for the
  Unix triples named there. Windows is not included. An external crate that
  depends on this package must set the same flags itself; this crate's build
  script runs after `zeromq-src` and cannot inject them.
- Bump `neuromod` from crates.io **0.6.0** to **0.7.0** (pre-1.0 range
  `>=0.7.0, <0.8.0`). `SpikingNetwork::step` keeps the same
  `(stimuli, modulators)` signature; 0.7 adds `StepError` variants the daemon
  does not match exhaustively. Modulator decay (`0.15 - 0.05 * ACh`) and the
  0.6 self-describing checkpoint shape still hold (LIM-1509).
- Built-in PUB defaults to loopback (`127.0.0.1`); remote subscribers require
  explicit `spine_pub_bind_host` configuration. Explicit positive SNDHWM and
  nonnegative finite LINGER bound buffering/teardown; empty batches remain
  enabled by default. Application attempted/suppressed/failed counters do not
  claim subscriber delivery or measure silent PUB loss (#74, qualified by #68).

## [0.3.0] - 2026-09-18

Pending crates.io publication for GitHub milestone **v0.3.0 — Spikenaut SNN runtime**.
In-tree package version is **0.3.0**; `cargo publish` has not run. Last published
crate remains **0.1.2** (April 2026, `neuromod` 0.4.0). There is no 0.2.0
crates.io line; that GitHub milestone was stub-quality work.

### Added

- README responsibility / failure-domain diagram: `thalamic-relay` (sensory +
  hardware safety), `corpus-ipc` (published wire/schema), and
  `brainstem-daemon` (canonical `SpikingNetwork` tick loop) remain separate
  processes. Documents independent Thalamic stop/restart without moving
  thermal/power duty into Brainstem (#39).
- Smoke coverage that a dropped/reconstructed Thalamic producer keeps
  hardware-safety state process-local while Brainstem still ticks a loaded
  checkpoint (`thalamic_restart_keeps_hardware_safety_out_of_brainstem`).
- OS-process Thalamic restart smoke: a child process starts with a clean
  safety flag; the parent's thermal fault is absent from the published
  `IpcMessage` JSON and from Brainstem `HealthSnapshot`
  (`thalamic_os_process_restart_does_not_leak_safety`).
- Distinct liveness, readiness, recoverable degradation, and sticky fatal health
  (`src/health/`) with a fake-clock state-machine test for every transition and
  recovery path. Optional `control_bind` listener serves `/livez`, `/readyz`,
  `/health`, and `/metrics` (the repository's first control surface; none existed
  before). Contract, transition table, and example snapshots: [`docs/health.md`](docs/health.md).
- Deterministic fake-clock / fake-core fault-injection harness (`src/runtime`)
  that injects failures at initialization, checkpoint validation, ingress,
  tick execution, metric publication, and shutdown. Restart preserves
  checkpoint identity and durable session/tick/ingress identifiers; replayed
  inputs are not counted as fresh work (LIM-1218).
- Live-mode startup gate that loads and validates a Distill sidecar
  `snn_model.json` (Hugging Face `rmems/Spikenaut-SNN`) before the tick loop.
  Provenance (path, SHA-256, schema id, encoder, lineage) is logged. Startup
  fails closed on missing, corrupt, dimension-mismatched, non-finite, or
  blank checkpoints, and on sidecars that carry `output_weights`. FPGA Q8.8
  `.mem` dumps are rejected.
- Explicit `runtime_mode = "simulation"` for blank `with_dimensions()`
  networks; it cannot masquerade as a loaded Spikenaut checkpoint.
- crates.io package metadata: `readme`, `homepage`, `documentation`,
  `keywords`, `categories`, docs.rs config, and `exclude` for repo-only
  files (CI, agent docs, Docker). Dual MIT/Apache-2.0 license files stay
  in the package.
- Tick-loop regression tests for `neuromod` 0.6 modulator snapshots, default
  ingress fallback, and the 0.6.0 `SpikingNetwork` serde contract
  (`stdp_config` / `eligibility`) used by checkpoint loading (#41).
- GitHub Actions CI matrix: stub build/test/clippy on Linux, macOS, and
  Windows; rustfmt and optional `corpus-ipc` / libzmq jobs on Linux only
  (`docs/ci.md`).
- Stub vs `corpus-ipc` backend feature truth table in `README.md`: which Cargo flags wire which backend, which TOML keys apply, and which env vars are no-ops under stub. Documents that `model_path` is passed literally (no `~` expansion) but currently ignored by published `ZmqIpcBackend`, that `SPIKENAUT_ZMQ_READOUT_IPC` is binary-set compatibility only, and that `log_level` is binary tracing-init only.
- GitHub Actions CI workflow for formatting, clippy, build, and test validation.
- Config-driven `ServiceRegistry` and `BrainstemDaemon` in the library.
- `DaemonConfig.services` field for registering named, enabled services.
- `## Role and boundary matrix` documentation in `README.md`.
- Local `StimulusSource` / `SpikeSink` traits + `IngressPacket` / `SpikeEvent` (owned by this crate).
- `BackendPair` + `BackendPair::stub()` for pluggable I/O.
- In-crate stub backend (`StubStimulusSource`, `NoopSpikeSink`) plus a public
  `CollectingSpikeSink` for tests and the Thalamic integration smoke harness.
- CPU-only Thalamic → `corpus-ipc` → Brainstem smoke (`tests/thalamic_brainstem_smoke.rs`,
  `required-features = ["corpus-ipc"]`). A Thalamic fixture with no `SpikingNetwork`
  publishes typed `IpcMessage::Stimuli(StimulusBatch)` JSON; Brainstem loads a Distill
  sidecar, validates schema/width/freshness/`valid_mask`, and ticks through
  `BrainstemDaemon::run_for_ticks`.
- Typed `corpus-ipc` ingress (`accept_ipc_json`): schema token
  `corpus-ipc.stimulus.v1`, channel-width check (or unspecified-width when
  `expected_channels = 0`), future-timestamp and max-age rejection, session_id
  passthrough, and `RuntimeStats` accepted/rejected counters. Rejected frames
  still advance the network. Modulation-only frames are drained in the same tick.
- `BrainstemDaemon::with_backend(cfg, pair)` constructor for tests and custom backends.
- Test coverage for the non-`corpus-ipc` (stub) path that runs under `--no-default-features`.
- Graceful `SIGTERM` handling alongside the existing `SIGINT` (Ctrl-C): the tick loop now
  breaks, flushes the backend, and exits `0` on either signal.
- Bounded per-class ingress (`BoundedIngress`) in the tick loop: configured capacities,
  overflow policies (`block_timeout`, `reject`, `drop_oldest`, `coalesce`), and
  accepted/rejected/dropped/coalesced/depth/high-water-mark/producer-wait metrics.
  Optional `[ingress]` TOML section; omitted keys keep the documented defaults.
  Shutdown unblocks waiting producers. Configured `max_payload_len` is
  rejected above `MAX_PAYLOAD_LEN` (1048576). Startup-fail paths in
  `run_loop` call `BoundedIngress::shutdown()` so blocked producers do not
  wait out `block_timeout_ms`.

### Changed

- Switch optional `corpus-ipc` from a git pin to crates.io `0.1.0`
  (`features = ["zmq"]`). ZMQ SUB ingress decodes unversioned JSON
  `IpcMessage` (`Stimuli` / `Neuromodulators`) with crates.io types; egress
  publishes unversioned `IpcMessage::Spikes` JSON. The binary sets
  `CORPUS_IPC_ZMQ_READOUT_IPC` and still sets `SPIKENAUT_ZMQ_READOUT_IPC` for
  older tooling.
- Upgrade `neuromod` from 0.4.0 to crates.io **0.6.0** (pre-1.0 range
  `>=0.6.0, <0.7.0`). Ingress modulators map to dopamine / serotonin /
  acetylcholine / norepinephrine; `cortisol`, `tempo`, and `aux_dopamine`
  were removed upstream. The tick loop still calls `SpikingNetwork::step`
  (0.6 thread-local wrapper around `step_with_rng`). Checkpoints now carry
  engine-wired R-STDP (`stdp_config` / per-LIF `eligibility`).
- Align MSRV and toolchain pins to **Rust 1.98.1** (`Cargo.toml`,
  `rust-toolchain.toml`, CI, `Dockerfile`, `AGENTS.md`, `README.md`,
  `.devin/blueprint.yaml`) so stub and optional `corpus-ipc` 0.1 builds
  share one pin (no `--ignore-rust-version`).
- Relicense from GPL-3.0 to dual MIT/Apache-2.0.
- `model_path` in live mode is a real checkpoint path (Distill sidecar JSON), not backend-initialization trivia. Simulation mode is the only remaining blank-network path.
- Add SPDX license identifiers to all source files.
- Refactor `soma-daemon` binary into a thin wrapper over `BrainstemDaemon`.
- Renamed the legacy `soma-daemon` binary to `brainstem-daemon` (matches the crate/repo name).
- Made `corpus-ipc` + `zmq` **optional** behind the `corpus-ipc` Cargo feature (temporarily off by default).
- `BrainstemDaemon` now drives the tick loop via the local traits instead of hard-coding `ZmqBrainBackend`.
- Binary now logs the active backend mode (`🔌 stub` / `📡 ZMQ corpus-ipc`).
- `decode_inputs` now accepts `&IngressPacket` (with explicit `None` modulator fallback).
- All direct `corpus_ipc` / `zmq` usage is now feature-gated (except the compatibility `CORPUS_IPC_READOUT_ENV` const).
- Package version **0.3.0** (`Cargo.toml` / `Cargo.lock`). README install and
  dependency snippets are **post-publication** (`brainstem-daemon = "0.3.0"`
  once the crate is on crates.io). Default vs `corpus-ipc` examples are
  separate tables so they cannot be pasted as a duplicate TOML key. This
  changelog section is pending registry publication; `cargo publish`, a Git
  tag, and a GitHub Release remain maintainer steps.

### Removed

- Qodana Cloud workflow (`.github/workflows/qodana_code_quality.yml`) and
  `qodana.yaml`; local Qodana notes in `REVIEW.md`. Qodana Cloud membership
  expired.

### Fixed / Cleaned

- Fail-closed non-finite ingress (`NaN`, `±Inf`) on every runtime entry path
  before `SpikingNetwork::step`: typed `corpus-ipc` validation, custom
  `StimulusSource` packets, direct `BoundedIngress` enqueue, and the drained
  merge packet via shared `ingress::first_non_finite`. The daemon finite
  gates increment `rejected_batches` and emit rate-limited diagnostics with a
  stable limiter key (not per-index). Typed `corpus-ipc` source warnings
  (`next_ingress` / `hold_or_reject`) are separate and are not passed through
  that limiter. A non-finite payload does not refresh `input_freshness`; a
  rejected frame that replays previously held finite modulators still can.
  Modulators are checked before any held/cached state update. Masked-out
  stimulus slots and short modulator tails (1..4 elements) are still scanned
  fail-closed. Already-`rejected` backend frames with non-finite payloads are
  counted once and not re-admitted (#72, #73, LIM-1322 / #66).
- **Backend vs direct-enqueue rejection:** a non-finite **backend** packet is
  dropped before enqueue. The tick still drains anything already queued,
  copies that sensory vector, and zero-fills only channels the vector does
  not cover (the whole buffer when the drain has no sensory input). A
  non-finite packet that surfaces only on the **drained** path (for example
  public `enqueue` / `try_enqueue`) skips `network.step` and spike emission
  for that tick while the daemon stays live.
- Live restore rejects Distill sidecars that carry `output_weights` (including
  explicit `null`) instead of validating then silently dropping the readout
  row. neuromod 0.6.0 has no Distill readout matrix; legacy sidecars that omit
  the field still load.
- Live restore rejects Distill values that are finite as `f64` but overflow `f32`, and requires Distill `source = "spikenaut_julia"`. The binary restores once before sockets and reuses that network for the tick loop. Live mode sets `RmStdpConfig.reward_lr = 0` so dopamine-gated R-STDP cannot retrain Distill weights; `neuromod` 0.6.0 `step` still owns decay/threshold/L1-renorm as engine contracts.
- Removed unconditional dependency on `corpus-ipc` git crate and system `libzmq` for core builds and tests.

## [0.1.2] - 2026-04-22

- Migrated daemon to `corpus-ipc` and `neuromod` v0.4.0.

## [0.1.1] - 2026-04-08

- Initial `soma-daemon` binary with TOML configuration, ZeroMQ PUB/SUB, and
  `neuromod::SpikingNetwork` integration.
