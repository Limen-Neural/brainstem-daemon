#!/usr/bin/env python3
"""
Turn raw usefulness-experiment results into the pre-registered checks and metrics.

See PROTOCOL.md §4–§6. Standard library only.

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
MODES = ("baseline", "daemon-live", "daemon-simulation")
CLASSES = ("sensory", "reward", "control", "telemetry")
# run.sh writes these into identity.json after this script has run, so they
# are left out of the embedded copy to keep summary.json reproducible.
VOLATILE_IDENTITY = ("status", "finished_utc")
INPROCESS_HEADER = "| run | checks | p50 | p99 | max | decode p50 | ticks/s | peak RSS | nonempty ticks |"
INPROCESS_RULE = "|---|---|---:|---:|---:|---:|---:|---:|---:|"
TRANSPORT_HEADER = (
    "| run | checks | sent | matched | lost | e2e p50 | e2e p99 | e2e max | "
    "ready after | SIGTERM→exit | daemon peak RSS |"
)
TRANSPORT_RULE = "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"


def load_json(path):
    """Parse one JSON file."""
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def load_records(path):
    """Parse a JSONL file, gzipped or plain."""
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def load_identity(out):
    """identity.json without the fields run.sh finalizes after summarizing."""
    path = os.path.join(out, "identity.json")
    if not os.path.exists(path):
        return {}
    return {k: v for k, v in load_json(path).items() if k not in VOLATILE_IDENTITY}


def percentile(sorted_values, p):
    """Nearest-rank on a sorted list: index round((n - 1) * p)."""
    if not sorted_values:
        return None
    return sorted_values[round((len(sorted_values) - 1) * p)]


def latency_stats(values):
    """p50 / p99 / max / min / mean of a latency sample, labelled measured."""
    values = sorted(values)
    if not values:
        return {"count": 0, "p50_ns": None, "p99_ns": None, "max_ns": None, "min_ns": None, "mean_ns": None, "label": "measured"}
    return {
        "count": len(values),
        "p50_ns": percentile(values, 0.50),
        "p99_ns": percentile(values, 0.99),
        "max_ns": values[-1],
        "min_ns": values[0],
        "mean_ns": sum(values) / len(values),
        "label": "measured",
    }


def within_budget(p99, budget):
    """Derived budget comparison; None when there is no sample."""
    if p99 is None:
        return None
    return p99 <= budget


def run_files(out, prefix):
    """(name, summary, records path) for every `<prefix>-r*` run in `out`."""
    runs = []
    for summary_path in sorted(glob.glob(os.path.join(out, f"{prefix}-r*.summary.json"))):
        stem = summary_path[: -len(".summary.json")]
        records = stem + ".jsonl.gz"
        if not os.path.exists(records):
            records = stem + ".jsonl"
        runs.append((os.path.basename(stem), load_json(summary_path), records))
    return runs


def channel_sets(records):
    """Per-tick spike channel sets as tuples."""
    return [tuple(r["channels"]) for r in records]


def reference_sets(expected, ticks):
    """Per-tick expected sets from expected_spikes.json (empty when absent)."""
    nonempty = expected["nonempty_ticks"]
    return [tuple(nonempty.get(str(t), [])) for t in range(ticks)]


def event_ticks(workload):
    """Every tick that carries a workload event, over all cycles."""
    per_cycle = [e["offset"] for e in workload["cycle_events"]]
    return {c * workload["ticks_per_cycle"] + o for c in range(workload["cycles"]) for o in per_cycle}


def first_mismatches(sets, reference, limit=5):
    """The first ticks where the run and the reference disagree."""
    pairs = enumerate(zip(sets, reference))
    bad = [{"tick": t, "got": list(a), "expected": list(b)} for t, (a, b) in pairs if a != b]
    return bad[:limit]


def check_accounting(stats, frames):
    """C2: every stimulus frame accepted, nothing rejected, dropped or failed."""
    return all([
        stats.get("accepted_batches") == frames,
        stats.get("rejected_batches") == 0,
        stats.get("receive_errors") == 0,
        stats.get("emit_errors") == 0,
        stats.get("dropped_spikes") == 0,
    ])


def check_health(health):
    """C3: ready, not fatal, not draining after the run."""
    return all([health["ready"] is True, health["fatal"] is None, health["phase"] not in ("fatal", "draining")])


def check_ingress(ingress, frames, modulator_frames):
    """C9: class queues accepted exactly the frames sent and lost nothing."""
    losses = [ingress[c][k] for c in CLASSES for k in ("rejected", "dropped")]
    return all([
        ingress["sensory"]["accepted"] == frames,
        ingress["reward"]["accepted"] == modulator_frames,
        all(v == 0 for v in losses),
    ])


def daemon_checks(summary, records, ticks):
    """C1, C2, C3 and C9 for a daemon-mode run."""
    stats = summary["runtime_stats"]
    return {
        "C1_ticks": all([stats.get("ticks") == ticks, len(records) == ticks]),
        "C2_accounting": check_accounting(stats, summary["stimulus_frames_sent"]),
        "C3_health": check_health(summary["health"]),
        "C9_ingress": check_ingress(summary["ingress"], summary["stimulus_frames_sent"], summary["modulator_frames_sent"]),
    }


def check_mask_equivalence(sets, workload):
    """C6: the masked segment equals the zero-equivalent segment in every cycle."""
    tpc = workload["ticks_per_cycle"]
    for cycle in range(workload["cycles"]):
        base = cycle * tpc
        if sets[base + MASKED[0] : base + MASKED[1]] != sets[base + ZERO[0] : base + ZERO[1]]:
            return False
    return True


def idle_mismatch(tick, got, want, events):
    """True when an idle tick (no workload event) differs from the reference."""
    return all([tick not in events, got != want])


def check_idle_ticks(sets, reference, workload):
    """C7: idle ticks emit exactly what the reference predicts."""
    events = event_ticks(workload)
    pairs = enumerate(zip(sets, reference))
    return not any(idle_mismatch(t, got, want, events) for t, (got, want) in pairs)


REFERENCE_CHECK = {"C": "C5_matches_reference", "B": "B5_baseline_matches_reference"}


def live_checks(prefix, sets, reference, workload):
    """Reference equality, idle ticks and mask equivalence (C* for daemon, B* for baseline)."""
    key = REFERENCE_CHECK[prefix]
    checks = {
        key: sets == reference,
        f"{prefix}7_idle_ticks": check_idle_ticks(sets, reference, workload),
        f"{prefix}6_mask_equivalence": check_mask_equivalence(sets, workload),
    }
    mismatches = first_mismatches(sets, reference)
    if mismatches:
        checks[key + "_first_mismatches"] = mismatches
    return checks


def inprocess_checks(summary, records, sets, ctx):
    """All pre-registered in-process checks for one run, keyed by check id."""
    mode = summary["mode"]
    if mode == "baseline":
        checks = {"C1_ticks": len(records) == ctx["ticks"]}
        checks.update(live_checks("B", sets, ctx["reference"], ctx["workload"]))
        checks["B4_baseline_runs_identical"] = sets == ctx["baseline_sets"]
        return checks
    checks = daemon_checks(summary, records, ctx["ticks"])
    if mode == "daemon-simulation":
        checks["C8_simulation_no_spikes"] = all([len(sets) == ctx["ticks"], not any(sets)])
        return checks
    checks.update(live_checks("C", sets, ctx["reference"], ctx["workload"]))
    checks["C4_equals_baseline"] = sets == ctx["baseline_sets"]
    return checks


def inprocess_metrics(summary, records, sets, warmup):
    """Latency, decode, throughput and memory for one in-process run."""
    measured = [r["latency_ns"] for r in records[warmup:]]
    decode = [r["decode_ns"] for r in records if "decode_ns" in r]
    tick_latency = latency_stats(measured)
    return {
        "tick_latency": tick_latency,
        "decode_latency": latency_stats(decode) if decode else None,
        "ticks_per_s": {"value": summary["timing"]["ticks_per_s"], "label": "measured"},
        "measured_wall_ns": summary["timing"]["measured_wall_ns"],
        "run_wall_ns": summary["run_wall_ns"],
        "peak_rss_kb": {"value": summary.get("peak_rss_kb"), "label": "measured"},
        "nonempty_ticks": sum(1 for s in sets if s),
        "budget_p99_within_1ms": {"value": within_budget(tick_latency["p99_ns"], TICK_BUDGET_NS), "label": "derived"},
    }


def all_true(checks):
    """Every check value is literally True (mismatch listings are informational)."""
    return all(v is True for k, v in checks.items() if not k.endswith("_first_mismatches"))


def analyze_inprocess(summary, records_path, ctx):
    """Checks and metrics for one in-process run; the first baseline run becomes the comparator."""
    records = load_records(records_path)
    sets = channel_sets(records)
    if summary["mode"] == "baseline" and ctx["baseline_sets"] is None:
        ctx["baseline_sets"] = sets
    checks = inprocess_checks(summary, records, sets, ctx)
    return {
        "mode": summary["mode"],
        "checkpoint": summary.get("checkpoint"),
        "checks": checks,
        "metrics": inprocess_metrics(summary, records, sets, ctx["workload"]["warmup_ticks"]),
        "all_checks_pass": all_true(checks),
    }


def load_mode_runs(out, prefix, ctx):
    """Analyze every repetition of one in-process mode."""
    runs = {}
    for name, summary, records_path in run_files(out, prefix):
        runs[name] = analyze_inprocess(summary, records_path, ctx)
    return runs


def load_inprocess_runs(out, ctx):
    """Analyze all in-process modes, baseline first so C4 has its comparator."""
    runs = {}
    for prefix in MODES:
        runs.update(load_mode_runs(out, prefix, ctx))
    return runs


def transport_checks(summary):
    """T1–T5 for one transport run."""
    metrics_text = summary.get("metrics_text") or ""
    return {
        "T1_ready_and_rss": all([summary["ready_after_ns"] is not None, summary.get("daemon_peak_rss_kb") is not None]),
        "T2_subscribed_and_primed": all([summary["subscribe_frames"] > 0, summary["prime_attempts"] >= 1]),
        "T3_all_pulses_matched": all([
            summary["matched"] == summary["pulses_sent"],
            summary["mismatched"] == 0,
            summary["lost"] == 0,
            summary["extra_nonempty"] == 0,
        ]),
        "T4_metrics_ready_not_fatal": all([
            summary.get("metrics_status") == 200,
            "brainstem_ready 1\n" in metrics_text,
            "brainstem_fatal 0\n" in metrics_text,
        ]),
        "T5_sigterm_clean_exit": all([
            summary["exit_code"] == 0,
            summary["termination_logged"] is True,
            summary["sigterm_exit_ns"] <= SIGTERM_BUDGET_NS,
        ]),
    }


def transport_metrics(summary):
    """Delivery counts, end-to-end latency, lifecycle timings and memory for one transport run."""
    lat = summary["latency_ns"]
    return {
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
        "budget_p99_within_5ms": {"value": within_budget(lat["p99"], TRANSPORT_BUDGET_NS) is True, "label": "derived"},
    }


def load_transport_runs(out):
    """Checks and metrics for every transport repetition."""
    runs = {}
    for name, summary, _records in run_files(out, "transport-live"):
        checks = transport_checks(summary)
        runs[name] = {"checks": checks, "metrics": transport_metrics(summary), "all_checks_pass": all_true(checks)}
    return runs


def transport_budget_ok(run):
    """Transport budget: p99 within 5 ms and nothing lost."""
    metrics = run["metrics"]
    return all([metrics["budget_p99_within_5ms"]["value"], metrics["lost"] == 0])


def decision_value(flags, have_live, have_transport):
    """Apply the pre-registered decision rule (PROTOCOL.md §6)."""
    if all(flags.values()):
        return "supports_further_development"
    if not all([have_live, have_transport]):
        return "blocked"
    return "change_of_direction"


def decide(runs, transport):
    """Decision block: the four gate flags and the rule's outcome."""
    live = [r for r in runs.values() if r["mode"] == "daemon-live"]
    # Every in-process mode must be present; a missing group cannot pass vacuously.
    groups_present = all(count_mode(runs, mode) > 0 for mode in MODES)
    flags = {
        "correctness_all_pass": all([groups_present, all(r["all_checks_pass"] for r in runs.values())]),
        "transport_all_pass": all([bool(transport), all(t["all_checks_pass"] for t in transport.values())]),
        "tick_budget_all_pass": all([bool(live), all(r["metrics"]["budget_p99_within_1ms"]["value"] for r in live)]),
        "transport_budget_all_pass": all([bool(transport), all(transport_budget_ok(t) for t in transport.values())]),
    }
    value = decision_value(flags, groups_present, bool(transport))
    return {"value": value, **flags, "label": "derived"}


