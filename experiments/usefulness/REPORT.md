# Usefulness experiment report

Tracker: LIM-1457 / GitHub issue #70. Protocol: [`PROTOCOL.md`](PROTOCOL.md).

| Item | Value |
|---|---|
| Pre-registration commit | `4c9bfc8` (protocol, fixture, workload, expectations, configs) |
| Measured commit | `40cfd056c08abc86d0e29588e30d8fed119f52ea` |
| Results | [`results/40cfd056c08abc86d0e29588e30d8fed119f52ea/`](results/40cfd056c08abc86d0e29588e30d8fed119f52ea/) (`identity.json`, `summary.json`, `summary.md`, raw `*.jsonl.gz`, daemon logs, per-step command logs) |
| Decision (pre-registered rule, §6) | **Supports further development** of the runtime path |

The results commit is the child of the measured commit; `run.sh` refused to
run on a dirty tree, so every raw file was produced by the code at the
measured SHA. A first run at `b4bdbb1` failed in the transport driver (the
image has no `kill` binary; daemon stdout was not captured). Its identity and
command logs are retained under `results/b4bdbb1…/`; the driver fix is the
only change between the two SHAs and touches no decision criterion.

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
  modulator-only packets per run, 1,000 warm-up ticks excluded from latency.
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
| r1 | 10.3 ms | 1 | 200 / 200 / 0 | 4,240 (4,040) | 1.111 ms | 3.332 ms | 4.281 ms | 7,052 kB | 6.09 ms |
| r2 | 12.1 ms | 1 | 200 / 200 / 0 | — | 1.109 ms | 3.459 ms | 4.233 ms | 7,036 kB | 6.23 ms |
| r3 | 10.5 ms | 1 | 200 / 200 / 0 | — | 1.111 ms | 4.133 ms | 7.223 ms | 6,968 kB | 6.11 ms |

Each measured pulse produced exactly one nonempty `Spikes` frame carrying the
expected channel set; every idle tick produced an empty frame; `/metrics`
reported `brainstem_ready 1` and `brainstem_fatal 0`; exit status 0 with
`Termination signal received` logged.

**Performance, in-process** (per-tick latency after warm-up; see
`summary.md` for every run).

| mode | p50 | p99 | max | ticks/s | peak RSS |
|---|---:|---:|---:|---:|---:|
| baseline (step only), 5 runs | 0.95–1.69 µs | 1.76–2.00 µs | 34 µs – 792 µs | 638k–912k | 4.4–4.5 MB |
| daemon-live, 5 runs | 1.07–1.91 µs | 3.23–4.84 µs | 68 µs – 3.02 ms | 477k–656k | 5.2–5.7 MB |
| daemon-simulation, 5 runs | 0.60–1.09 µs | 2.45–3.53 µs | 20 µs – 547 µs | 747k–1,212k | 5.1–5.6 MB |

Typed decode (`accept_ipc_json`) per stimulus frame, daemon runs: p50
1.1–1.9 µs (counted separately from tick latency).

Derived: daemon-live p99 ≤ 1 ms in every run (budget met, margin > 200×).
Transport p99 ≤ 5 ms with zero loss in every run (budget met; r3 at
4.13 ms is the closest). Two single-tick outliers exceeded 1 ms (daemon-live
r1 max 3.02 ms, transport r3 max 7.22 ms); the criterion is p99, so they do
not change the decision, but they are the first thing to look at under a
real-time deployment on a shared host.

## What works, what fails, limitations

Works:

- The supported live path (`DaemonConfig::load` → `try_with_backend` →
  `run_for_ticks`) restores a nonblank sidecar, decodes typed `corpus-ipc`
  frames with masks and modulators, and produces exactly the spikes an
  independent model predicts, with correct accounting and health.
- The daemon adds roughly 0.1–0.4 µs median and ~2–3 µs p99 over the bare
  engine step for validation, bounded ingress, health and accounting, plus
  ~1–2 µs decode per frame; memory overhead is ~1.2 MB in-process and
  ~2.6 MB for the full binary with ZeroMQ.
- The binary is ready in ~10 ms, delivers every pulse over loopback ZeroMQ
  with ~1.1 ms median end-to-end (one tick period of quantization), and
  exits cleanly ~6 ms after SIGTERM.
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
  (~30 ms of measured time each); outliers above are consistent with
  scheduler noise, not attributed further.
- Release qualification and publication stay in LIM-1447; this is not a
  release gate.

## Protocol deviations

None to the checks, metrics, or criteria. The only post-registration code
change was the transport driver fix described above (`sh -c kill`, stdout
capture), made before any transport result existed.

## Recommendation

The evidence supports continuing development of the runtime path: the
daemon is correct, deterministic on this fixture, and its overhead over the
bare engine is microseconds against a 1 ms tick budget. The open gap is the
model side, not the runtime: a compatible trained artifact without
`output_weights` (or loader support for a readout matrix) is needed before
any usefulness claim beyond runtime plumbing can be made.
