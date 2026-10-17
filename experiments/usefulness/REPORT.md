# Usefulness experiment: report

Tracking: GitHub #70 / Linear LIM-1457. Protocol: [`PROTOCOL.md`](PROTOCOL.md).
Release qualification (LIM-1447) is separate and is not affected by this report.

## Identity

| Item | Value |
|---|---|
| Pre-registration commit (protocol, fixture, workload, configs) | `ff6f300e9dec1d9224dbdfa80fb1f9874f6edcbe` |
| Measured commit (drivers + runner + summarizer; clean tree) | `3fe3f42ef5f1182ae8a4574068f47e98a3277ecb` |
| Results directory | [`results/3fe3f42ef5f1182ae8a4574068f47e98a3277ecb/`](results/3fe3f42ef5f1182ae8a4574068f47e98a3277ecb/) |
| Recorded at | 2026-10-17T04:11:52Z |

The measured commit is the **parent** of the commit that adds the results and
this report. `run.sh` refuses a dirty tree, so the results were produced from
exactly `3fe3f42`. The protocol file hash that `identity.json` records for the
measured run matches the pre-registered `PROTOCOL.md` (`392bbbcd…`).

The branch history was rewritten once, at the maintainer's request to change
commit authorship. See [Protocol deviations and disclosures](#protocol-deviations-and-disclosures):
the pre-registration content is byte-identical to the original pre-registration
commit, and the original measurement is superseded by this one.

## Environment (from `identity.json`)

| Item | Value |
|---|---|
| Host | Debian GNU/Linux 13 (trixie), kernel Linux 6.1.158+ x86_64, shared cloud VM |
| CPU / memory | Intel Xeon @ 2.60 GHz, 8 logical CPUs; 16 GB RAM |
| Toolchain | rustc 1.98.1, cargo 1.98.1 (`rust-toolchain.toml`), libzmq 4.3.5, Python 3.11 |
| Build | `--release --locked --features corpus-ipc` (thin LTO, opt-level 3) |
| Configuration | `config/live.toml`, `config/simulation.toml`, `config/transport-live.toml` (16 LIF × 8 channels, 1 kHz) |
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
- **Completeness.** The summarizer fails any check that no run evaluated, and
  gate I1 fails unless every pre-registered run is present. Missing evidence
  cannot count as a pass.

## Results against the pre-registered checks

Mechanical decision from `summary.md`: **supports further development**. All
18 gates pass: I1 plus the 17 pre-registered checks.

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
| T1 lifecycle | PASS | Ready after about 10 ms; SIGTERM → exit 0 in 3.1–3.8 ms with `Termination signal received` |
| T2 probes | PASS | 600/600 probes matched exactly `{2c, 2c+1}`; 0 unexpected nonempty batches |
| T3 transport p99 ≤ 3 ms | PASS | p99 1.63–1.78 ms per rep |
| P1 in-process p99 ≤ 1 ms | PASS | Daemon-live p99 4.8–16.8 µs |
| P2 ≥ 1,000 ticks/s | PASS | Daemon-live 181,172–258,343 ticks/s |

### Performance (measured unless marked derived)

| | Daemon-live (15 runs) | Baseline (5 runs) | Daemon-simulation (5 runs) |
|---|---|---|---|
| Per-tick latency p50 | 2.9–4.4 µs | 0.9–1.5 µs | 2.5–2.6 µs |
| Per-tick latency p99 | 4.8–16.8 µs | 1.5–16.3 µs | 4.4–6.5 µs |
| Per-tick latency max | 46–370 µs | 44–119 µs | 46–186 µs |
| Throughput | 181,172–258,343 ticks/s | 432,563–944,165 ticks/s | 232,649–286,156 ticks/s |
| Peak RSS (VmHWM) | 7.9–8.0 MB | 8.7–8.8 MB | 7.4–7.8 MB |

- **Noisy repetitions.** r0 and r4 of daemon-live `main`, r0 of `zeroed`, and
  r0 and r4 of the baseline ran markedly slower (p99 up to about 17 µs, lower
  throughput). Daemon and baseline slowed together in the same repetitions,
  which points to noise on the shared VM rather than to daemon behavior. The
  other repetitions match the superseded run closely.
- **Derived overhead** (daemon-live main − baseline main, matching reps):
  - Δp50 is about 2.0 µs in reps 1–3; the range over all reps is 1.4–2.9 µs.
  - Δp99 is 0.5–6.7 µs.
  - The daemon reaches 27–44 % of the baseline's tight-loop throughput. The
    44 % is r0, where the baseline itself was slowed.

  The overhead covers typed JSON decode and validation, queue admit/drain,
  health updates, and spike conversion. Encoding the frame is producer work and
  is excluded.
- **Derived budget headroom.** The worst daemon p99 (16.8 µs) is about 1.7 %
  of the 1 ms tick budget. The worst single tick (370 µs) is still under 40 %
  of the budget.
- **RSS caveat.** Baseline RSS is higher because the baseline driver
  pre-decodes all 10,000 inputs before the timed loop. RSS here measures each
  driver process, not the engine alone.

Transport, measured, 3 reps × 200 probes:

| | Value |
|---|---|
| Probe latency, pooled (n = 600) | p50 1.08 ms, p99 1.77 ms, max 3.51 ms |
| Output batches per expected tick (derived) | 0.999–1.000 (9,995–10,006 batches per 10,005 ms window) |
| Daemon peak RSS | 6.6 MB |
| Shutdown | exit 0 in 3.1–3.8 ms after SIGTERM |

The probe-latency spread is consistent with a uniform wait of 0–1 ms for the
next tick plus roughly 0.4–0.6 ms of ZeroMQ, scheduling, and JSON overhead.
That split is an **estimate**: the experiment did not measure the
decomposition.

Two single probes (r1, r2) exceeded 3 ms. T3 is a p99 criterion, so they are
reported here rather than counted as failures.

About 13–15 % of consecutive `batch_id` steps are 0 ms and a matching share
are 2 ms. That is aliasing: `batch_id` is the wall-clock millisecond and
ticks jitter around millisecond boundaries. It is not loss. In r0, 9,995
batches arrived in a 10,005 ms window, about 10 fewer than expected. Possible
causes are `MissedTickBehavior::Skip` and silent PUB drops, and the publisher
cannot distinguish them (documented PUB semantics).

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
  - Every probe gets the correct, selective response, with p99 1.8 ms at 1 kHz.
  - SIGTERM gives a clean exit in under 4 ms.
  - Peak RSS is about 6.6 MB.
- **Ample headroom.** At this network size, daemon overhead is a few µs per
  tick, far inside a 1 ms budget, even in the noisy repetitions.

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
  CPU pinning or isolation. Tail latencies, and whole repetitions in this run,
  reflect that noise.
- **Order-based matching.** Transport matching is by order and time only.
- **Linux only.** RSS comes from `/proc`.
- **Scope.** Not tested: rejection paths (stale, schema, or non-finite frames)
  under load, overload/backpressure, and long-duration stability.
- **Release qualification** (LIM-1447) is separate and is not established by
  this report.

## Protocol deviations and disclosures

- **No changes to criteria, workload, fixture, or configuration** after
  pre-registration.
- **History rewrite and re-measurement.**
  - The maintainer asked for every commit to be re-authored, which rewrote
    the branch history.
  - Original pre-registration commit: `5b6c5ae`. Original measured commit:
    `ecfeb9d`, recorded 2026-10-17T00:44:15Z, which also passed every check.
  - The new pre-registration commit `ff6f300` has byte-identical content to
    `5b6c5ae` (`git diff 5b6c5ae ff6f300` is empty). `5b6c5ae` predates both
    measurements.
  - Rewritten commits get new SHAs, so the original results no longer named
    a commit on the branch. The experiment was therefore re-run at the new
    measured commit `3fe3f42`, and those are the results reported here.
- **Tooling changes before re-measurement.** Between `ecfeb9d` and `3fe3f42`
  the drivers, runner, and summarizer were refactored to satisfy repository
  static analysis (Codacy, CodeScene, DeepSource) and two review findings.
  - **Same measurement logic.** The drivers keep the same measurement points
    and output schema. All 25 in-process rasters of this run are identical,
    file by file, to those of the `ecfeb9d` run.
  - **Stricter summarizer.** It now fails unevaluated checks and adds gate I1,
    which checks that every pre-registered run is present. Before, a missing
    run family could pass vacuously. Re-applied to the `ecfeb9d` data, it
    gives the same pass/fail result on every check, and I1 passes.
  - **Runner.** The clean-tree guard ignores earlier result sets under
    `results/`, and step durations are recorded in milliseconds.
- **Development smoke runs.** Driver development included smoke runs of the
  unfinished drivers, written to scratch directories. They confirmed that the
  drivers ran and that the summarizer detects failures: an injected one-tick
  baseline corruption fails C4, and deleting run families fails I1 and the
  affected checks. Those outputs were discarded. They are not part of the
  results.
- **Results packaging.**
  - Raw per-tick JSONL files are committed gzip-compressed (`*.jsonl.gz`,
    `gzip -9 -n`) to keep the repository small. `summarize.py` reads plain
    JSONL.
  - To re-derive the summary, copy the results directory, run
    `gunzip -k raw/*.jsonl.gz`, and run `summarize.py` on the copy.
    `measured_commit` is taken from the directory name.
  - Daemon logs are stored as `*.daemon.txt`, because `.gitignore` excludes
    `*.log`.

## Decision

**The evidence supports further development of `brainstem-daemon` as runtime
plumbing.**

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
bash .agents/setup                  # packages, Rust 1.98.1, cargo fetch --locked
experiments/usefulness/run.sh       # clean tree required; writes results/<HEAD sha>/
```
