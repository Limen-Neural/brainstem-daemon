# Usefulness experiment: pre-registered protocol

Tracking: GitHub #70 / Linear LIM-1457. This protocol is committed **before**
any measurement is recorded. The commit that adds this file is the
pre-registration commit. Results, decisions, and any protocol deviations are
in [`REPORT.md`](REPORT.md). Criteria in this file must not change after
results exist. A required change is recorded as a deviation in the report.

Release qualification (LIM-1447) is separate and is not affected by this
experiment.

## Question

Does `brainstem-daemon` give correct and useful runtime behavior on a
representative input → tick → output workload? Successful compilation does
not count as evidence.

## Practical behavior under test

A typed `corpus-ipc` stimulus stream drives a restored, nonblank LIF network.
The stream uses `IpcMessage::Stimuli` with schema `corpus-ipc.stimulus.v1`,
per-channel `valid_mask`, and `IpcMessage::Neuromodulators` frames. On every
tick the daemon must:

1. decode and validate the frame,
2. apply the mask,
3. step the network,
4. emit the expected spike channels within the 1 kHz tick budget,
5. account for every batch, and
6. shut down cleanly.

The daemon does **not** add model capability. Its claimed value is runtime
plumbing around `neuromod::SpikingNetwork`:

- typed ingress validation and masking,
- bounded classified queues,
- health and metrics,
- ZeroMQ transport, and
- signal lifecycle.

The experiment tests whether that plumbing preserves the engine's output
exactly, and what it costs.

## Modes (kept distinct)

| Mode | How it is built | Role in this experiment |
|---|---|---|
| **Live** | `runtime_mode = "live"`; restores `fixtures/snn_model.json` through the daemon's own loader | Primary result |
| **Simulation** | `runtime_mode = "simulation"`; blank `SpikingNetwork::with_dimensions(16, 0, 8)` | Negative control: no spikes expected |
| **Stub** | default features, `BrainstemDaemon::new()` / `try_new()`, `BackendPair::stub()` | Documented only. No input path, no-op output. **Not used as evidence** |
| **`RuntimeHarness` / `FakeCore`** | `runtime::harness` test double | Out of scope. **Not used as evidence** |

"Live" means a validated sidecar restored by the daemon. The checkpoint is
still synthetic (see Fixture), so the experiment cannot show trained-model
usefulness.

## Fixture

`fixtures/snn_model.json` is version `usefulness-fixture-v1`. See
`fixtures/fixture-manifest.json` for its SHA-256 and the full mapping.

- **Hand-designed, synthetic, and untrained.** It is not a trained model.
- Source `spikenaut_julia`, LIF only (16 neurons × 8 channels), no
  `output_weights` key, decay 0.15, threshold 0.30.
- Neuron `n` has weight 2.0 on channel `floor(n/2)` and 0 elsewhere.
  Channel `c` therefore drives neurons `{2c, 2c+1}`.
- Each row sums to 2.0, the `neuromod` 0.6 L1 budget, so per-step
  renormalization does not change the weights.

The published Hugging Face sidecar is not used, for two reasons:

- It carries `output_weights`, which live startup rejects.
- Fetching it would leave the repository-local scope.

A compatible trained artifact does not exist, so trained-model usefulness
**remains unproven** whatever this experiment shows.

Before pre-registration, the fixture was confirmed to pass the live validation
in `src/checkpoint/mod.rs`. A design-time float64 mirror of the `neuromod` 0.6
LIF equations was also run to choose segment lengths. It is not a measurement
and is not committed as evidence.

## Workload

Defined in `fixtures/workload.json` (`usefulness-workload-v1`). One cycle is
200 ticks:

| Segment | Ticks | Input |
|---|---|---|
| `single` | 10 | ch0 = 1.0 |
| `idle` | 40 | zeros |
| `pair` | 10 | ch3 = 1.0, ch5 = 0.6 |
| `idle` | 40 | zeros |
| `masked` | 10 | ch1 = 1.0, ch2 = 1.0 with `valid_mask[2] = false` (masked placeholder) |
| `idle` | 40 | zeros |
| `modulated` | 10 | ch6 = 1.0 plus a `Neuromodulators` frame `{dopamine 0, cortisol 0, acetylcholine 0.5, tempo 1.0}` |
| `idle` | 40 | zeros |

