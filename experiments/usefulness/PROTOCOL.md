# Usefulness experiment protocol (pre-registered)

Tracker: LIM-1457 / GitHub issue #70. This file is committed **before** any
measurement is recorded. The commit SHA that introduces it is the
pre-registration SHA quoted in `REPORT.md`. Decision criteria below are not
edited after results exist; any necessary change is recorded as a deviation
in `REPORT.md`.

Scope: only `Limen-Neural/brainstem-daemon`, its public interfaces, the
repository-local fixture under `fixtures/`, and published dependencies
(`neuromod` 0.7.0, `corpus-ipc` 0.1, `zmq` 0.10). Nothing is fetched from
other repositories or model hubs.

## 1. Workload and the behavior under test

**Practical behavior demonstrated.** A typed `corpus-ipc` stimulus stream
(`IpcMessage::Stimuli` with schema `corpus-ipc.stimulus.v1`, validity masks,
and `IpcMessage::Neuromodulators` packets) drives a restored, nonblank LIF
network through the daemon's supported start → input → tick → output →
shutdown path. For each tick the daemon must emit exactly the spike channel
set the fixture predicts, keep correct accounting (ticks, accepted batches,
health), stay within the tick budget, and shut down cleanly.

**Fixture.** `fixtures/lif8x16-v1/snn_model.json`: 16 LIF neurons × 8 input
channels, source `spikenaut_julia`, no `output_weights`, finite parameters,
decay 0.15, threshold 0.25. Neuron `i < 8` has weight 2.0 on channel `i`
only; neuron `8 + k` has weight 1.0 on channels `k` and `(k + 1) % 8`. Every
row sums to the engine L1 budget (2.0), so per-step renormalization is a
no-op. The fixture is **synthetic and untrained**; its SHA-256 is recorded in
`fixtures/lif8x16-v1/MANIFEST.json` and verified by `run.sh`. It cannot show
trained-model usefulness; it shows whether the runtime moves inputs to
outputs correctly and cheaply.

**Workload** (`fixtures/workload.json`): 20,000 ticks = 10 cycles × 2,000
ticks. Each cycle (tick offsets):

| Segment | Offsets | Content |
|---|---|---|
| A isolated pulses | 0, 20, …, 140 | channel `c` = 1.0 at `20·c`, all valid |
| B burst | 200, 201 | channel 3 = 1.0 on two consecutive ticks |
| C two channels | 300 | channels 1 and 5 = 1.0 in one frame |
| D weak pulse | 400 | channel 2 = 0.3 |
| M masked | 500 | channel 4 = 1.0 **masked out**, channel 6 = 1.0 valid |
| Z zero-equivalent | 600 | channel 4 = 0.0, channel 6 = 1.0, all valid |
| N modulators | 700, 701 | modulator-only packet (DA 0.5, cortisol 0.2, ACh 0.4, tempo 0.5), then channel 0 = 1.0 carrying the same modulators |
| idle | all other ticks | source returns `None`; the daemon zero-fills |

Every stimulus frame is a `StimulusBatch` with a unique `batch_id`, a
current timestamp, `session_id = "usefulness"`, and
`metadata.custom.schema = "corpus-ipc.stimulus.v1"`. The first 1,000 ticks
are warm-up for latency statistics only; correctness is checked on all ticks.

**Expected outputs** (`fixtures/expected_spikes.json`) are produced by
`make_fixture.py` from `reference_model.py`, a standard-library
re-implementation of the documented `neuromod` 0.7.0 LIF step (its
normal-mode pipeline is unchanged from 0.6.0). With the
loader's `reward_lr = 0.0` no weight ever changes, so the engine's
thread-local RNG (which only feeds STDP traces) cannot affect spikes. The
generator asserts a minimum distance of 5·10⁻³ between every membrane
potential and its threshold so f32 rounding cannot flip a crossing. Isolated
pulse on channel `c` → neurons `{c, 8+c, 8+((c+7) mod 8)}`; the weak pulse →
`{2}` only; M and Z → identical sets.

## 2. Modes (kept distinct)

