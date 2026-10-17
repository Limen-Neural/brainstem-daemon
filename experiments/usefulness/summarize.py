#!/usr/bin/env python3
"""Turn raw usefulness-experiment results into the pre-registered checks and
metrics (PROTOCOL.md §4–§6). Standard library only.

Usage: summarize.py <results-dir>
Writes <results-dir>/summary.json and <results-dir>/summary.md.

Every number is labelled `measured` except the budget comparisons, which are
`derived` from the pre-registered thresholds.
"""

import glob
import gzip
import json
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
TICK_BUDGET_NS = 1_000_000  # 1 ms at 1 kHz (PROTOCOL.md §6)
TRANSPORT_BUDGET_NS = 5_000_000  # 5 ms end-to-end (PROTOCOL.md §6)
SIGTERM_BUDGET_NS = 10_000_000_000
MASKED = (500, 520)
ZERO = (600, 620)


def load_json(path):
    with open(path) as fh:
        return json.load(fh)


def load_records(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def percentile(sorted_values, p):
    """Nearest-rank on a sorted list: index round((n - 1) * p)."""
    if not sorted_values:
        return None
    return sorted_values[round((len(sorted_values) - 1) * p)]


def latency_stats(values):
    values = sorted(values)
    return {
        "count": len(values),
        "p50_ns": percentile(values, 0.50),
        "p99_ns": percentile(values, 0.99),
        "max_ns": values[-1] if values else None,
        "min_ns": values[0] if values else None,
        "mean_ns": (sum(values) / len(values)) if values else None,
        "label": "measured",
    }


def run_files(out, prefix):
    runs = []
    for summary_path in sorted(glob.glob(os.path.join(out, f"{prefix}-r*.summary.json"))):
        stem = summary_path[: -len(".summary.json")]
        records = stem + ".jsonl.gz"
        if not os.path.exists(records):
            records = stem + ".jsonl"
        runs.append((os.path.basename(stem), load_json(summary_path), records))
    return runs


def channel_sets(records):
    return [tuple(r["channels"]) for r in records]


def check_inprocess(name, summary, records, workload, expected, baseline_sets):
    """C1–C9 for one in-process run. Returns (checks dict, metrics dict, sets)."""
    ticks = workload["ticks"]
    warmup = workload["warmup_ticks"]
    tpc = workload["ticks_per_cycle"]
    mode = summary["mode"]
    sets = channel_sets(records)
    expected_sets = [tuple(expected["nonempty_ticks"].get(str(t), [])) for t in range(ticks)]
    stats = summary.get("runtime_stats", {})
    frames = summary["stimulus_frames_sent"]
    checks = {}
    if mode.startswith("daemon"):
        checks["C1_ticks"] = stats.get("ticks") == ticks and len(records) == ticks
        checks["C2_accounting"] = (
            stats.get("accepted_batches") == frames
            and stats.get("rejected_batches") == 0
            and stats.get("receive_errors") == 0
            and stats.get("emit_errors") == 0
            and stats.get("dropped_spikes") == 0
        )
        health = summary["health"]
        checks["C3_health"] = (
            health["ready"] is True
            and health["fatal"] is None
            and health["phase"] not in ("fatal", "draining")
        )
        ingress = summary["ingress"]
        checks["C9_ingress"] = (
            ingress["sensory"]["accepted"] == frames
            and ingress["reward"]["accepted"] == summary["modulator_frames_sent"]
            and all(
                ingress[c]["rejected"] == 0 and ingress[c]["dropped"] == 0
                for c in ("sensory", "reward", "control", "telemetry")
            )
        )
    else:
        checks["C1_ticks"] = len(records) == ticks
    if mode == "daemon-simulation":
        checks["C8_simulation_no_spikes"] = all(not s for s in sets) and len(sets) == ticks
    else:
        mismatches = [t for t, (a, b) in enumerate(zip(sets, expected_sets)) if a != b]
        key = "C5_matches_reference" if mode == "daemon-live" else "B5_baseline_matches_reference"
        checks[key] = not mismatches and len(sets) == ticks
        if mismatches:
            checks[key + "_first_mismatches"] = [
                {"tick": t, "got": list(sets[t]), "expected": list(expected_sets[t])}
                for t in mismatches[:5]
            ]
        events = {
            c * tpc + e["offset"] for c in range(workload["cycles"]) for e in workload["cycle_events"]
        }
        idle_bad = [t for t in range(ticks) if t not in events and sets[t] != expected_sets[t]]
        checks["C7_idle_ticks" if mode == "daemon-live" else "B7_idle_ticks"] = not idle_bad
        mask_ok = True
        for cycle in range(workload["cycles"]):
            base = cycle * tpc
            m = sets[base + MASKED[0] : base + MASKED[1]]
            z = sets[base + ZERO[0] : base + ZERO[1]]
            if m != z:
                mask_ok = False
        checks["C6_mask_equivalence" if mode == "daemon-live" else "B6_mask_equivalence"] = mask_ok
    if mode == "daemon-live" and baseline_sets is not None:
        checks["C4_equals_baseline"] = sets == baseline_sets
    measured = [r["latency_ns"] for r in records[warmup:]]
    decode = [r["decode_ns"] for r in records if "decode_ns" in r]
    metrics = {
        "tick_latency": latency_stats(measured),
        "decode_latency": latency_stats(decode) if decode else None,
        "ticks_per_s": {"value": summary["timing"]["ticks_per_s"], "label": "measured"},
        "measured_wall_ns": summary["timing"]["measured_wall_ns"],
        "run_wall_ns": summary["run_wall_ns"],
        "peak_rss_kb": {"value": summary.get("peak_rss_kb"), "label": "measured"},
        "nonempty_ticks": sum(1 for s in sets if s),
        "budget_p99_within_1ms": {
            "value": percentile(sorted(measured), 0.99) <= TICK_BUDGET_NS if measured else None,
            "label": "derived",
        },
    }
    return checks, metrics, sets


def check_transport(summary):
    metrics_text = summary.get("metrics_text") or ""
    lat = summary["latency_ns"]
    checks = {
        "T1_ready_and_rss": summary["ready_after_ns"] is not None
        and summary.get("daemon_peak_rss_kb") is not None,
        "T2_subscribed_and_primed": summary["subscribe_frames"] > 0 and summary["prime_attempts"] >= 1,
        "T3_all_pulses_matched": summary["matched"] == summary["pulses_sent"]
        and summary["mismatched"] == 0
        and summary["lost"] == 0
        and summary["extra_nonempty"] == 0,
        "T4_metrics_ready_not_fatal": summary.get("metrics_status") == 200
        and "brainstem_ready 1\n" in metrics_text
        and "brainstem_fatal 0\n" in metrics_text,
        "T5_sigterm_clean_exit": summary["exit_code"] == 0
        and summary["termination_logged"] is True
        and summary["sigterm_exit_ns"] <= SIGTERM_BUDGET_NS,
    }
    metrics = {
        "end_to_end_latency": {**lat, "label": "measured"},
        "pulses_sent": summary["pulses_sent"],
        "matched": summary["matched"],
        "mismatched": summary["mismatched"],
        "lost": summary["lost"],
        "extra_nonempty": summary["extra_nonempty"],
        "measured_frames_total": summary["measured_frames_total"],
        "measured_frames_empty": summary["measured_frames_empty"],
        "ready_after_ns": summary["ready_after_ns"],
        "prime_attempts": summary["prime_attempts"],
        "sigterm_exit_ns": summary["sigterm_exit_ns"],
        "daemon_peak_rss_kb": {"value": summary.get("daemon_peak_rss_kb"), "label": "measured"},
        "daemon_rss_kb": {"value": summary.get("daemon_rss_kb"), "label": "measured"},
        "budget_p99_within_5ms": {
            "value": (lat["p99"] is not None and lat["p99"] <= TRANSPORT_BUDGET_NS),
            "label": "derived",
        },
    }
    return checks, metrics


def all_true(checks):
    return all(v is True for k, v in checks.items() if not k.endswith("_first_mismatches"))


def fmt_ns(value):
    if value is None:
        return "n/a"
    if value >= 1_000_000:
        return f"{value / 1_000_000:.3f} ms"
    if value >= 1_000:
        return f"{value / 1_000:.2f} µs"
    return f"{value} ns"


def main():
    out = sys.argv[1]
    workload = load_json(HERE / "fixtures" / "workload.json")
    expected = load_json(HERE / "fixtures" / "expected_spikes.json")
    identity = load_json(os.path.join(out, "identity.json")) if os.path.exists(os.path.join(out, "identity.json")) else {}

    baseline_sets = None
    runs = {}
    for prefix in ("baseline", "daemon-live", "daemon-simulation"):
        for name, summary, records_path in run_files(out, prefix):
            records = load_records(records_path)
            checks, metrics, sets = check_inprocess(
                name, summary, records, workload, expected, baseline_sets
            )
            if prefix == "baseline":
                if baseline_sets is None:
                    baseline_sets = sets
                checks["B4_baseline_runs_identical"] = sets == baseline_sets
            runs[name] = {
                "mode": summary["mode"],
                "checkpoint": summary.get("checkpoint"),
                "checks": checks,
                "metrics": metrics,
                "all_checks_pass": all_true(checks),
            }
    transport = {}
    for name, summary, _records in run_files(out, "transport-live"):
        checks, metrics = check_transport(summary)
        transport[name] = {"checks": checks, "metrics": metrics, "all_checks_pass": all_true(checks)}

    live = [r for r in runs.values() if r["mode"] == "daemon-live"]
    sim = [r for r in runs.values() if r["mode"] == "daemon-simulation"]
    base = [r for r in runs.values() if r["mode"] == "baseline"]
    correctness = bool(live) and all(r["all_checks_pass"] for r in live + sim + base)
    transport_ok = bool(transport) and all(t["all_checks_pass"] for t in transport.values())
    budget_ok = bool(live) and all(r["metrics"]["budget_p99_within_1ms"]["value"] for r in live)
    transport_budget_ok = bool(transport) and all(
        t["metrics"]["budget_p99_within_5ms"]["value"] and t["metrics"]["lost"] == 0
        for t in transport.values()
    )
    if correctness and transport_ok and budget_ok and transport_budget_ok:
        decision = "supports_further_development"
    elif not live or not transport:
        decision = "blocked"
    else:
        decision = "change_of_direction"
    summary = {
        "sha": identity.get("sha"),
        "identity": identity,
        "repetitions": {
            "daemon-live": len(live),
            "baseline": len(base),
            "daemon-simulation": len(sim),
            "transport-live": len(transport),
        },
        "percentile_method": "nearest-rank: sorted[round((n-1)*p)]",
        "budgets": {
            "tick_p99_ns": TICK_BUDGET_NS,
            "transport_p99_ns": TRANSPORT_BUDGET_NS,
            "label": "pre-registered",
        },
        "runs": runs,
        "transport": transport,
        "decision": {
            "value": decision,
            "correctness_all_pass": correctness,
            "transport_all_pass": transport_ok,
            "tick_budget_all_pass": budget_ok,
            "transport_budget_all_pass": transport_budget_ok,
            "label": "derived",
        },
    }
    with open(os.path.join(out, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=2)
        fh.write("\n")

    lines = [f"# Usefulness experiment summary — `{identity.get('sha', '?')}`", ""]
    lines.append(f"Decision (derived): **{decision}**")
    lines.append("")
    lines.append(
        f"## In-process runs (latency after {workload['warmup_ticks']:,} warm-up ticks; measured)"
    )
    lines.append("")
    lines.append("| run | checks | p50 | p99 | max | decode p50 | ticks/s | peak RSS | nonempty ticks |")
    lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|")
    for name, r in runs.items():
        m = r["metrics"]
        lat = m["tick_latency"]
        dec = m["decode_latency"]
        lines.append(
            f"| {name} | {'pass' if r['all_checks_pass'] else 'FAIL'} | {fmt_ns(lat['p50_ns'])} | "
            f"{fmt_ns(lat['p99_ns'])} | {fmt_ns(lat['max_ns'])} | "
            f"{fmt_ns(dec['p50_ns']) if dec else '—'} | {m['ticks_per_s']['value']:,.0f} | "
            f"{m['peak_rss_kb']['value']} kB | {m['nonempty_ticks']} |"
        )
    lines.append("")
    lines.append("## Transport runs (measured)")
    lines.append("")
    lines.append("| run | checks | sent | matched | lost | e2e p50 | e2e p99 | e2e max | ready after | SIGTERM→exit | daemon peak RSS |")
    lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for name, t in transport.items():
        m = t["metrics"]
        lat = m["end_to_end_latency"]
        lines.append(
            f"| {name} | {'pass' if t['all_checks_pass'] else 'FAIL'} | {m['pulses_sent']} | {m['matched']} | "
            f"{m['lost']} | {fmt_ns(lat['p50'])} | {fmt_ns(lat['p99'])} | {fmt_ns(lat['max'])} | "
            f"{fmt_ns(m['ready_after_ns'])} | {fmt_ns(m['sigterm_exit_ns'])} | {m['daemon_peak_rss_kb']['value']} kB |"
        )
    lines.append("")
    lines.append("## Per-run checks")
    lines.append("")
    for name, r in list(runs.items()) + list(transport.items()):
        failed = [k for k, v in r["checks"].items() if v is False]
        lines.append(f"- `{name}`: " + ("all pass" if not failed else "FAILED " + ", ".join(failed)))
    lines.append("")
    with open(os.path.join(out, "summary.md"), "w") as fh:
        fh.write("\n".join(lines))
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
