# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
- In-crate stub backend (`StubStimulusSource`, `NoopSpikeSink`, `CollectingSpikeSink` under `#[cfg(test)]` for our own tests; not re-exported for downstream test use).
- `BrainstemDaemon::with_backend(cfg, pair)` constructor for tests and custom backends.
- Test coverage for the non-`corpus-ipc` (stub) path that runs under `--no-default-features`.
- Graceful `SIGTERM` handling alongside the existing `SIGINT` (Ctrl-C): the tick loop now
  breaks, flushes the backend, and exits `0` on either signal.

### Changed

- Switch optional `corpus-ipc` from a git pin to crates.io `0.1.0`
  (`features = ["zmq"]`). ZMQ ingress uses published `ZmqIpcBackend` /
  `IpcBackend::process_batch`; egress publishes unversioned
  `IpcMessage::Spikes` JSON. The binary sets `CORPUS_IPC_ZMQ_READOUT_IPC`
  (what 0.1 reads) and still sets `SPIKENAUT_ZMQ_READOUT_IPC` for older
  tooling.
- Upgrade `neuromod` from 0.4.0 to crates.io **0.6.0** (pre-1.0 range
  `>=0.6.0, <0.7.0`). Library `IngressPacket` modulators map to dopamine /
  serotonin / acetylcholine / norepinephrine; `cortisol`, `tempo`, and
  `aux_dopamine` were removed upstream. With `--features corpus-ipc`, the
  corpus-ipc 0.1 ZMQ 4-float tail stays Nero order (dopamine, cortisol,
  acetylcholine, tempo): DA and ACh are forwarded, and cortisol/tempo are
  dropped instead of being treated as serotonin/norepinephrine. The tick
  loop still calls `SpikingNetwork::step` (0.6 thread-local wrapper around
  `step_with_rng`). Checkpoints now carry engine-wired R-STDP
  (`stdp_config` / per-LIF `eligibility`).
- Align MSRV and toolchain pins to **Rust 1.98.1** (`Cargo.toml`,
  `rust-toolchain.toml`, CI, `Dockerfile`, `AGENTS.md`, `README.md`,
  `.devin/blueprint.yaml`, `docs/ci.md`, `REVIEW.md`) so stub and
  optional `corpus-ipc` 0.1 builds share one pin (no `--ignore-rust-version`).
- Relicense from GPL-3.0 to dual MIT/Apache-2.0.
- Add SPDX license identifiers to all source files.
- Refactor `soma-daemon` binary into a thin wrapper over `BrainstemDaemon`.
- Renamed the legacy `soma-daemon` binary to `brainstem-daemon` (matches the crate/repo name).
- Made `corpus-ipc` + `zmq` **optional** behind the `corpus-ipc` Cargo feature (temporarily off by default).
- `BrainstemDaemon` now drives the tick loop via the local traits instead of hard-coding `ZmqBrainBackend`.
- Binary now logs the active backend mode (`🔌 stub` / `📡 ZMQ corpus-ipc`).
- `decode_inputs` now accepts `&IngressPacket` (with explicit `None` modulator fallback).
- All direct `corpus_ipc` / `zmq` usage is now feature-gated (except the compatibility `CORPUS_IPC_READOUT_ENV` const).

### Removed

- Qodana Cloud workflow (`.github/workflows/qodana_code_quality.yml`) and
  `qodana.yaml`; local Qodana notes in `REVIEW.md`. Qodana Cloud membership
  expired.

### Fixed / Cleaned

- Removed unconditional dependency on `corpus-ipc` git crate and system `libzmq` for core builds and tests.

## [0.1.2] - 2026-04-22

- Migrated daemon to `corpus-ipc` and `neuromod` v0.4.0.

## [0.1.1] - 2026-04-08

- Initial `soma-daemon` binary with TOML configuration, ZeroMQ PUB/SUB, and
  `neuromod::SpikingNetwork` integration.
