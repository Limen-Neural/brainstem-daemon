# Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) checks that the default stub
backend is portable, and that optional ZeroMQ features still build where
system `libzmq` is available.

## Matrix

| Job | Runner | Features | Commands |
|---|---|---|---|
| `rustfmt` | `ubuntu-latest` | n/a | `cargo fmt --check` |
| `stub` | `ubuntu-latest`, `macos-latest`, `windows-latest` | default (empty) | `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked` |
| `corpus-ipc` | `ubuntu-latest` | `--all-features` (the `corpus-ipc` feature, which enables the optional `zmq` dependency) | clippy, build, test |

Default features are empty. Stub jobs do **not** install `libzmq` and do
not pass `--features corpus-ipc`. The Thalamic → corpus-ipc → Brainstem
integration smoke (`tests/thalamic_brainstem_smoke.rs`) is compiled only in
the Linux `corpus-ipc` job (`required-features = ["corpus-ipc"]`).

## Skips

- **macOS** and **Windows** skip the `corpus-ipc` / ZeroMQ job. That
  feature links system `libzmq` (`libzmq3-dev` on Debian/Ubuntu). The
  GitHub-hosted macOS and Windows images do not provide that package, and
  installing it would make the portable stub matrix depend on optional
  native deps. Enable `corpus-ipc` locally on those OSes only after you
  have installed ZeroMQ yourself.
- `cargo fmt --check` runs on Linux only. rustfmt output does not depend
  on the host OS.

## Local equivalent

The human checklist is in [`REVIEW.md`](../REVIEW.md). Mandatory stub
commands match CI: `cargo fmt --check`, `cargo clippy --locked --all-targets
-- -D warnings`, `cargo build --locked`, and `cargo test --locked`, plus
optional `corpus-ipc` / `--all-features` commands on Linux.