def count_mode(runs, mode):
    """Number of analyzed runs in `mode`."""
    return sum(1 for r in runs.values() if r["mode"] == mode)


def build_summary(out, runs, transport):
    """The summary.json document."""
    identity = load_identity(out)
    return {
        "sha": identity.get("sha"),
        "identity": identity,
        "repetitions": {
            "daemon-live": count_mode(runs, "daemon-live"),
            "baseline": count_mode(runs, "baseline"),
            "daemon-simulation": count_mode(runs, "daemon-simulation"),
            "transport-live": len(transport),
        },
        "percentile_method": "nearest-rank: sorted[round((n-1)*p)]",
        "budgets": {"tick_p99_ns": TICK_BUDGET_NS, "transport_p99_ns": TRANSPORT_BUDGET_NS, "label": "pre-registered"},
        "runs": runs,
        "transport": transport,
        "decision": decide(runs, transport),
    }


def fmt_ns(value):
    """Human-readable nanoseconds."""
    if value is None:
        return "n/a"
    if value >= 1_000_000:
        return f"{value / 1_000_000:.3f} ms"
    if value >= 1_000:
        return f"{value / 1_000:.2f} µs"
    return f"{value} ns"


def pass_label(run):
    """`pass` or `FAIL` for a run's check set."""
    return "pass" if run["all_checks_pass"] else "FAIL"


