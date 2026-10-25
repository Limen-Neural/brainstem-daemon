# Secondary usefulness experiment: report

Tracking: GitHub #70 / Linear LIM-1457. Protocol: [`PROTOCOL.md`](PROTOCOL.md).

**Secondary experiment.** The primary #70 experiment and its report are in
[`experiments/usefulness/`](../usefulness/) (#94). This independent
replication uses its own fixture, workload, drivers, and summarizer. It is kept
as corroborating evidence and does not replace the primary results. Both reach
the same decision. Release qualification (LIM-1447) is separate.

## Identity

| Item | Value |
|---|---|
| Pre-registration commit (protocol, fixture, workload, configs) | `0f15197` |
| Measured commit (drivers + runner + summarizer; clean tree) | `023377370f393121f23aa24355a038737ce5a163` |
| Results directory | [`results/023377370f393121f23aa24355a038737ce5a163/`](results/023377370f393121f23aa24355a038737ce5a163/) |
| Recorded at | 2026-10-25T20:58:35Z |

The measured commit is the **parent** of the commit that adds the results and
this report. `run.sh` refuses a dirty tree, so the results were produced from
exactly `0233773`.

## Environment (from `identity.json`)

| Item | Value |
|---|---|
| Host | Debian GNU/Linux 13 (trixie), kernel Linux 6.1.158+ x86_64, shared cloud VM |
| CPU / memory | Intel Xeon @ 2.60 GHz, 8 logical CPUs; 16 GB RAM |
| Toolchain | rustc 1.99.0, cargo 1.99.0 (`rust-toolchain.toml`), `neuromod` 0.7.0, libzmq 4.3.5, Python 3.11 |
| Build | `--release --locked --features corpus-ipc` (thin LTO, opt-level 3) |
| Configuration | `config/live.toml`, `config/simulation.toml`, `config/transport-live.toml` (16 LIF × 8 channels, 1 kHz; loopback ports 15565–15567, distinct from the primary experiment) |
| Fixture | `usefulness-fixture-v1`, SHA-256 `2254bfc8…070b1f`; synthetic, **untrained** |
| Workload | `usefulness-workload-v1`; 10,000 ticks/run (50 × 200-tick cycles); first 200 ticks excluded from latency/throughput |
| Repetitions | 5 separate processes for each in-process mode/variant (25 runs), 3 transport processes |

## Method

How each quantity was measured:

- **In-process latency.** A monotonic `Instant` is taken at
  `StimulusSource::next_ingress` entry, after the producer has encoded the
  frame. A second is taken at `SpikeSink::emit`.
- **Throughput.** Post-warm-up ticks divided by the span from the first
  post-warm-up poll to the last emit. Ticks run back to back
  (`run_for_ticks`, not paced).
- **Memory.** `VmHWM` from `/proc`.
- **Transport latency.** Driver PUB send time to driver SUB receive time of
  the matching spike batch, on loopback, with the daemon ticking at 1 kHz on
  its own clock.
- **Statistics.** Percentiles use the nearest-rank method.
- **Summary.** `summarize.py` (stdlib only) computes everything from raw
  JSONL. Values labeled "derived" are arithmetic on measured values. All
  others are measured.
- **Completeness.** Any check that no run evaluated fails, and the I1 gate
  fails unless every pre-registered run is present.

## Results against the pre-registered checks

Mechanical decision from `summary.md`: **supports further development**. All
18 gates pass.

| Check | Result | Evidence |
|---|---|---|
| I1 run inventory | PASS | 5 reps for each of the 5 in-process families, and 3 transport reps |
| C1 accounting | PASS | All 20 daemon runs: `ticks == accepted_batches == 10,000`; 0 rejected, 0 receive/emit errors, 0 dropped spikes |
| C2 ingress | PASS | Sensory accepted 10,000; reward accepted 500 (= modulated ticks); 0 lost or coalesced in any class |
| C3 health | PASS | Post-run phase `running`, `fatal = null`, checkpoint digest equals the fixture SHA-256 |
| C4 daemon == baseline | PASS | Per-tick channel sets identical in 5/5 repetitions (10,000 ticks each) |
| C5 determinism | PASS | All repetitions of each mode/variant are identical, despite the thread-local RNG that is not checkpointed |
| C6 drive | PASS | Every `single`/`pair`/`masked` tick fires exactly the mapped neurons |
| C7 selectivity | PASS | No spike from a channel that had no input in the previous 30 ticks |
| C8 quiet | PASS | No spikes ≥ 30 ticks after the last input |
| C9 mask equivalence | PASS | The `zeroed` raster equals the `main` raster |
| C10 mask sensitivity | PASS | The `unmasked` raster differs, and neurons {4,5} fire when the placeholder is applied |
| C11 modulator effect | PASS | `modulated` fired {12,13} on 2/10 ticks in every cycle; unmodulated driven segments fire 10/10 |
| C12 simulation control | PASS | 0 spikes in 5/5 simulation runs; C1–C3 also hold |
| T1 lifecycle | PASS | Ready after about 10 ms; SIGTERM → exit 0 in 2.9–4.1 ms with `Termination signal received` |
| T2 probes | PASS | 600/600 probes matched exactly `{2c, 2c+1}`; 0 unexpected nonempty batches |
| T3 transport p99 ≤ 3 ms | PASS | p99 1.68–1.71 ms per rep |
| P1 in-process p99 ≤ 1 ms | PASS | Daemon-live p99 3.6–5.3 µs |
| P2 ≥ 1,000 ticks/s | PASS | Daemon-live 265,492–337,762 ticks/s |

**`neuromod` 0.7.0 behaves like 0.6.0 on this workload.** All 25 in-process
spike rasters of this run are identical, file by file, to those of the earlier
`neuromod` 0.6.0 / Rust 1.98.1 run of the same drivers. That run is described
under disclosures below. This matches a source comparison: the 0.7.0
normal-mode `step` keeps the 0.6.0 equations and constants and adds input-shape
validation and a separate `Frozen` mode.

### Performance (measured unless marked derived)

| | Daemon-live (15 runs) | Baseline (5 runs) | Daemon-simulation (5 runs) |
|---|---|---|---|
| Per-tick latency p50 | 2.2–2.9 µs | 0.7–0.9 µs | 1.9–2.2 µs |
| Per-tick latency p99 | 3.6–5.3 µs | 1.1–1.5 µs | 3.5–3.6 µs |
| Per-tick latency max | 40–263 µs | 28–59 µs | 37–66 µs |
| Throughput | 265,492–337,762 ticks/s | 1,026,323–1,206,136 ticks/s | 337,696–366,408 ticks/s |
| Peak RSS (VmHWM) | 7.8–8.0 MB | 8.6–8.7 MB | 7.7–7.8 MB |

- **Derived overhead** (daemon-live main − baseline main, matching reps):
  - Δp50 is 1.4–2.0 µs.
  - Δp99 is 2.7–3.5 µs.
  - The daemon reaches 26–30 % of the baseline's tight-loop throughput.

  The overhead covers typed JSON decode and validation, queue admit/drain,
  health updates, and spike conversion. Encoding the frame is producer work and
  is excluded.
- **Derived budget headroom.** The worst daemon p99 (5.3 µs) is about 0.5 % of
  the 1 ms tick budget. The worst single tick (263 µs) is about 26 % of the
  budget.
- **RSS caveat.** Baseline RSS is higher because the baseline driver
  pre-decodes all 10,000 inputs before the timed loop. RSS here measures each
  driver process, not the engine alone.

Transport, measured, 3 reps × 200 probes:

| | Value |
|---|---|
| Probe latency, pooled (n = 600) | p50 0.93 ms, p99 1.69 ms, max 4.09 ms |
| Output batches per expected tick (derived) | 1.000 (10,004–10,006 batches per 10,005 ms window) |
| Daemon peak RSS | 6.6 MB |
| Shutdown | exit 0 in 2.9–4.1 ms after SIGTERM |

The probe-latency spread is consistent with a uniform wait of 0–1 ms for the
next tick plus roughly 0.4–0.6 ms of ZeroMQ, scheduling, and JSON overhead.
That split is an **estimate**: the experiment did not measure the
decomposition. One probe (r2) exceeded 3 ms. T3 is a p99 criterion, so it is
reported here rather than counted as a failure.

About 11–14 % of consecutive `batch_id` steps are 0 ms and a matching share are
2 ms. That is aliasing: `batch_id` is the wall-clock millisecond and ticks
jitter around millisecond boundaries. It is not loss. The batch count matched
the elapsed ticks in every rep.

## What works

- **Correct, deterministic plumbing.** The daemon preserves the engine's
  output exactly: 10,000 ticks × 5 reps match the baseline. The typed
  ingress, mask handling, modulator mapping, bounded queues, and accounting
  add no observable distortion on this workload.
- **The mask contract holds.** A masked placeholder of 1.0 behaves exactly
  like a real 0.0. Without the mask the placeholder does drive the network,
  so the check is not vacuous.
- **Restore is fail-closed and traceable.** Live mode restores a validated,
  versioned checkpoint, and its SHA-256 appears in health and logs. The
  simulation control stays silent.
- **Real binary over ZeroMQ.**
  - Start-up to ready takes about 10 ms.
  - Every probe gets the correct, selective response, with p99 1.7 ms at 1 kHz.
  - SIGTERM gives a clean exit in under 5 ms.
  - Peak RSS is about 6.6 MB.
- **Ample headroom.** At this network size, daemon overhead is a few µs per
  tick, far inside a 1 ms budget.

## What fails or is weak

No pre-registered check failed. The experiment did surface these weaknesses:

- **Modulator semantics are surprising.** `corpus-ipc` `tempo` has a valid
  range of [0.5, 2.0], with 1.0 meaning nominal. The positional mapping sends
  it to neuromod `norepinephrine`, and the engine computes
  `stress_multiplier = max(1 − NE, 0.1)`. So any valid tempo cuts input drive
  to between 10 % and 50 %, and **nominal tempo is the strongest
  suppression** (10 %). C11 confirms this: driven firing drops from 10/10 to
  2/10 ticks. The behavior is deterministic and documented, so it is not a
  daemon bug. It is probably not what a producer sending "nominal tempo"
  intends, and it should be reviewed before real use.
- **Modulators are not held in-process.** On the in-process path, a tick
  without a modulator frame uses default (zero) modulators. The ZMQ source
  instead holds the last modulators. The experiment fed modulators on every
  modulated tick, so it did not exercise this difference.
- **No input/output correlation.** Output batches do not carry the input
  `batch_id` or `session_id`, so transport matching relies on order and time.
- **No delivery acknowledgment.** PUB sends are best-effort. Subscriber-side
  loss cannot be observed by the daemon.

## Limitations

- **Synthetic, untrained fixture.** The fixture is hand-designed so that
  expected spikes can be derived. It shows runtime correctness, not model
  usefulness. **Trained-model usefulness remains unproven** until a compatible
  artifact without `output_weights` exists. The published Hugging Face
  sidecar carries `output_weights`, which live startup rejects. It was not
  fetched, which keeps the experiment repository-local.
- **RNG and engine state.** The thread-local RNG used by `step` is not
  checkpointed. With `reward_lr = 0` it cannot change spike output, which C4
  and C5 confirm empirically for this workload. A nonzero `reward_lr` would
  change that. Engine state also changes during stepping (ACh retunes decay,
  thresholds blend when dopamine > 0, L1 renormalization), and it is identical
  in daemon and baseline.
- **Small network.** 16 LIF × 8 channels. Overhead and latency at production
  sizes are not measured.
- **Shared environment.** A single shared cloud VM ran the experiment with no
  CPU pinning or isolation. Tail latencies (max) reflect that noise.
- **Order-based matching.** Transport matching is by order and time only.
- **Linux only.** RSS comes from `/proc`.
- **Scope.** Not tested: rejection paths (stale, schema, or non-finite frames)
  under load, overload/backpressure, and long-duration stability.
- **Release qualification** (LIM-1447) is separate and is not established by
  this report.

## Protocol deviations and disclosures

- **No changes to criteria, workload, fixture, or configuration values** after
  the pre-registration commit `0f15197`.
- **Earlier runs of this experiment.** The same protocol, fixture, workload,
  and drivers were developed in PR #96 at `experiments/usefulness/`, before
  #94 put the primary experiment at that path.
  - They were measured twice on `neuromod` 0.6.0 / Rust 1.98.1, at `ecfeb9d`
    and `3fe3f42`. Both runs passed every check with the same decision.
  - Those results named commits that were later rewritten.
  - When #94 merged, this experiment was moved to
    `experiments/usefulness-secondary/` with `usefulness_secondary_*` example
    names, at the maintainer's request, so it does not collide with the
    primary experiment.
- **Pre-registration content** changed only in these respects, all before this
  measurement:
  - paths;
  - the "secondary" framing;
  - the Rust 1.99.0 / `neuromod` 0.7 references;
  - an explicit pre-registered I1 inventory gate.

  The I1 gate was previously enforced only by the summarizer.

  The fixture files are byte-identical to the earlier pre-registration.
- **Development smoke runs.** Driver development included smoke runs of the
  unfinished drivers, written to scratch directories. They confirmed that the
  drivers ran and that the summarizer detects failures: an injected one-tick
  baseline corruption fails C4, and deleting run families fails I1 and the
  affected checks. Those outputs were discarded. They are not part of the
  results.
- **Results packaging.**
  - Raw per-tick JSONL files are committed gzip-compressed (`*.jsonl.gz`,
    `gzip -9 -n`). `summarize.py` reads plain JSONL.
  - To re-derive the summary, copy the results directory, run
    `gunzip -k raw/*.jsonl.gz`, and run `summarize.py` on the copy.
    `measured_commit` is taken from the directory name.
  - Daemon logs are stored as `*.daemon.txt`, because `.gitignore` excludes
    `*.log`.

## Decision

**The evidence supports further development of `brainstem-daemon` as runtime
plumbing.** This corroborates the primary experiment's decision.

- It is correct and deterministic relative to the direct engine.
- Its validation, masking, accounting, health, transport, and lifecycle work
  as documented.
- Its overhead is small against a 1 kHz budget.

The evidence does **not** support any claim of model or trained-network
usefulness. The next things to settle are:

1. A compatible trained artifact, or a decision on readout weights
   (`output_weights`).
2. A review of the `tempo` → norepinephrine positional mapping.
3. Input/output correlation IDs, if downstream consumers need per-input
   attribution.

## Reproduce

```bash
bash .agents/setup                          # packages, Rust 1.99.0, cargo fetch --locked
experiments/usefulness-secondary/run.sh     # clean tree required; writes results/<HEAD sha>/
```