- **Size:** 50 cycles, N = 10,000 ticks per process. A stimulus frame is sent
  on every tick, idle ticks included.
- **Warm-up:** the first cycle (200 ticks) is excluded from latency and
  throughput statistics. All ticks are included in correctness checks.
- **Repetitions:** R = 5 separate processes per in-process mode/variant, and
  R = 3 separate processes for the transport run.
- **Variants** (in-process, live, R each):
  - `main` is the workload as listed.
  - `zeroed` gives every masked slot value 0.0 and mask `true`.
  - `unmasked` drops `valid_mask`, so the ch2 placeholder 1.0 is applied.

### Transport workload

The binary ticks at 1 kHz with its own clock. The producer is not
tick-synchronized with it, so per-tick raster equality is not testable over
ZeroMQ. The transport run instead sends isolated probes:

- **Probes:** 200, one every 50 ms (about 10 s). Probe `p` is a single frame
  with channel `p mod 8` = 1.0.
- **Priming:** before measurement, ch0 = 1.0 frames are sent every 50 ms until
  a nonempty `{0,1}` batch arrives (5 s timeout). The driver then waits 200 ms
  to settle. Priming frames and outputs are excluded from measurement.

## Baseline

The baseline is a direct-engine loop in the same process type. It uses:

- the same `neuromod` `SpikingNetwork`,
- the same restored fixture, from the public `brainstem_daemon::restore_network`
  with the same config, so it shares one loader and gets `reward_lr = 0.0`,
- the same pre-decoded stimuli.

It applies the documented daemon decoding by hand: mask → 0, width pad or
truncate, and positional modulator mapping from dopamine/cortisol/acetylcholine/tempo
to DA/5-HT/ACh/NE (missing modulators use defaults). It then calls
`network.step` directly. It has no JSON decode, queues, health, transport, or
lifecycle.

The two are comparable because they have the same output semantics: spike IDs
are converted to `u16` channels. Latency and throughput differences are the
daemon's runtime overhead. The daemon is **not assumed** to beat the baseline
on any metric.

## Pre-registered correctness checks

In-process. All repetitions must pass:

- **C1 accounting.** `RuntimeStats.ticks == N`.
  `accepted_batches == N` (one typed batch per tick).
  `rejected_batches == receive_errors == emit_errors == dropped_spikes == 0`.
  The sink observed N emitted batches.
- **C2 ingress.** Sensory accepted `== N`. Reward accepted `==` number of
  modulated ticks (500). `lost_or_coalesced == 0` for every class.
- **C3 health.** The post-run snapshot is not `fatal` and has no fatal payload.
- **C4 baseline equality.** For every repetition, the per-tick channel sets of
  daemon-live `main` equal those of baseline `main`.
- **C5 determinism.** All repetitions of a given mode/variant produce
  identical rasters.
- **C6 drive.** On every tick of `single`, `pair`, and `masked`, the spike set
  equals exactly the mapped neurons of channels with effective input ≥ 0.5.
  Effective input means after masking.
- **C7 selectivity.** Every spike belongs to a channel with nonzero effective
  input within the current or previous 30 ticks.
- **C8 quiet.** A tick at least 30 ticks after the last nonzero effective input
  on any channel has no spikes.
- **C9 mask equivalence.** The `zeroed` raster equals the `main` raster.
- **C10 mask sensitivity.** The `unmasked` raster differs from `main`, and
  neurons {4,5} fire during `masked`.
- **C11 modulator effect.** In `modulated`, neurons {12,13} fire on fewer than
  10 of 10 ticks in every cycle. An unmodulated driven segment fires 10 of 10.
- **C12 simulation control.** Daemon-simulation emits zero spikes over the
  full workload. C1–C3 must also hold for it.

Transport. All repetitions must pass:

- **T1 lifecycle.** The daemon is ready on `/readyz`. After SIGTERM it exits
  with status 0 within 2 s and logs `Termination signal received`.