def inprocess_row(name, run):
    """One Markdown table row for an in-process run."""
    metrics = run["metrics"]
    lat = metrics["tick_latency"]
    decode = metrics["decode_latency"]
    decode_p50 = fmt_ns(decode["p50_ns"]) if decode else "—"
    return (
        f"| {name} | {pass_label(run)} | {fmt_ns(lat['p50_ns'])} | {fmt_ns(lat['p99_ns'])} | "
        f"{fmt_ns(lat['max_ns'])} | {decode_p50} | {metrics['ticks_per_s']['value']:,.0f} | "
        f"{metrics['peak_rss_kb']['value']} kB | {metrics['nonempty_ticks']} |"
    )


def transport_row(name, run):
    """One Markdown table row for a transport run."""
    metrics = run["metrics"]
    lat = metrics["end_to_end_latency"]
    return (
        f"| {name} | {pass_label(run)} | {metrics['pulses_sent']} | {metrics['matched']} | "
        f"{metrics['lost']} | {fmt_ns(lat['p50'])} | {fmt_ns(lat['p99'])} | {fmt_ns(lat['max'])} | "
        f"{fmt_ns(metrics['ready_after_ns'])} | {fmt_ns(metrics['sigterm_exit_ns'])} | "
        f"{metrics['daemon_peak_rss_kb']['value']} kB |"
    )


