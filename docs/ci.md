# Continuous integration

Two pipelines check that the default stub backend is portable, and that
optional ZeroMQ features still build where system `libzmq` is available.

- **GitHub Actions** (`.github/workflows/ci.yml`) — default PR/push checks.
- **Azure Pipelines** (`azure-pipelines.yml`) — independent OS matrix
  (Linux, macOS, Windows) so portability does not depend on GitHub-hosted
  runners alone.

Connect `azure-pipelines.yml` in Azure DevOps with **Pipelines → New
pipeline → GitHub** pointing at this repository. Until that project exists,
GitHub Actions is the check that actually runs on pull requests.

## GitHub Actions

| Job | Runner | Features | Commands |
|---|---|---|---|
| `rustfmt` | `ubuntu-latest` | n/a | `cargo fmt --check` |
| `stub` | `ubuntu-latest`, `macos-latest`, `windows-latest` | default (empty) | `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked` |
| `corpus-ipc` | `ubuntu-latest`, `macos-latest` | `--all-features` (the `corpus-ipc` feature, which enables the optional `zmq` dependency) | clippy, build, test (`libzmq3-dev` on Ubuntu; `brew install zeromq` on macOS) |

## Azure Pipelines

Same command matrix as GitHub Actions. Jobs are `rustfmt` (Ubuntu),
`stub` (Ubuntu / macOS / Windows), and `corpus-ipc` (Ubuntu / macOS).

`CARGO_HOME` is `$(Pipeline.Workspace)/.cargo`. Cache keys and restore-keys
are feature-scoped (`stub` vs `corpus-ipc` vs `rustfmt`) so stub and
`--all-features` jobs cannot restore each other's `target/` artifacts.
The pinned toolchain comes from `rust-toolchain.toml` (1.97.1, rustfmt,
clippy) after rustup is installed on the agent.

## Shared rules

Default features are empty. Stub jobs do **not** install `libzmq` and do
not pass `--features corpus-ipc`.

## Skips

- **Windows** skips the `corpus-ipc` / ZeroMQ job. That feature links
  system `libzmq` (`libzmq3-dev` on Debian/Ubuntu; Homebrew `zeromq` on
  macOS). Hosted Windows images do not provide that package, and
  installing it would make the portable stub matrix depend on optional
  native deps. Enable `corpus-ipc` locally on Windows only after you have
  installed ZeroMQ yourself.
- `cargo fmt --check` runs on Linux only. rustfmt output does not depend
  on the host OS.

## Local equivalent

The human checklist is in [`REVIEW.md`](../REVIEW.md). Mandatory stub
commands match CI: `cargo fmt --check`, `cargo clippy --locked --all-targets
-- -D warnings`, `cargo build --locked`, and `cargo test --locked`, plus
optional `corpus-ipc` / `--all-features` commands on Linux (and macOS after
`brew install zeromq`).
