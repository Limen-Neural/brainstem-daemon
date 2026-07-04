# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- GitHub Actions CI workflow for formatting, clippy, build, and test validation.
- Config-driven `ServiceRegistry` and `BrainstemDaemon` in the library.
- `DaemonConfig.services` field for registering named, enabled services.
- `## Role and boundary matrix` documentation in `README.md`.
- Local `StimulusSource` / `SpikeSink` traits + `IngressPacket` / `SpikeEvent` (owned by this crate).
- `BackendPair` + `BackendPair::stub()` for pluggable I/O.
- In-crate stub backend (`StubStimulusSource`, `NoopSpikeSink`, `CollectingSpikeSink` under `#[cfg(test)]` for our own tests; not re-exported for downstream test use).
- `BrainstemDaemon::with_backend(cfg, pair)` constructor for tests and custom backends.
- Test coverage for the non-`corpus-ipc` (stub) path that runs under `--no-default-features`.

### Changed

- Relicense from GPL-3.0 to dual MIT/Apache-2.0.
- Add SPDX license identifiers to all source files.
- Refactor `soma-daemon` binary into a thin wrapper over `BrainstemDaemon`.
- Made `corpus-ipc` + `zmq` **optional** behind the `corpus-ipc` Cargo feature (temporarily off by default).
- `BrainstemDaemon` now drives the tick loop via the local traits instead of hard-coding `ZmqBrainBackend`.
- Binary now logs the active backend mode (`🔌 stub` / `📡 ZMQ corpus-ipc`).
- `decode_inputs` now accepts `&IngressPacket` (with explicit `None` modulator fallback).
- All direct `corpus_ipc` / `zmq` usage is now feature-gated (except the compatibility `CORPUS_IPC_READOUT_ENV` const).

### Fixed / Cleaned

- Removed unconditional dependency on `corpus-ipc` git crate and system `libzmq` for core builds and tests.

## [0.1.2] - 2026-04-22

- Migrated daemon to `corpus-ipc` and `neuromod` v0.4.0.

## [0.1.1] - 2026-04-08

- Initial `soma-daemon` binary with TOML configuration, ZeroMQ PUB/SUB, and
  `neuromod::SpikingNetwork` integration.
