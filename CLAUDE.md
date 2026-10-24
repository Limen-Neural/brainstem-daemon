@AGENTS.md

# CLAUDE.md

Claude-specific notes only. Boundaries, the stub vs `corpus-ipc` backend split, config and run instructions, and the
toolchain pin rule are in `AGENTS.md` (imported above); do not duplicate them here. The pre-PR gate is `REVIEW.md`.

## Commands

Run from the repo root. "Verified" means it ran successfully in a fresh checkout on Rust 1.98.1 (the pin in
`rust-toolchain.toml`), Linux, CPU only. `Cargo.lock` is tracked, so `--locked` is correct.

| Command | Status |
|---|---|
| `cargo fmt --check` | verified |
| `cargo clippy --locked --all-targets -- -D warnings` | verified |
| `cargo build --locked` | verified |
| `cargo test --locked` | verified (159 unit tests passed, 1 ignored; plus `config_startup`) |
| `CC=gcc CXX=g++ cargo clippy --locked --all-targets --all-features -- -D warnings` | verified (no system `libzmq`; `zmq-sys` built from source) |
| `CC=gcc CXX=g++ cargo test --locked --all-features` | verified (182 unit tests passed, 1 ignored; plus `config_startup`, `thalamic_brainstem_smoke`, `pub_lifecycle`) |
| `cargo build --release --bin brainstem-daemon` | not run (disk limits; the debug builds above cover compilation) |
| `python3 .github/scripts/qualify-package.py target/package-evidence` | not run (CI `package` job; needs `libzmq3-dev`, runs packaging and publish dry-run) |

CI (`.github/workflows/ci.yml`) runs `fmt`, `stub` (Linux, macOS, Windows), `corpus-ipc` (Linux only), and `package`.
`--all-features` is safe here: it only enables `corpus-ipc` (ZeroMQ), with no CUDA or GPU. Run fmt and clippy before committing.

## Architecture map

- `src/lib.rs`: crate root. `src/bin/brainstem_daemon.rs`: the CLI binary, which wires ZeroMQ under `corpus-ipc`.
- `src/config.rs`: `DaemonConfig` TOML types, serde defaults, PUB socket validation. `registry.rs`: config-driven `ServiceRegistry`.
- `src/daemon.rs`: `BrainstemDaemon` tick loop (wall-clock `tokio` time). `backend.rs`: local stimulus-ingress and spike-egress traits (`BackendPair`, stub by default).
- `src/checkpoint/`: Distill sidecar (`snn_model.json`) loader and the live-mode startup gate. `ingress/`: bounded queues feeding the tick loop (`corpus.rs` for ZeroMQ).
- `src/health/`: liveness, readiness, degradation, sticky fatal state.
- `src/control.rs`: optional health and metrics listener. `src/logging.rs`: logging.
- `src/runtime/`: deterministic restart and fault-injection harness (fakes, clock, durable state).
- `tests/`: `config_startup.rs`; `thalamic_brainstem_smoke.rs` and `pub_lifecycle.rs` require `corpus-ipc` (Linux-only lifecycle test).
- `docs/ci.md`: CI matrix and skips. `docs/health.md`: health contract. `README.md` has the backend truth table and the role and boundary matrix.

## Invariants and non-goals (from `AGENTS.md` and `README.md`)

- Inference-only runtime. No training or weight optimization (`plasticity-lab`), trading or mining adapters, or hardware control (`Spikenaut-Hardware`).
- `corpus-ipc` and `zmq` stay optional and off by default; the default stub path opens no ZeroMQ sockets and needs no `libzmq`, but an optional `control_bind` still opens the HTTP control listener.
- Live mode fails closed on a missing, corrupt, or non-finite checkpoint. Do not add a silent fallback to a blank `with_dimensions()` network.
- SNN dynamics come from `neuromod` 0.7; the daemon applies neuromodulator signals from `limbic-critic` but does not generate them.
- Keep `Cargo.toml` `rust-version`, `rust-toolchain.toml`, every CI `toolchain:` string, `Dockerfile`, and `.devin/blueprint.yaml` on the same version (`REVIEW.md`).
- New `.rs` files should start with `// SPDX-License-Identifier: MIT OR Apache-2.0`. `src/config.rs`, `src/logging.rs`, and the `src/health/tests*.rs` files currently lack it.
- `Cargo.toml` `exclude` lists `AGENTS.md`, so `CLAUDE.md` is listed there too.

## Gotchas

- Stub and `corpus-ipc` builds use separate cache keys in CI; the first `--all-features` build compiles ZeroMQ and took about 2 minutes here.
- If a `--all-features` build cannot find a C++ standard-library header, set `CC=gcc CXX=g++`.
- Never run `cargo publish` or create release tags from an agent session.
- `.claude/` is gitignored, so keep shared agent guidance in this file or `AGENTS.md`.
- Edit only what the task needs.
