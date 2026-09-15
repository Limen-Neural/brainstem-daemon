# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- GitHub Actions CI matrix: stub build/test/clippy on Linux, macOS, and
  Windows; rustfmt and optional `corpus-ipc` / libzmq jobs on Linux only
  (`docs/ci.md`).
- Stub vs `corpus-ipc` backend feature truth table in `README.md`: which Cargo flags wire which backend, which TOML keys apply, and which env vars are no-ops under stub. Documents that `model_path` is a checkpoint path when the file exists (otherwise a blank network), that `CORPUS_IPC_ZMQ_READOUT_IPC` is the SUB endpoint for typed JSON `IpcMessage` frames, and that `log_level` is binary tracing-init only.
- GitHub Actions CI workflow for formatting, clippy, build, and test validation.
- Config-driven `ServiceRegistry` and `BrainstemDaemon` in the library.
- `DaemonConfig.services` field for registering named, enabled services.
- `## Role and boundary matrix` documentation in `README.md`.
- Local `StimulusSource` / `SpikeSink` traits + `IngressPacket` / `SpikeEvent` (owned by this crate).
- `BackendPair` + `BackendPair::stub()` for pluggable I/O.
- In-crate stub backend (`StubStimulusSource`, `NoopSpikeSink`, `CollectingSpikeSink`).
- `BrainstemDaemon::with_backend(cfg, pair)` constructor for tests and custom backends.
- Test coverage for the non-`corpus-ipc` (stub) path that runs under `--no-default-features`.
- Graceful `SIGTERM` handling alongside the existing `SIGINT` (Ctrl-C): the tick loop now
  breaks, flushes the backend, and exits `0` on either signal.
- Explicit JSON checkpoint loader (`src/checkpoint.rs`) that fails closed on schema, dimension, NaN, and blank-weight fixtures.
- Typed `corpus-ipc` ingress validation (`src/ingress.rs`) for width, freshness, `valid_mask`, and schema token `corpus-ipc.stimulus.v1`.
- CPU-only Thalamic → corpus-ipc → Brainstem integration smoke test (`tests/thalamic_brainstem_smoke.rs`, `--features corpus-ipc`).
- `BrainstemDaemon::run_for_ticks` for bounded, signal-free tick runs.

### Changed

- Relicense from GPL-3.0 to dual MIT/Apache-2.0.
- Add SPDX license identifiers to all source files.
- Refactor `soma-daemon` binary into a thin wrapper over `BrainstemDaemon`.
- Renamed the legacy `soma-daemon` binary to `brainstem-daemon` (matches the crate/repo name).
- Made `corpus-ipc` + `zmq` **optional** behind the `corpus-ipc` Cargo feature (temporarily off by default).
- `BrainstemDaemon` now drives the tick loop via the local traits instead of hard-coding `ZmqBrainBackend`.
- Binary now logs the active backend mode (`🔌 stub` / `📡 ZMQ corpus-ipc`).
- `decode_inputs` now accepts `&IngressPacket` (with explicit `None` modulator fallback).
- All direct `corpus_ipc` / `zmq` usage is now feature-gated (except the compatibility `CORPUS_IPC_READOUT_ENV` const).
- `corpus-ipc` feature pins crate version **0.1.0** at git rev `3ad764a` (`IpcMessage` / `StimulusBatch`; crates.io does not yet resolve `corpus-ipc = "0.1"`). ZMQ ingress decodes JSON `IpcMessage::Stimuli` rather than the legacy binary readout packet.
- MSRV / `rust-toolchain.toml` aligned to **1.98.1** so the 0.1.0 `corpus-ipc` crate compiles.

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
