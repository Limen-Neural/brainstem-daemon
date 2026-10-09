# Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) checks that the default stub
backend is portable, and that optional ZeroMQ features still build where
system `libzmq` is available.

All jobs install **Rust 1.98.1** — the same string as `Cargo.toml`
`rust-version` and `rust-toolchain.toml` `channel` (see REVIEW.md
"MSRV pin rule").

## Matrix

| Job | Runner | Features | Commands |
|---|---|---|---|
| `rustfmt` | `ubuntu-latest` | n/a | `cargo fmt --check` |
| `stub` | `ubuntu-latest`, `macos-latest`, `windows-latest` | default (empty) | `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked`, `cargo test --locked` |
| `corpus-ipc` | `ubuntu-latest` | `--all-features` (the `corpus-ipc` feature, which enables the optional `zmq` dependency) | clippy, build, test |
| `packaged release` | `ubuntu-latest` | default and `corpus-ipc`, separately | archive contents, extracted build/test, strict rustdoc, isolated installs and CLI smoke, external consumers, publish dry-run |

Default features are empty. Stub jobs do **not** install `libzmq` and do
not pass `--features corpus-ipc`. The Thalamic → corpus-ipc → Brainstem
integration smoke (`tests/thalamic_brainstem_smoke.rs`) is compiled only in
the Linux `corpus-ipc` job (`required-features = ["corpus-ipc"]`).

## Skips

- **macOS** and **Windows** skip the `corpus-ipc` / ZeroMQ job. That
  feature compiles ZeroMQ (C/C++) via `zmq-sys`. The GitHub-hosted macOS
  and Windows images are left on the portable stub matrix so optional
  native deps do not gate default CI. Enable `corpus-ipc` locally on
  those OSes after you have a C++ toolchain (and optionally system
  ZeroMQ). Stub and `corpus-ipc` jobs both use Rust 1.98.1.
- `cargo fmt --check` runs on Linux only. rustfmt output does not depend
  on the host OS.

## Local equivalent

The human checklist is in [`REVIEW.md`](../REVIEW.md). Mandatory stub
commands match CI: `cargo fmt --check`, `cargo clippy --locked --all-targets
-- -D warnings`, `cargo build --locked`, and `cargo test --locked`, plus
optional `corpus-ipc` / `--all-features` commands on Linux.

## Cargo archive release qualification

The `packaged release` job checks the PR's exact head SHA (or the pushed SHA
on `main`), not the synthetic PR merge commit. It deliberately avoids checkout
build caches. Reproduce it on Linux with Python 3.11.6+ (including tarfile's
safe extraction filter), GCC/G++, `libzmq3-dev`, and Rust 1.98.1:

```bash
# Run from a clean, committed checkout. Use a new, empty evidence directory.
python3 .github/scripts/qualify-package.py target/package-evidence
```

The driver runs these gates without `--allow-dirty`, `--no-verify`, or
`--ignore-rust-version`:

1. Record `cargo +1.98.1 package --locked --list`. Require manifests and
   lockfile, README, both licenses, toolchain pin, VCS identity, and every tracked
   source/test fixture. Reject repository-only directories and build artifacts.
2. Run `cargo +1.98.1 package --locked`, retain the `.crate` and SHA-256,
   verify its embedded Git identity, and extract it outside the checkout.
3. Build/test the extracted package separately with default features and
   `--features corpus-ipc`. Run `cargo doc --locked --no-deps` with
   `RUSTDOCFLAGS="-D warnings"` in each mode.
4. `cargo install --locked --path <extracted-package> --root <isolated-root>`
   in each mode, using the normal release profile. Verify `--help`, matching
   `--version`, simulation readiness (`/readyz`), successful bounded SIGINT
   and SIGTERM shutdown, and nonzero checkpoint-specific failures for missing
   and corrupt live checkpoints. No external IPC publisher is required.
5. Build and run a tiny external path-dependent library consumer in each mode.
   It loads simulation config, restores the blank simulation network, and
   verifies that missing live checkpoints are rejected through the public API.
6. Run `cargo +1.98.1 publish --dry-run --locked` from the clean checkout.
   Cargo packages/verifies it again; **nothing is uploaded to crates.io**.

CI always uploads `package-qualification-<candidate-sha>` with 90-day retention,
including partial logs on failure. `identity.json` records the exact SHA,
archive checksum, and qualification status; only `passed` means all gates
finished. Keep the artifact with the release record before it expires, and rerun
qualification if the candidate SHA changes. Logs and temporary binaries are
not committed; build/install/consumer directories are cleaned automatically.
