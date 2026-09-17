# AGENTS.md

Last updated: 2026-09-17

This file guides autonomous agents working on `brainstem-daemon`.

## Identity

You are a Rust maintenance assistant for `brainstem-daemon`. You help build, test, lint, run, and document the headless spiking-neural-network runtime. You only work on the `Limen-Neural/brainstem-daemon` repository.

## Boundaries

- Keep changes scoped to the inference-only spiking-neural-network runtime.
- Do not add trading, mining, hardware-control, or weight-training logic.
- Do not make `corpus-ipc` or `zmq` required by default.
- Do not commit secrets, model weights, or generated build artifacts.
- Prefer minimal, idiomatic Rust and run `cargo fmt --check` before committing.

## Tools

The pre-PR quality gate (fmt, clippy, stub vs `corpus-ipc` test matrix)
lives in [`REVIEW.md`](REVIEW.md). Run that checklist before claiming a
PR is ready. GitHub Actions OS matrix and ZeroMQ skips are in
[`docs/ci.md`](docs/ci.md).

Default (stub) commands — no `libzmq` required:

- `cargo fmt --check` — verify formatting.
- `cargo build --locked` — compile the default stub backend.
- `cargo clippy --locked --all-targets -- -D warnings` — lint the stub path.
- `cargo test --locked` — run stub-backend tests.
- `cargo build --release --bin brainstem-daemon` — build the release binary.

Optional `corpus-ipc` / `--all-features` commands need the system ZeroMQ
dev package (`libzmq3-dev` on Debian/Ubuntu). They are not vendored.
Run them on the **1.98.1** pin (`rust-toolchain.toml` / `Cargo.toml`
`rust-version`); published `corpus-ipc` 0.1 declares that MSRV, so do
not pass `--ignore-rust-version`:

- `cargo test --locked --all-features`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`

If a `--all-features` build fails because the C++ compiler cannot find a standard-library header, set the C Compiler (CC) and C++ Compiler (CXX) variables first (still on 1.98.1):

- `CC=gcc CXX=g++ cargo build --locked --all-features`
- `CC=gcc CXX=g++ cargo test --locked --all-features`
- `CC=gcc CXX=g++ cargo clippy --locked --all-targets --all-features -- -D warnings`

## Cursor Cloud setup

This repository is preconfigured on the Cursor Cloud virtual machine.
The Rust toolchain is pinned to **1.98.1 only** via `rust-toolchain.toml`.
Keep `Cargo.toml` `rust-version` and CI `toolchain:` in lockstep.
See REVIEW.md "MSRV (minimum supported Rust version) pin rule".
At startup the environment runs `cargo fetch`.

### Backend features

For most development, use the in-memory stub backend. It needs no `libzmq` and no open ports. This is the safest path for everyday work and continuous integration.

If you need ZeroMQ networking, enable the `corpus-ipc` feature.

- The `corpus-ipc` feature pulls published `corpus-ipc` 0.1 from crates.io (`features = ["zmq"]`) and this crate's optional `zmq` dependency.
- Published `corpus-ipc` compiles libzmq via `zmq-sys`. You need a C++ compiler; `libzmq3-dev` is still useful on Debian/Ubuntu.
- The `corpus-ipc` crate does not vendor ZeroMQ as a git submodule.
- Stub and `corpus-ipc` builds share this crate's MSRV **1.98.1** (same as published `corpus-ipc` 0.1).

Prefer the README [Backends (temporary)](README.md#backends-temporary) section as the user-facing truth table, unless a newer code change supersedes it.
That table maps Cargo flags to the backend and to which config keys and env vars apply.
`BrainstemDaemon::new()` uses the stub even when the feature is enabled.
The `brainstem-daemon` binary is what wires ZeroMQ.

### Running the daemon

Build the release binary:

- `cargo build --release --bin brainstem-daemon`

Start it with a TOML (Tom's Obvious, Minimal Language) configuration file:

- `./target/release/brainstem-daemon --config <path.toml>`

The default config path is platform-dependent. On Linux it is typically `~/.config/soma/daemon.toml`, resolved via `default_config_path()` and `dirs::config_dir()`.

A minimal TOML configuration includes:

```toml
lif_count      = 16
izh_count      = 5
channels       = 16
tick_rate_hz   = 1000
log_level      = "info"
spine_sub_port = 5555
spine_pub_port = 5556
model_path     = "~/models/soma16.mem"
```

The `model_path` is not used by the stub backend.
With `--features corpus-ipc` the binary passes it literally to `ZmqStimulusSource::initialize`.
`~` is not expanded, and published `ZmqIpcBackend` currently ignores `_model_path`.
With the stub backend, `brainstem-daemon` runs a headless spiking-neural-network tick loop and logs `🔌 Using stub backend`.
