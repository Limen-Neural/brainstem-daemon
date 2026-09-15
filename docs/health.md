# Runtime health

Supervisors should treat **liveness** and **readiness** as independent. A live
`brainstem-daemon` process has a health reporter; it is ready only after
`StimulusSource::initialize` succeeds. Until [`LIM-1133`](https://linear.app/rpd-34/issue/LIM-1133),
that successful `initialize` is also the checkpoint-validation stand-in: the
daemon applies `CheckpointValidated` immediately afterward. There is no
separate digest/weight check in this release.

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
waiting on the tick loop's backend or `SpikingNetwork::step`.

## Transition table

| From | Event | To | live | ready | Notes |
|---|---|---|---|---|---|
| (unstarted) | `ProcessStarted` | `starting` | true | false | Construction. Live does not imply ready. |
| (unstarted) | init/checkpoint success or failure | (unstarted) | false | false | Ignored until `ProcessStarted`; the fatal transition below applies only after `ProcessStarted`. |
| `starting` | `InitializationCompleted` | `loading_checkpoint` | true | false | `initialize()` succeeded. Live daemon then applies the checkpoint stand-in (next row). |
| `starting` | `InitializationFailed` | `fatal` | true | false | Sticky. Detail is JSON-only, never a metric label. |
| `loading_checkpoint` | `CheckpointValidated` | `running` | true | true | Ready only after this gate. Until LIM-1133 the daemon emits this right after `initialize`. |
| `loading_checkpoint` | `CheckpointRejected` | `fatal` | true | false | Sticky. |
| `running` | clock ≥ `stale_after` without ingress | `degraded` | true | true | Reason `stale_input`. Ready stays true. |
| `degraded` (stale) | `IngressObserved` | `running` (if no other reasons) | true | true | Ticks without ingress do **not** clear stale. |
| `running` | queue fill ≥ `overload_high` | `degraded` | true | true | Reason `overload`. |
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
leaves the other. `capacity == 0` means “no queue instrumented” (LIM-1216) and
never counts as overload.

Fatal and draining are sticky for **this process**. A new process starts in
`starting` again.

## Checkpoint stand-in

[`LIM-1133`](https://linear.app/rpd-34/issue/LIM-1133) will load and digest a
real Spikenaut checkpoint. Until then, a successful `StimulusSource::initialize`
is treated as the checkpoint gate. The snapshot identity is the `model_path`
file name (not the full path) with `digest: null`.

## Example snapshots

Healthy (ready to consume events). Until LIM-1133 the live daemon stand-in uses
`"digest": null`.

```json
{
  "live": true,
  "ready": true,
  "phase": "running",
  "reasons": [],
  "last_successful_tick_ms": 0,
  "tick_age_ms": 0,
  "checkpoint": { "id": "soma16", "digest": null },
  "input_freshness": { "age_ms": 0, "stale": false },
  "queue_pressure": { "depth": 0, "capacity": 0, "ratio": null, "overloaded": false },
  "fatal": null,
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
  "checkpoint": { "id": "soma16", "digest": null },
  "input_freshness": { "age_ms": 100, "stale": true },
  "queue_pressure": { "depth": 95, "capacity": 100, "ratio": 0.95, "overloaded": true },
  "fatal": null,
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
  "observed_at_ms": 0
}
```

`fatal.detail` belongs in JSON/logs. Prometheus `/metrics` exposes
`brainstem_fatal 1` and `brainstem_phase{phase="fatal"} 1` without the detail
string.
