# Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) checks that the default stub
backend is portable, and that optional ZeroMQ features still build where
system `libzmq` is available.

## Matrix

| Job | Runner | Features | Commands |
|---|---|---|---|
| `rustfmt` | `ubuntu-latest` | n/a | `cargo fmt --check` |
| `stub` | `ubuntu-latest`, `macos-latest`, `windows-latest` | default (empty) | `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked` |
| `corpus-ipc` | `ubuntu-latest` | `--all-features --ignore-rust-version` (the `corpus-ipc` feature; published `corpus-ipc` 0.1 declares rust-version 1.98.1) | clippy, build, test |

Default features are empty. Stub jobs do **not** install `libzmq` and do
not pass `--features corpus-ipc`.

## Skips

- **macOS** and **Windows** skip the `corpus-ipc` / ZeroMQ job. That
  feature compiles ZeroMQ (C/C++) via `zmq-sys`. The GitHub-hosted macOS
  and Windows images are left on the portable stub matrix so optional
  native deps do not gate default CI. Enable `corpus-ipc` locally on
  those OSes after you have a C++ toolchain (and optionally system
  ZeroMQ). Published `corpus-ipc` 0.1 declares rust-version 1.98.1; the default
  stub jobs stay on 1.97.1.
- `cargo fmt --check` runs on Linux only. rustfmt output does not depend
  on the host OS.

## Local equivalent

The human checklist is in [`REVIEW.md`](../REVIEW.md). Mandatory stub
commands match CI: `cargo fmt --check`, `cargo clippy --locked --all-targets
-- -D warnings`, `cargo build --locked`, and `cargo test --locked`, plus
optional `corpus-ipc` / `--all-features` commands on Linux.
