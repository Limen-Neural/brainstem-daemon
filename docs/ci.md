# Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) checks that the default stub
backend is portable, and that optional ZeroMQ features still build where
system `libzmq` is available.

All jobs install **Rust 1.99.0** — the same string as `Cargo.toml`
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

The same job runs the CPU-only PUB qualification tests: empty/nonempty wire
compatibility, application send counters, absent/disconnected/stalled subscribers,
and finite-linger teardown with port reuse. `tests/pub_lifecycle.rs` is also
feature-gated and Linux-only: it launches the real simulation-mode binary,
waits for `/readyz` and subscriber receipt, sends SIGTERM/SIGINT, and checks clean
exit and released PUB/control ports with bounded waits. Child processes are
killed and reaped on failure. No external producer, checkpoint, or GPU is needed.
The job has a 15-minute timeout as a final guard against native blocking calls.

## Skips

- **macOS** and **Windows** skip the `corpus-ipc` / ZeroMQ job. That
  feature compiles ZeroMQ (C/C++) via `zmq-sys`. The GitHub-hosted macOS
  and Windows images are left on the portable stub matrix so optional
  native deps do not gate default CI. Enable `corpus-ipc` locally on
  those OSes after you have a C++ toolchain (and optionally system
  ZeroMQ). The checkout's `.cargo/config.toml` passes the libzmq alignment
  defines for macOS triples. It does not pass them for Windows, which has
  no `posix_memalign`. Stub and `corpus-ipc` jobs both use Rust 1.99.0.
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
safe extraction filter), GCC/G++, `libzmq3-dev`, and Rust 1.99.0:

```bash
# Run from a clean, committed checkout. Use a new, empty evidence directory.
python3 .github/scripts/qualify-package.py target/package-evidence
```

The driver runs these gates without `--allow-dirty`, `--no-verify`, or
`--ignore-rust-version`:

1. Record `cargo +1.99.0 package --locked --list`. Require manifests and
   lockfile, README, both licenses, toolchain pin, VCS identity, and every tracked
   source/test fixture. Reject repository-only directories and build artifacts.
2. Run `cargo +1.99.0 package --locked`, retain the `.crate` and SHA-256,
   verify its embedded Git identity, and extract it outside the checkout.
3. Build/test the extracted package separately with default features and
   `--features corpus-ipc`. Run `cargo doc --locked --no-deps` with
   `RUSTDOCFLAGS="-D warnings"` in each mode.
4. `cargo install --locked --path <extracted-package> --root <isolated-root>`
   in each mode, using the normal release profile. Verify `--help`, matching
   `--version`, simulation readiness (`/readyz`), successful bounded SIGINT
   and SIGTERM shutdown, and nonzero checkpoint-specific failures for missing
   and corrupt live checkpoints. Also start a live instance from the packaged
   synthetic fixture, wait for `/readyz`, and require a graceful SIGTERM
   exit. No external IPC publisher or checkpoint download is required.
   A daemon that exits before readiness fails the gate, unless that attempt's
   log shows it lost a race for a freshly allocated loopback port; then up to
   three startup attempts are made in total, each with its own config file.
5. Build and run a tiny external path-dependent library consumer in each mode.
   It loads simulation config, restores the blank simulation network, and
   verifies that missing live checkpoints are rejected through the public API.
6. Run `cargo +1.99.0 publish --dry-run --locked` from the clean checkout.
   Cargo packages/verifies it again; **nothing is uploaded to crates.io**.

CI uploads `package-qualification-<candidate-sha>` with 90-day retention when
the driver has written evidence, including partial logs if a later gate fails.
If checkout or toolchain setup fails first, there is no evidence directory and
the upload step warns instead of failing the job a second time. `identity.json`
records the exact SHA, archive checksum, and qualification status; only
`passed` means all gates finished. Keep the artifact with the release record
before it expires, and rerun qualification if the candidate SHA changes. Logs
and temporary binaries are not committed; build/install/consumer directories
are cleaned automatically.

### Registry qualification

Before actual publication, rerun the gate on the selected commit and retain
its evidence. Confirm registry ownership and version availability with the
maintainer. The dry-run does not publish.

After an authorized upload, verify both version-pinned registry installs
in separate install roots. A registry install does not read the crate's
`.cargo/config.toml` like the extracted `--path` install does. The archive
driver also injects the POSIX ZeroMQ flags itself. For the Linux registry
`corpus-ipc` smoke, append the same flags explicitly:

```bash
stub_root="$(mktemp -d)"
ipc_root="$(mktemp -d)"
cargo +1.99.0 install --locked --version 0.3.0 --root "$stub_root" brainstem-daemon

CFLAGS="${CFLAGS:+$CFLAGS }-DHAVE_POSIX_MEMALIGN -DZMQ_CACHELINE_SIZE=64" \
CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-DHAVE_POSIX_MEMALIGN -DZMQ_CACHELINE_SIZE=64" \
cargo +1.99.0 install --locked --version 0.3.0 --root "$ipc_root" \
  --features corpus-ipc brainstem-daemon
```

The flags are for the POSIX native build; do not pass them on Windows.

Run `--help` and `--version` on each installed binary by its full path and
check for version `0.3.0`. Use the [README configuration](../README.md#configuration)
with an enabled loopback `control_bind`. For each binary, start it in
simulation, then with the bundled live fixture. Require HTTP 200 from
`/readyz` and exit 0 after SIGTERM on both runs; a missing live checkpoint
must exit nonzero with a checkpoint error.

For Linux `corpus-ipc`, also exercise the native queues with actual input
and output. Run the existing transport driver from the release checkout
with its configured loopback ports (5755, 5756, 9755) free. Point it at the
**registry-installed** binary, not a binary under `target/`:

```bash
smoke_dir="$(mktemp -d)"
cargo +1.99.0 run --locked --features corpus-ipc --example usefulness_transport -- \
  --daemon "$ipc_root/bin/brainstem-daemon" \
  experiments/usefulness/config/transport-live.toml \
  experiments/usefulness/fixtures/workload.json \
  "$smoke_dir/frames.jsonl" "$smoke_dir/summary.json"
```

The driver waits for readiness and subscriber frames, sends typed stimuli,
and stops the daemon with SIGTERM. Require at least one matching nonempty
spike response (`matched > 0`) and a clean shutdown (`exit_code = 0`,
`termination_logged = true`) in `summary.json`. An install or readiness
probe alone does not exercise the ZeroMQ queues.

Finally, verify a registry-only library consumer, the docs.rs result, and
the release notes/tag against the same commit.
Retain the runtime logs and transport summary with the release evidence
before removing the temporary `stub_root`, `ipc_root`, and `smoke_dir`
directories. Keep those roots until all of the installed-binary checks finish.
