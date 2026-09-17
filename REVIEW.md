# Local review quality gate

These commands are the **human quality bar** beyond GitHub Actions.
Run them before claiming a PR is ready when the change touches `src/`,
`Cargo.toml`, public APIs, or CI.

This file is the local checklist. GitHub Actions runs the stub commands on
Linux, macOS, and Windows, and the optional `corpus-ipc` job on Linux
only. See [`docs/ci.md`](docs/ci.md).

## MSRV (minimum supported Rust version) pin rule

`Cargo.toml` `rust-version`, `rust-toolchain.toml` `channel`, and every
`toolchain:` string in `.github/workflows/ci.yml` must stay **identical**
(currently **1.98.1**). `Dockerfile` `FROM rust:` tags and
`.devin/blueprint.yaml` rustup pins must match too.

To bump MSRV:

1. Set the new version in `Cargo.toml`, `rust-toolchain.toml`, `ci.yml`,
   `Dockerfile`, `.devin/blueprint.yaml`, `README.md`, `AGENTS.md`,
   `docs/ci.md`, and `REVIEW.md`.
2. Run the mandatory stub commands below on that toolchain.
3. Do not bump only one pin.

## When to run

- Before every push that changes `src/`, `Cargo.toml`, or CI
- After resolving merges with `main`
- Before requesting review or merge

## Backends

Default features are empty. That path uses the in-memory **stub** backend
and does **not** need `libzmq`.

The optional `corpus-ipc` feature (same as `--all-features` today) links
ZeroMQ. Install `libzmq` first (`libzmq3-dev` on Debian/Ubuntu).

See README [Backends (temporary)](README.md#backends-temporary) for the
feature → backend → config-key truth table (including env vars that are
no-ops under stub).

## Mandatory commands (stub, no ZMQ)

```bash
# Success is silent: exit 0 and no stdout means formatting is clean.
cargo fmt --check

cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo test --locked
```

## Optional `corpus-ipc` matrix (needs libzmq)

```bash
# Debian/Ubuntu
sudo apt-get install -y libzmq3-dev

cargo clippy --locked --all-targets --features corpus-ipc -- -D warnings
cargo test --locked --features corpus-ipc

# CI today uses --all-features (equivalent while corpus-ipc is the only feature)
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --all-features
cargo test --locked --all-features
```

If a `--all-features` build fails because the C++ compiler cannot find a
standard-library header, set `CC=gcc CXX=g++` first (see `AGENTS.md`).

## Diff hygiene

```bash
git fetch origin main
git diff --stat origin/main...HEAD
```

Expect only intentional files. Do not commit local tooling dirs
(`.idea/`, `.kilo/`, `.mimocode/`, `.worktrees/`).

```bash
git ls-files .idea .kilo .mimocode .worktrees   # must print nothing
```

## Pass criteria

- All mandatory stub commands exit 0
- `cargo fmt --check` is silent (no output) with exit 0
- Clippy reports zero warnings under `-D warnings`
- Default (stub) tests pass without `libzmq`
- If you touched `corpus-ipc` / ZMQ code, the optional feature matrix
  also passes
- Diff contains only intentional files
