# Usefulness experiment report

Tracker: LIM-1457 / GitHub issue #70. Protocol: [`PROTOCOL.md`](PROTOCOL.md).

| Item | Value |
|---|---|
| Pre-registration commit | `7b145a1` (protocol, fixture, workload, expectations, configs) |
| Measured commit | `d3f369320b226223644284952b9f60f732175480` |
| Results | [`results/d3f369320b226223644284952b9f60f732175480/`](results/d3f369320b226223644284952b9f60f732175480/): `identity.json`, `summary.json`, `summary.md`, raw `*.jsonl.gz`, `steps.log`, per-step command logs under `logs/`, and the three daemon logs |
| Decision (pre-registered rule, §6) | **Supports further development** of the runtime path |

The results commit is the child of the measured commit; `run.sh` refused to
run on a dirty tree, so every raw file was produced by the code at the
measured SHA.

Run history. The branch was rebased onto `main` (which bumped `neuromod`
from 0.6.0 to 0.7.0) after the first three runs, so their commit ids were
rewritten; the original ids are kept here because `identity.json` files
record them.

| run | original id | rebased id | outcome |
|---|---|---|---|
| first attempt | `b4bdbb1` | `04df820` | failed in the transport driver: the sandbox image has no `kill` binary and daemon stdout was not captured. `identity.json`, `steps.log` and command logs retained under `results/b4bdbb1…/`. |
| first complete run | `40cfd05` | `f85f427` | all checks passed, same decision; superseded after code-health review restructured the drivers. Removed from the tree, in history. |
| second complete run | `a3247e5` | `3671b6f` | all checks passed, same decision, on neuromod 0.6.0; superseded by the run below after a further restructuring and the rebase. Removed from the tree, in history. |
| measured run | — | `d3f3693` | reported here, on neuromod 0.7.0. |

## Setup (measured run)

- Hardware: AMD EPYC 9354P, 64 logical CPUs, 791,774,148 kB RAM, shared
  sandbox (no CPU pinning, no isolation from other tenants).
