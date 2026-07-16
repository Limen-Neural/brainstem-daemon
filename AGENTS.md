# AGENTS.md

Last updated: 2026-07-16

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

Common commands for this project:

- `cargo build` — compile the default stub backend.
- `cargo test --all-features` — run all tests, including the optional `corpus-ipc` feature.
- `cargo clippy --all-targets --all-features -- -D warnings` — lint the project.
- `cargo fmt --check` — verify formatting.
- `cargo build --release --bin soma-daemon` — build the release binary.

If a `--all-features` build fails because the C++ compiler cannot find a standard-library header, set the C Compiler (CC) and C++ Compiler (CXX) variables first:

- `CC=gcc CXX=g++ cargo build --all-features`

Apply the same `CC=gcc CXX=g++` prefix to `cargo test --all-features` and `cargo clippy --all-targets --all-features -- -D warnings` as needed.

## Cursor Cloud setup

This repository is preconfigured on the Cursor Cloud virtual machine. The Rust toolchain (stable, edition-2024 capable) is already installed. At startup the environment runs `cargo fetch`.

### Backend features

For most development, use the in-memory stub backend. It needs no `libzmq` and no open ports. This is the safest path for everyday work and continuous integration.

If you need ZeroMQ networking, enable the `corpus-ipc` feature. That feature pulls the `corpus-ipc` git dependency. It also builds a vendored ZeroMQ C++ library.

### Running the daemon

Build the release binary:

- `cargo build --release --bin soma-daemon`

Start it with a TOML (Tom's Obvious, Minimal Language) configuration file:

- `./target/release/soma-daemon --config <path.toml>`

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

The `model_path` is not used by the stub backend. With the stub backend, `soma-daemon` runs a headless spiking-neural-network tick loop and logs `🔌 Using stub backend`.