| Mode | Construction | Role |
|---|---|---|
| **live** | `DaemonConfig::load(config/live.toml)` → `BrainstemDaemon::try_with_backend` with a workload source and a recording sink → `run_for_ticks(20000)` | primary evidence |
| **simulation** | same, `config/simulation.toml` (blank `with_dimensions()` network) | negative control: no stimulus-driven spikes |
| **baseline** | `brainstem_daemon::restore_network(&live config)` then `SpikingNetwork::step` per tick with the daemon's documented decoding applied by the driver | comparator |
| **transport-live** | the `corpus-ipc` binary with `config/transport-live.toml`, external ZMQ producer/consumer, `/readyz`, SIGTERM | executable path evidence |
| stub | `BrainstemDaemon::new()`/`try_new()` or the default-feature binary: no input path, no-op output | documented only; **not evidence** |
| `RuntimeHarness` / `FakeCore` | test harness | **not evidence** |

Drivers load TOML only through `DaemonConfig::load` (which validates) and
construct only through fallible constructors.

## 3. Baseline and comparability

The baseline is the direct-engine loop: the same `restore_network` result
(same fixture, same `reward_lr = 0.0`), the same decoded stimuli (mask → 0,
pad/truncate to 8 channels) and the same positional modulator mapping
(dopamine/cortisol/acetylcholine/tempo → DA/5-HT/ACh/NE), calling
`SpikingNetwork::step` directly with no ingress queues, health, accounting,
or transport. It has identical output semantics, so per-tick equality is
testable and the difference in latency is the daemon's overhead.

What the daemon is expected to add over the baseline: typed validation,
mask handling, bounded classified ingress, health/readiness, accounting,
ZMQ transport, and signal lifecycle. It is **not** expected to add model
capability or to be faster than the baseline.

## 4. Correctness checks (pre-registered)

In-process, per repetition:

- **C1** `RuntimeStats.ticks == 20000` (every tick stepped) and the sink
  received exactly 20,000 batches.
- **C2** `accepted_batches ==` number of stimulus frames sent (150 per run);
  `rejected_batches == receive_errors == emit_errors == dropped_spikes == 0`.
- **C3** Health after the run: `ready == true`, `fatal == None`, phase not
  `fatal`/`draining`.
- **C4** Daemon-live per-tick channel sets equal baseline per-tick sets on
  all 20,000 ticks (timestamps and batch ids excluded).
- **C5** Daemon-live per-tick channel sets equal `expected_spikes.json`
  (the independent reference) on all ticks.
- **C6** Mask equivalence: the daemon's sets on ticks `[500, 520)` equal
  those on `[600, 620)` within every cycle.
- **C7** Idle ticks (no event in the workload) emit exactly what the
  reference predicts (an empty set except the reference-predicted echoes).
- **C8** Simulation control: zero spikes on all 20,000 ticks, `ticks ==
  20000`, `accepted_batches == 150`.
- **C9** Ingress metrics: sensory `accepted == 150`, reward `accepted ==
  20`, no rejected/dropped in any class.

Transport, per repetition:

- **T1** `/readyz` returns 200 before any frame is sent; daemon RSS is read
  from `/proc/<pid>/status` before shutdown.
- **T2** Subscription is confirmed by receiving a `Spikes` frame (empty
  allowed), then priming pulses are sent until one nonempty frame with the
  expected set arrives. Priming frames are excluded from measurement.
- **T3** For 200 measured pulses (channel `k mod 8`, one every 20 ms, fresh
  timestamps): nonempty frames received == 200 and each matches
  `expected_by_channel` for its pulse, in order. Any mismatch or missing
  frame is a failure (`lost = sent − matched`).
- **T4** `/metrics` reports `brainstem_ready 1` and `brainstem_fatal 0`
  after the measured pulses.
- **T5** SIGTERM leads to exit status 0 within 10 s and the log contains
  `Termination signal received`.

Divergence rule: if daemon and baseline (or reference) outputs diverge, the
cause must be attributed before the run counts as a pass. Known sources
that could produce divergence are listed in §8; none is expected to apply
under this fixture.

## 5. Performance metrics (pre-registered)

Measured, after excluding the 1,000 warm-up ticks:

- **In-process tick latency** per tick, p50 / p99 / max in nanoseconds.
  Daemon: from the instant the source returns the packet (after
  `accept_ipc_json`) to the instant the sink's `emit` is called. The decode
  cost (`accept_ipc_json`) is recorded separately per stimulus frame.
  Baseline: from the instant the driver has the decoded stimuli to the
  instant `step` returns and ids are converted to `u16` channels.
