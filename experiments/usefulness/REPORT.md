# Usefulness experiment report

Tracker: LIM-1457 / GitHub issue #70. Protocol: [`PROTOCOL.md`](PROTOCOL.md).

| Item | Value |
|---|---|
| Pre-registration commit | `4c9bfc8` (protocol, fixture, workload, expectations, configs) |
| Measured commit | `a3247e5fbd759e54cbffa7e88db5eeb6ba8d2ce1` |
| Results | [`results/a3247e5fbd759e54cbffa7e88db5eeb6ba8d2ce1/`](results/a3247e5fbd759e54cbffa7e88db5eeb6ba8d2ce1/): `identity.json`, `summary.json`, `summary.md`, raw `*.jsonl.gz`, `steps.log`, per-step command logs under `logs/`, and the three daemon logs |
| Decision (pre-registered rule, §6) | **Supports further development** of the runtime path |

The results commit is the child of the measured commit; `run.sh` refused to
run on a dirty tree, so every raw file was produced by the code at the
measured SHA.

Run history:

- `b4bdbb1`: first attempt, failed in the transport driver because the
  sandbox image has no `kill` binary and daemon stdout was not captured.
  Its `identity.json`, `steps.log` and command logs are retained under
  `results/b4bdbb1…/`.
- `40cfd05`: first complete run; all checks passed with the same decision.
  Superseded (and removed from the tree) after code-health review led to
  restructuring the drivers and tooling; its files remain in git history.
- `a3247e5`: the measured run reported here, on the restructured drivers.

## Setup (measured run)

- Hardware: AMD EPYC 9354P, 64 logical CPUs, 791,897,740 kB RAM, shared
  sandbox (no CPU pinning, no isolation from other tenants).
- OS: Debian GNU/Linux 12, kernel 6.8.0-139-generic.
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1`,
  `--locked`, release profile, features `corpus-ipc` (published `corpus-ipc`
  0.1, `neuromod` 0.6.0, `zmq` 0.10, system `libzmq3-dev`).
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
| r1 | 10.3 ms | 1 | 200 / 200 / 0 | 4,253 (4,053) | 1.110 ms | 2.184 ms | 3.217 ms | 6,964 kB | 5.91 ms |
| r2 | 10.2 ms | 1 | 200 / 200 / 0 | 4,250 (4,050) | 1.109 ms | 2.577 ms | 4.137 ms | 7,008 kB | 8.55 ms |
| r3 | 10.2 ms | 1 | 200 / 200 / 0 | 4,245 (4,045) | 1.108 ms | 1.857 ms | 2.833 ms | 6,988 kB | 5.90 ms |

Each measured pulse produced exactly one nonempty `Spikes` frame carrying the
expected channel set; every idle tick produced an empty frame; `/metrics`
reported `brainstem_ready 1` and `brainstem_fatal 0`; exit status 0 with
`Termination signal received` in each committed daemon log.

**Performance, in-process** (per-tick latency after warm-up; see
`summary.md` for every run).

| mode | p50 | p99 | max | ticks/s | peak RSS |
|---|---:|---:|---:|---:|---:|
| baseline (step only), 5 runs | 0.97–0.98 µs | 1.13–1.77 µs | 12–59 µs | 882k–944k | 4.3–4.5 MB |
| daemon-live, 5 runs | 1.07–1.84 µs | 3.12–4.51 µs | 49–180 µs | 527k–798k | 5.6–5.7 MB |
| daemon-simulation, 5 runs | 0.60–0.86 µs | 2.11–2.67 µs | 14–59 µs | 1,031k–1,319k | 5.1–5.7 MB |

Typed decode (`accept_ipc_json`) per stimulus frame, daemon runs: p50
1.1–1.6 µs (counted separately from tick latency).

Derived: daemon-live p99 ≤ 1 ms in every run (budget met, margin > 200×);
no single tick exceeded 1 ms in this run. Transport p99 ≤ 5 ms with zero
loss in every run (budget met; the largest single pulse took 4.1 ms).

## What works, what fails, limitations

Works:

- The supported live path (`DaemonConfig::load` → `try_with_backend` →
  `run_for_ticks`) restores a nonblank sidecar, decodes typed `corpus-ipc`
  frames with masks and modulators, and produces exactly the spikes an
  independent model predicts, with correct accounting and health.
- The daemon adds roughly 0.1–0.9 µs median and 1.5–2.7 µs p99 over the
  bare engine step for validation, bounded ingress, health and accounting,
  plus 1–2 µs decode per frame; memory overhead is ~1.2 MB in-process and
  ~2.6 MB for the full binary with ZeroMQ.
- The binary is ready in ~10 ms, delivers every pulse over loopback ZeroMQ
  with ~1.1 ms median end-to-end (one tick period of quantization), and
  exits cleanly 6–9 ms after SIGTERM.
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
  (~25 ms of measured time each). The superseded `40cfd05` run saw two
  single-tick outliers above 1 ms (3.0 ms in-process, 7.2 ms transport);
  this run saw none, consistent with scheduler noise on a shared host rather
  than a property of the daemon. The p99 criteria were met in both runs.
- Release qualification and publication stay in LIM-1447; this is not a
  release gate.

## Protocol deviations

None to the checks, metrics, or criteria. Post-registration code changes
were limited to the drivers and tooling: the transport driver fix after the
`b4bdbb1` failure (`sh -c kill`, stdout capture), and the restructuring
into smaller functions after code-health review, which regenerates the
fixtures byte-for-byte and reproduces the previous run's `summary.md`
exactly from its raw data.

## Recommendation

The evidence supports continuing development of the runtime path: the
daemon is correct, deterministic on this fixture, and its overhead over the
bare engine is microseconds against a 1 ms tick budget. The open gap is the
model side, not the runtime: a compatible trained artifact without
`output_weights` (or loader support for a readout matrix) is needed before
any usefulness claim beyond runtime plumbing can be made.
