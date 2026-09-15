# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Live-mode startup gate that loads and validates a Distill sidecar
  `snn_model.json` (Hugging Face `rmems/Spikenaut-SNN`) before the tick loop.
  Provenance (path, SHA-256, schema id, encoder, lineage) is logged. Startup
  fails closed on missing, corrupt, dimension-mismatched, non-finite, or
  blank checkpoints. FPGA Q8.8 `.mem` dumps are rejected.
- Explicit `runtime_mode = "simulation"` for blank `with_dimensions()`
  networks; it cannot masquerade as a loaded Spikenaut checkpoint.
- GitHub Actions CI matrix: stub build/test/clippy on Linux, macOS, and
  Windows; rustfmt and optional `corpus-ipc` / libzmq jobs on Linux only
  (`docs/ci.md`).
- Stub vs `corpus-ipc` backend feature truth table in `README.md`: which Cargo flags wire which backend, which TOML keys apply, and which env vars are no-ops under stub. Documents that `model_path` is passed literally (no `~` expansion) but currently ignored by pinned `ZmqBrainBackend`, that `CORPUS_IPC_ZMQ_READOUT_IPC` is binary-set compatibility only, and that `log_level` is binary tracing-init only.
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

### Removed

- Qodana Cloud workflow (`.github/workflows/qodana_code_quality.yml`) and
  `qodana.yaml`; local Qodana notes in `REVIEW.md`. Qodana Cloud membership
  expired.

### Fixed / Cleaned

- Live restore rejects Distill values that are finite as `f64` but overflow `f32`, and requires Distill `source = "spikenaut_julia"`. The binary restores once before sockets and reuses that network for the tick loop.
- Removed unconditional dependency on `corpus-ipc` git crate and system `libzmq` for core builds and tests.

## [0.1.2] - 2026-04-22

- Migrated daemon to `corpus-ipc` and `neuromod` v0.4.0.

## [0.1.1] - 2026-04-08

- Initial `soma-daemon` binary with TOML configuration, ZeroMQ PUB/SUB, and
  `neuromod::SpikingNetwork` integration.