- **Throughput**: ticks per second over the measured ticks of
  `run_for_ticks` (synchronous tight loop; no timer) and of the baseline
  loop.
- **Peak RSS**: `VmHWM` from `/proc/self/status` at the end of each process.
- **Transport**: end-to-end latency from producer send to consumer receive
  for each measured pulse (p50 / p99 / max), delivered/lost counts, output
  frames received (including empty), daemon `VmHWM` and `VmRSS`, and time
  from SIGTERM to exit.

Repetitions: R = 5 in-process per mode (daemon-live, baseline,
daemon-simulation), R = 3 transport; every repetition is a separate process.
`summarize.py` labels each value `measured`; the budget comparison is
`derived`.

## 6. Decision criteria (fixed before running)

- **Supports further development**: C1–C9 pass in all repetitions, T1–T5
  pass in all repetitions, daemon-live p99 tick latency ≤ 1,000,000 ns (the
  1 ms budget at 1 kHz) in every repetition, and transport p99 end-to-end
  latency ≤ 5 ms (one tick period of quantization plus loopback ZMQ) with
  zero lost pulses.
- **Change of direction**: any unexplained output divergence (C4/C5/T3), or
  the in-process budget is missed.
- **Blocked**: the supported path cannot be exercised (fixture rejected,
  binary fails to start/exit, driver cannot subscribe).

Independent of the outcome: trained-model usefulness remains unproven until
a compatible artifact without `output_weights` exists.

## 7. Reproduction from a fresh checkout (Linux only)

```bash
# Debian/Ubuntu packages, pinned toolchain, dependency fetch (network needed once)
sudo apt-get install -y build-essential curl pkg-config libzmq3-dev python3
rustup toolchain install 1.98.1 --profile minimal
cargo +1.98.1 fetch --locked
# Verify the fixture and expectations regenerate byte-for-byte, then run.
python3 -I -B experiments/usefulness/make_fixture.py && git diff --exit-code experiments/usefulness/fixtures
experiments/usefulness/run.sh                      # refuses a dirty tree
python3 -I -B experiments/usefulness/summarize.py experiments/usefulness/results/<sha>
```

`run.sh` records the commit SHA, `rustc`/`cargo` versions, kernel, CPU model
and core count, memory size, features, and SHA-256 of every config and
fixture file into `results/<sha>/identity.json`, keeps a per-step command
log, and keeps partial results if a step fails. Raw per-tick records are
gzipped JSONL; step, command and daemon logs are committed next to them.
`summarize.py` embeds `identity.json` without the `status` and
`finished_utc` fields that `run.sh` finalizes after summarizing, so
re-running it on committed results is byte-identical. `run.sh` passes the
release `brainstem-daemon` binary to the transport driver with `--daemon`.
Results are committed in a follow-up commit; the measured SHA is that
commit's parent.

Fixture ports: 5755 (producer PUB / daemon SUB), 5756 (daemon PUB), 9755
(control). They must be free on 127.0.0.1.

## 8. Known divergence and interpretation caveats

- `SpikingNetwork::step` uses a thread-local RNG that is not checkpointed.
  Under `reward_lr = 0.0` it cannot change weights or spikes; a nonzero
  reward rate would make per-tick equality probabilistic.
- The engine mutates `decay_rate` (from ACh), blends `threshold` toward a
  modulator-defined target when dopamine is nonzero, and L1-renormalizes
  weight rows every step. The fixture is designed so renormalization is a
  no-op; the threshold drift from segment N is part of the reference model.
- Output `SpikeEvent.time`, batch ids, and timestamps are wall-clock values
  and are excluded from equality.
- In-process, modulators apply only on the tick whose packet carries them
  (no hold). The ZMQ source holds the last modulator vector across idle
  ticks; the transport run therefore sends no modulator packets.
- The daemon maps `corpus-ipc` `tempo` (nominal 1.0, range 0.5–2.0) onto
  norepinephrine positionally. The workload uses `tempo = 0.5` to keep the
  stress multiplier at 0.5.
- Transport correlates input and output by order and time only; the daemon
  does not propagate input batch/session ids. PUB sends are not delivery
  acknowledgments; loss is measured at the subscriber.
- `VmHWM` is Linux-specific; the transport driver sends SIGTERM through `sh -c kill`.
- Release qualification and publication stay in LIM-1447.
