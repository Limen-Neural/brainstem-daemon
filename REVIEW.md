# Local review quality gate

These commands are the **human quality bar** beyond GitHub Actions.
Run them before claiming a PR is ready when the change touches `src/`,
`Cargo.toml`, public APIs, or CI.

This file is the local checklist. Multi-OS CI is tracked separately in
[#21](https://github.com/Limen-Neural/brainstem-daemon/issues/21) and is
not duplicated here.

## When to run

- Before every push that changes `src/`, `Cargo.toml`, or CI
- After resolving merges with `main`
- Before requesting review or merge

## Backends

Default features are empty. That path uses the in-memory **stub** backend
and does **not** need `libzmq`.

The optional `corpus-ipc` feature (same as `--all-features` today) links
ZeroMQ. Install `libzmq` first (`libzmq3-dev` on Debian/Ubuntu).

## Mandatory commands (stub, no ZMQ)

```bash
# Success is silent: exit 0 and no stdout means formatting is clean.
cargo fmt --check

cargo clippy --all-targets -- -D warnings
cargo test
```

## Optional `corpus-ipc` matrix (needs libzmq)

```bash
# Debian/Ubuntu
sudo apt-get install -y libzmq3-dev

cargo clippy --all-targets --features corpus-ipc -- -D warnings
cargo test --features corpus-ipc

# CI today uses --all-features (equivalent while corpus-ipc is the only feature)
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

If a `--all-features` build fails because the C++ compiler cannot find a
standard-library header, set `CC=gcc CXX=g++` first (see `AGENTS.md`).

## Qodana (optional local)

CI twin: `.github/workflows/qodana_code_quality.yml` (`qodana.yaml`).

```bash
qodana scan --project-dir . --linter qodana-rust --print-problems --save-report
```

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