- **T2 probe correctness.** Every probe gets exactly one matching nonempty
  batch equal to `{2c, 2c+1}`. During measurement no nonempty batch has
  other content.
- **T3 latency.** p99 end-to-end probe latency (driver send → driver receive
  of the matching batch) is ≤ 3 ms. That is one 1 ms tick period plus 2 ms of
  loopback/scheduling slack.

## Pre-registered performance metrics

All are measured. Derived values are labeled as derived.

- In-process per-tick latency: source poll (`Instant` at `next_ingress`) →
  sink emit (`Instant` at `emit`). For the daemon this includes JSON
  decode/validation, queue admit/drain, health, and `step`. For the baseline it
  is `step` plus spike conversion. Reported as p50, p99, and max after warm-up.
- In-process throughput: post-warm-up ticks / wall time of the tight loop
  (`run_for_ticks` for the daemon).
- Peak RSS: `VmHWM` from `/proc/self/status` (in-process) and
  `/proc/<pid>/status` before SIGTERM (transport daemon).
- Transport:
  - probe end-to-end latency (p50, p99, max),
  - output batches received during measurement versus the expected tick count
    (elapsed ms),
  - gaps between consecutive batch IDs (`batch_id` = tick wall-clock ms),
  - the `/metrics` text, captured raw.
- **Derived:** daemon − baseline latency overhead, and comparisons against
  the 1 ms tick budget.

## Decision criteria

- **Supports further development.** All of C1–C12 and T1–T3 pass, and:
  - **P1:** daemon-live in-process p99 latency ≤ 1 ms (the 1 kHz budget) in
    every repetition;
  - **P2:** daemon-live in-process throughput ≥ 1,000 ticks/s in every
    repetition.
- **Change of direction or blocked.** Any of these:
  - daemon and baseline outputs diverge for a cause that is not explained,
  - any correctness check fails, or
  - P1, P2, or T3 is missed.

  The report must attribute the cause.

In every outcome, the report states that trained-model usefulness stays
unproven until a compatible artifact without `output_weights` exists.

### Known divergence sources and how they are handled

- **Thread-local RNG.** `step` uses an RNG that is not checkpointed. It only
  affects `input_spike_times` (the Bernoulli input encoding). Those feed
  R-STDP eligibility, and with `reward_lr = 0.0` the weight update is zero.
  The RNG is **expected not** to affect spike output. C4 and C5 test this.
- **State-changing engine contracts.** These are:
  - acetylcholine-driven `decay_rate` retuning,
  - dopamine-gated threshold blending (gate 0 when dopamine is 0), and
  - L1 renormalization (a no-op for this fixture).

  Daemon and baseline run the same engine, so these contracts cannot cause a
  divergence by themselves.
- **Wall-clock fields.** Output `time`, `batch_id`, and `timestamp` come from
  the wall clock. They are excluded from all equality checks.
- **Divergence rule.** If C4 or C5 fails, the cause must be attributed before
  the run can count as a pass.

## Reproduction from a fresh checkout

Prerequisites (Linux only, because `/proc` is used for RSS):

- The packages that `.agents/setup` installs: `build-essential`, `curl`,
  `pkg-config`, `libzmq3-dev`.
- Rust **1.98.1** (`rust-toolchain.toml`).
- `cargo fetch --locked`. This needs network access to crates.io. No model
  artifacts are fetched.
- `python3`, standard library only.
- Free loopback ports 15565–15567.

Then:

```bash
bash .agents/setup          # or install the prerequisites above
experiments/usefulness/run.sh
```

`run.sh` does the following:

1. Refuses a dirty working tree.
2. Records identity: the SHA, tool versions, OS, CPU, memory, and the
   SHA-256 of the configs and fixtures.
3. Builds the release examples and the binary with
   `--locked --features corpus-ipc`.
4. Runs every mode/variant as separate processes.
5. Writes raw JSONL, per-step logs, `identity.json`, `summary.json`, and
   `summary.md` to `experiments/usefulness/results/<sha>/`.

It keeps partial results and records an explicit final status on failure.

Drivers load the TOML configs with `DaemonConfig::load` and construct the
daemon only through fallible constructors (`try_with_backend`). Raw serde
deserialization does not validate.
