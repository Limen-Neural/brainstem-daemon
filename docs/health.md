# Runtime health

Supervisors should treat **liveness** and **readiness** as independent. A live
`brainstem-daemon` process has a health reporter; it is ready only after
`StimulusSource::initialize` succeeds **and** a checkpoint is validated
(`restore_network` in live mode, or the explicit simulation blank network).
Live mode uses the Distill sidecar SHA-256 as `checkpoint.digest`. Simulation
mode reports `digest: null`.

This repository had no HTTP/metrics server before this surface. When
`control_bind` is set, `BrainstemDaemon::run` starts **one** listener:

| Path | Meaning |
|---|---|
| `GET /livez` | `200` if live, `503` otherwise |
| `GET /readyz` | `200` if ready, `503` otherwise |
| `GET /health` | `200` JSON [`HealthSnapshot`](../src/health/snapshot.rs) (inspect `phase`); bounded `503 busy` if the control plane cannot snapshot |
| `GET /metrics` | Prometheus text; labels are phase/reason codes only; same `503 busy` exception |

Leave `control_bind` unset to preserve the historical no-extra-socket default.
Do not add a second control server beside this one.

Library embedders can also clone [`HealthHandle`](../src/health/handle.rs) from
`BrainstemDaemon::health()` and call `snapshot()` / `try_snapshot()` without
waiting on the tick loop's backend or `SpikingNetwork::step`. Control probes
use `try_snapshot()` so they never wait on an in-flight `apply`.

## Transition table

| From | Event | To | live | ready | Notes |
|---|---|---|---|---|---|
| (unstarted) | `ProcessStarted` | `starting` | true | false | Construction. Live does not imply ready. |
| (unstarted) | init/checkpoint success or failure | (unstarted) | false | false | Ignored until `ProcessStarted`; the fatal transition below applies only after `ProcessStarted`. |
| `starting` | `InitializationCompleted` | `loading_checkpoint` | true | false | `initialize()` succeeded. Checkpoint restore is a separate gate. |
| `starting` | `InitializationFailed` | `fatal` | true | false | Sticky. Detail is JSON-only, never a metric label. |
| `loading_checkpoint` | `CheckpointValidated` | `running` | true | true | Ready only after this gate. Live identity is sidecar `model_id` plus SHA-256. |
| `loading_checkpoint` | `CheckpointRejected` | `fatal` | true | false | Sticky. |
| `running` | clock ≥ `stale_after` without ingress | `degraded` | true | true | Reason `stale_input`. Ready stays true. |
| `degraded` (stale) | `IngressObserved` | `running` (if no other reasons) | true | true | Ticks without ingress do **not** clear stale. Empty stub packets do not count. |
| `running` | queue fill ≥ `overload_high` | `degraded` | true | true | Reason `overload`. Fill is the aggregate of the four bounded ingress classes. |
| `degraded` (overload) | fill ≤ `overload_low` | `running` (if no other reasons) | true | true | Hysteresis: mid-band does not recover. |
| `running` / `degraded` | `BeginDrain` | `draining` | true | false | SIGTERM/SIGINT. Does not return to ready. |
| any started non-fatal | `Fatal` / init or checkpoint failure | `fatal` | true | false | Subsequent validate/tick/drain cannot restore ready. Unstarted init/checkpoint failures stay ignored. |

After `BeginDrain`, `BrainstemDaemon::run` stops the control listener. External
`/readyz` probes may get connection refused rather than `503`. In-process
`HealthHandle::snapshot()` still reports `phase: draining`. There is no probe
grace period. If all 32 in-flight control slots are busy, up to 4 extra
connections get a short `503` (`busy`); further accepts are closed immediately.
The same `503` is used when a snapshot read would block on an in-flight `apply`.

Recoverable reasons (`stale_input`, `overload`) are independent: clearing one
leaves the other. `capacity == 0` means “no queue instrumented” and never
counts as overload. After LIM-1216 the tick loop reports the sum of the four
class depths and capacities.

Fatal and draining are sticky for **this process**. A new process starts in
`starting` again.