def checks_line(name, run):
    """One bullet naming the failed checks of a run, or `all pass`."""
    failed = [k for k, v in run["checks"].items() if v is False]
    if not failed:
        return f"- `{name}`: all pass"
    return f"- `{name}`: FAILED " + ", ".join(failed)


def render_markdown(summary, warmup):
    """summary.md: decision, the two result tables, and per-run check status."""
    runs = summary["runs"]
    transport = summary["transport"]
    lines = [f"# Usefulness experiment summary — `{summary['identity'].get('sha', '?')}`", ""]
    lines += [f"Decision (derived): **{summary['decision']['value']}**", ""]
    lines += [f"## In-process runs (latency after {warmup:,} warm-up ticks; measured)", ""]
    lines += [INPROCESS_HEADER, INPROCESS_RULE]
    lines += [inprocess_row(name, run) for name, run in runs.items()]
    lines += ["", "## Transport runs (measured)", "", TRANSPORT_HEADER, TRANSPORT_RULE]
    lines += [transport_row(name, run) for name, run in transport.items()]
    lines += ["", "## Per-run checks", ""]
    lines += [checks_line(name, run) for name, run in list(runs.items()) + list(transport.items())]
    lines.append("")
    return "\n".join(lines)


def write_outputs(out, summary, markdown):
    """Write summary.json and summary.md, and echo the Markdown."""
    with open(os.path.join(out, "summary.json"), "w", encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2)
        handle.write("\n")
    with open(os.path.join(out, "summary.md"), "w", encoding="utf-8") as handle:
        handle.write(markdown)
    print(markdown)


def main():
    """Summarize the results directory given on the command line."""
    out = sys.argv[1]
    workload = load_json(HERE / "fixtures" / "workload.json")
    expected = load_json(HERE / "fixtures" / "expected_spikes.json")
    ctx = {
        "workload": workload,
        "ticks": workload["ticks"],
        "reference": reference_sets(expected, workload["ticks"]),
        "baseline_sets": None,
    }
    runs = load_inprocess_runs(out, ctx)
    transport = load_transport_runs(out)
    summary = build_summary(out, runs, transport)
    write_outputs(out, summary, render_markdown(summary, workload["warmup_ticks"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