- OS: Debian GNU/Linux 12, kernel 6.8.0-137-generic.
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1`,
  `--locked`, release profile, features `corpus-ipc` (published `corpus-ipc`
  0.1, `neuromod` 0.7.0, `zmq` 0.10, system `libzmq3-dev`). The 0.7.0
  normal-mode step pipeline is identical to 0.6.0 (every helper diffed), and
  the pre-registered expectations were left untouched; the equality checks
  below confirm it empirically.
- Fixture: `lif8x16-v1`, SHA-256
  `9d30e6a1968db99552974577025c676663ca3d88d11a9445abbd270e4c1b3a60`,
  synthetic and untrained, loaded through the live checkpoint gate
  (`model_id = spikenaut-snn:usefulness-lif8x16-v1`).
- Workload: 20,000 ticks (10 × 2,000), 150 stimulus frames and 10
  modulator-only packets per run (counts observed by the source and
  recorded in each summary), 1,000 warm-up ticks excluded from latency.
- Repetitions: 5 × daemon-live, 5 × baseline, 5 × daemon-simulation,
  3 × transport-live; each a separate process.
- Measurement: `Instant` pairs per tick (daemon: source return → sink
  `emit`; baseline: around `SpikingNetwork::step`), `VmHWM` from
  `/proc/<pid>/status`, nearest-rank percentiles. Transport: producer send
  instant → subscriber receive instant per pulse, correlated by order.

## Results against the pre-registered checks

All values are measured unless marked derived.

**Correctness (C1–C9), in-process.** All pass in all 15 runs.

- C1/C2: 20,000 ticks, 150 accepted batches, 0 rejected, 0 receive/emit
  errors, 0 dropped spikes in every daemon run.
- C3: health `ready = true`, phase `running`, no fatal state after each run.
- C4: daemon-live per-tick spike sets equal the baseline sets on all 20,000
  ticks in every pairing; all five baseline runs are identical.
- C5/C7: daemon-live sets equal the independent reference
  (`expected_spikes.json`) on every tick, including idle ticks; 150 nonempty
  ticks per run, exactly the pre-registered ones.
- C6: masked-channel segment equals the zero-equivalent segment in all 10
  cycles.
- C8: simulation control emitted 0 spikes on all 20,000 ticks while still
  accepting all 150 batches.
- C9: ingress sensory `accepted = 150`, reward `accepted = 20`, no rejects or
  drops in any class.

**Transport (T1–T5).** All pass in all 3 runs.

| run | ready after | prime attempts | pulses sent / matched / lost | frames received (empty) | e2e p50 | e2e p99 | e2e max | daemon `VmHWM` | SIGTERM → exit |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|
| r1 | 10.2 ms | 1 | 200 / 200 / 0 | 4,250 (4,050) | 1.125 ms | 1.325 ms | 2.159 ms | 6,996 kB | 6.72 ms |
| r2 | 10.2 ms | 1 | 200 / 200 / 0 | 4,250 (4,050) | 1.113 ms | 2.022 ms | 4.056 ms | 6,920 kB | 6.05 ms |
| r3 | 10.2 ms | 1 | 200 / 200 / 0 | 4,251 (4,051) | 1.113 ms | 1.232 ms | 2.468 ms | 6,976 kB | 6.33 ms |

Each measured pulse produced exactly one nonempty `Spikes` frame carrying the
expected channel set; every idle tick produced an empty frame; `/metrics`
reported `brainstem_ready 1` and `brainstem_fatal 0`; exit status 0 with
`Termination signal received` in each committed daemon log.

**Performance, in-process** (per-tick latency after warm-up; see
`summary.md` for every run).

| mode | p50 | p99 | max | ticks/s | peak RSS |
|---|---:|---:|---:|---:|---:|
| baseline (step only), 5 runs | 0.93–0.98 µs | 1.21–1.45 µs | 9–31 µs | 898k–988k | 4.8–4.9 MB |
| daemon-live, 5 runs | 1.06–1.23 µs | 2.92–2.98 µs | 20 µs – 1.72 ms | 700k–762k | 6.2 MB |
| daemon-simulation, 5 runs | 0.58–0.63 µs | 1.62–1.79 µs | 7–43 µs | 1,182k–1,389k | 5.6–5.7 MB |

Typed decode (`accept_ipc_json`) per stimulus frame, daemon runs: p50
1.10–1.16 µs (counted separately from tick latency).

Derived: daemon-live p99 ≤ 1 ms in every run (budget met, margin > 300×).
Two single ticks exceeded 1 ms (1.70 ms in r4, 1.72 ms in r5, one tick each
out of 19,000 measured); the criterion is p99, so they do not change the
decision, but they are the first thing to look at before any hard real-time
claim on a shared host. Transport p99 ≤ 5 ms with zero loss in every run
(budget met; the largest single pulse took 4.1 ms).

## What works, what fails, limitations

Works:

- The supported live path (`DaemonConfig::load` → `try_with_backend` →
  `run_for_ticks`) restores a nonblank sidecar, decodes typed `corpus-ipc`
  frames with masks and modulators, and produces exactly the spikes an
  independent model predicts, with correct accounting and health.
- The daemon adds roughly 0.1–0.3 µs median and 1.5–1.8 µs p99 over the
  bare engine step for validation, bounded ingress, health and accounting,
  plus ~1.1 µs decode per frame; memory overhead is ~1.4 MB in-process and
  ~2.1 MB for the full binary with ZeroMQ.
- The binary is ready in ~10 ms, delivers every pulse over loopback ZeroMQ
  with ~1.1 ms median end-to-end (one tick period of quantization), and
  exits cleanly 6–7 ms after SIGTERM.
- Simulation mode behaves as a genuine negative control (no spikes).

Fails: nothing in the pre-registered set.

Limitations (all pre-registered or observed):

- The fixture is synthetic and untrained. This experiment shows that the
  runtime moves inputs to outputs correctly and cheaply; it says nothing
  about trained-model usefulness. The published Hugging Face sidecar still
  cannot be used because it carries `output_weights`, which the loader
  rejects by design.
- The engine's thread-local RNG is not checkpointed; equality holds here
  only because `reward_lr = 0.0` makes weights immutable. A nonzero reward
  rate would make per-tick equality probabilistic.
- The engine mutates decay/threshold state during stepping; the reference
  model reproduces this, and a different fixture would need the same care.
- Transport has no delivery acknowledgments and no input/output correlation
  (batch/session ids are not propagated), so matching is by order and time.
- `tempo` maps positionally onto norepinephrine; a nominal `tempo = 1.0`
  would drive the stress multiplier to its floor (0.1). Worth revisiting in
  the decoder.
- Linux-only (`/proc`, `sh -c kill`), shared host, 20,000-tick runs
  (~25 ms of measured time each). Single-tick outliers above 1 ms appeared
  in two of the four complete runs (never more than two ticks per run);
  this is consistent with scheduler noise on a shared host rather than a
  property of the daemon, but it was not attributed further. The p99
  criteria were met in every run.
- Release qualification and publication stay in LIM-1447; this is not a
  release gate.

## Protocol deviations

None to the checks, metrics, or criteria. Post-registration code changes
were limited to the drivers and tooling: the transport driver fix after the
first failed attempt (`sh -c kill`, stdout capture), two restructurings into
smaller files and functions after code-health review, and the rebase onto
`main` that moved the engine to neuromod 0.7.0. Each restructuring
regenerated the fixtures byte-for-byte and reproduced the previous run's
`summary.md` exactly from its raw data; the engine bump left the
pre-registered expectations unchanged and all equality checks pass on it.

The reproduction toolchain in `PROTOCOL.md` and `run.sh` later moved from
Rust 1.98.1 to Rust 1.99.0 with the crate's minimum supported Rust version.
The committed result files still reflect the Rust 1.98.1 run that produced
them.

## Recommendation

The evidence supports continuing development of the runtime path: the
daemon is correct, deterministic on this fixture, and its overhead over the
bare engine is microseconds against a 1 ms tick budget. The open gap is the
model side, not the runtime: a compatible trained artifact without
`output_weights` (or loader support for a readout matrix) is needed before
any usefulness claim beyond runtime plumbing can be made.