Configuration rejection happens earlier: `DaemonConfig::validate()` runs before
checkpoint allocation/restoration, source initialization, or binary socket
startup. Invalid configuration returns an error from fallible constructors and
`restore_network`; the CLI prints the offending field and exits `1`, without
starting the control listener or exposing a `fatal` health snapshot. A restart
with unchanged invalid configuration fails again. Exit `1` does not suppress
`Restart=on-failure`; use the explicit restart delay and start limits in the
[systemd service example](../README.md#systemd-user-service-fedora-43), then fix
the configuration before resetting the failed unit and restarting it.

## Runtime counters

The tick loop publishes its cumulative `RuntimeStats` counters to the health
handle once per tick (one in-memory `CountersPublished` event, no I/O and no
backend lock), including ticks that return early. They appear in `/health`
under `counters` and on `/metrics` as unlabeled Prometheus counters. Counters
only increase for the life of the process (the health machine keeps the
field-wise maximum) and reset when the process restarts, which Prometheus
`rate()`/`increase()` handle as a counter reset.

| `/health` field (`counters.*`) | `/metrics` counter | Meaning |
|---|---|---|
| `ticks` | `brainstem_ticks_total` | Successful network steps. A tick whose drained input is non-finite (rejected before the step) or whose step fails is not counted; backend-rejected or receive-error ticks still step on queued or zero-filled input and are counted. |
| `ticks_skipped` | `brainstem_ticks_skipped_total` | Scheduler slots dropped by `MissedTickBehavior::Skip` after a late tick; the period never ran. See [Scheduler skips](#scheduler-skips). |
| `accepted_batches` | `brainstem_accepted_batches_total` | Non-rejected backend packets carrying a `batch_id`, counted before bounded-queue admission (a later queue overflow does not decrement it). |
| `rejected_batches` | `brainstem_rejected_batches_total` | Batches rejected: backend-flagged, receive failure, or non-finite input. |
| `receive_errors` | `brainstem_receive_errors_total` | Backend receive failures, including rate-limited repeats. |
| `emit_errors` | `brainstem_emit_errors_total` | Spike emission failures, including rate-limited repeats. |
| `dropped_spikes` | `brainstem_dropped_spikes_total` | Spikes discarded because their IDs did not fit the wire type. |
| `diagnostic_emissions` | `brainstem_diagnostic_emissions_total` | Tick diagnostics actually logged after rate limiting. |
| `suppressed_diagnostics` | `brainstem_suppressed_diagnostics_total` | Recurring diagnostic occurrences suppressed by rate limiting. |

New runtime counters plug into the same path, as `ticks_skipped` does: add
the field to `RuntimeStats`, copy it in `From<&RuntimeStats> for RuntimeCounters`
(`src/daemon.rs`), and add the field, its `merged_monotonic` line, and one row
to `RuntimeCounters::metrics()` (`src/health/snapshot.rs`).

## Scheduler skips

`ticks_skipped` counts scheduler slots that Tokio `MissedTickBehavior::Skip`
drops when the loop runs late — the period never ran and `SpikingNetwork::step`
was never called for it. `Interval::tick()` returns the *scheduled* instant of
the slot it fires; under `Skip`, consecutive returned instants are always
whole periods apart, so each consumed tick records
`(scheduled_n − scheduled_{n−1}) / period − 1` (saturating; the first tick
counts 0).

A wakeup that runs late *inside* Tokio's ~5 ms tolerance is not a skip: Tokio
bursts through the pending slots, the scheduled instants stay one period
apart, and the count stays 0. Only slots past that tolerance are dropped and
counted — the counter mirrors what the scheduler actually did instead of
estimating lateness, so it cannot report a skip that did not happen.

`ticks_skipped` is an observability signal only:

- it sets no reason code, does not move `phase`, and does not affect `ready`
  or `live`;
- missed network steps are not replayed — cadence realigns to the original
  schedule with no catch-up burst;
- `run_for_ticks`/`drive_ticks` have no scheduler interval and always report 0;
- skips are attributed on the wakeup that observes the gap; a trailing
  unconsumed slot at shutdown is never counted.

Distinguish a scheduler skip from the nearby conditions:

| Condition | Signal | What actually happened |
|---|---|---|
| Scheduler skip | `ticks_skipped` | The period never ran; no `network.step` for that slot. |
| No ingress | `input_freshness.age_ms` / `stale` | The tick ran and stepped on queued or zero-filled input; `ticks` keeps rising. |
| Rejected input | `rejected_batches` | Input was rejected (backend-flagged, receive failure, or non-finite); the tick still steps unless the drained packet itself was non-finite. |
| Receive error | `receive_errors` | `StimulusSource::next_ingress` failed; the tick still ran. |
| Egress loss/failure | `emit_errors`, `dropped_spikes` | The step ran; encoding or publishing its spikes failed. |

The `StimulusSource` contract phrase "skip this tick" (a `None` packet) means
a zero-stimulus step — the tick runs — and is unrelated to a scheduler skip.
Rate-limited `warn` diagnostics (`skipped_this_wakeup`, cumulative
`ticks_skipped`, `suppressed`) share the runtime's existing one-second minimum
gap; every dropped slot is counted whether the diagnostic emits or suppresses.

## Checkpoint identity

Live mode copies [`ModelProvenance::model_id`](../src/checkpoint/mod.rs) and
the sidecar SHA-256 into `checkpoint`. Simulation mode uses
`id = "simulation/blank"` and `digest: null`.

## Example snapshots

Healthy (ready to consume events) after a live sidecar restore:

```json
{
  "live": true,
  "ready": true,
  "phase": "running",
  "reasons": [],
  "last_successful_tick_ms": 0,
  "tick_age_ms": 0,
  "checkpoint": { "id": "spikenaut-snn:soma16", "digest": "abc123" },
  "input_freshness": { "age_ms": 0, "stale": false },
  "queue_pressure": { "depth": 0, "capacity": 216, "ratio": 0.0, "overloaded": false },
  "fatal": null,
  "counters": { "ticks": 0, "ticks_skipped": 0, "accepted_batches": 0, "rejected_batches": 0, "receive_errors": 0, "emit_errors": 0, "dropped_spikes": 0, "diagnostic_emissions": 0, "suppressed_diagnostics": 0 },
  "observed_at_ms": 0
}
```

Degraded (still ready; supervisors should not bounce the process):

```json
{
  "live": true,
  "ready": true,
  "phase": "degraded",
  "reasons": ["stale_input", "overload"],
  "last_successful_tick_ms": 0,
  "tick_age_ms": 0,
  "checkpoint": { "id": "spikenaut-snn:soma16", "digest": "abc123" },
  "input_freshness": { "age_ms": 100, "stale": true },
  "queue_pressure": { "depth": 95, "capacity": 100, "ratio": 0.95, "overloaded": true },
  "fatal": null,
  "counters": { "ticks": 412, "ticks_skipped": 6, "accepted_batches": 400, "rejected_batches": 3, "receive_errors": 1, "emit_errors": 0, "dropped_spikes": 0, "diagnostic_emissions": 2, "suppressed_diagnostics": 5 },
  "observed_at_ms": 100
}
```

Fatal (never returns to ready in this process):

```json
{
  "live": true,
  "ready": false,
  "phase": "fatal",
  "reasons": ["fatal"],
  "last_successful_tick_ms": null,
  "tick_age_ms": null,
  "checkpoint": null,
  "input_freshness": { "age_ms": null, "stale": false },
  "queue_pressure": { "depth": 0, "capacity": 0, "ratio": null, "overloaded": false },
  "fatal": { "code": "checkpoint_invalid", "detail": "blank weights" },
  "counters": { "ticks": 0, "ticks_skipped": 0, "accepted_batches": 0, "rejected_batches": 0, "receive_errors": 0, "emit_errors": 0, "dropped_spikes": 0, "diagnostic_emissions": 0, "suppressed_diagnostics": 0 },
  "observed_at_ms": 0
}
```

`fatal.detail` belongs in JSON/logs. Prometheus `/metrics` exposes
`brainstem_fatal 1` and `brainstem_phase{phase="fatal"} 1` without the detail
string. Optional gauges (`last_successful_tick_ms`, `tick_age_ms`,
`input_age_ms`) emit `NaN` when the observation is missing so supervisors do
not treat startup as a tick at time zero.
