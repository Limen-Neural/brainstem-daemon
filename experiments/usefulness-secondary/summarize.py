#!/usr/bin/env python3
"""Summarize secondary usefulness-experiment raw results against PROTOCOL.md."""

# Standard library only. Usage: summarize.py experiments/usefulness-secondary/results/<sha>
#
# Writes summary.json and summary.md into that directory. Every number is
# measured from raw files unless its key or label says "derived". Exits 0 when
# the summary was produced, whether checks pass or fail (a negative result is
# valid), and nonzero only when raw inputs are missing or malformed.
#
# A check that no run evaluated fails ("not evaluated"); check I1 fails when the
# pre-registered run inventory (5 reps per in-process family, 3 transport reps)
# is incomplete, so missing evidence can never read as a pass.

import bisect
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORKLOAD = os.path.join(HERE, "fixtures", "workload.json")
MANIFEST = os.path.join(HERE, "fixtures", "fixture-manifest.json")
SUMMARY_SUFFIX = ".summary.json"
USAGE = "usage: summarize.py experiments/usefulness-secondary/results/<sha>"
# Result field holding a check's pass/fail flag in summary.json.
RESULT_FIELD = "pass"

TICK_BUDGET_NS = 1_000_000  # 1 kHz
MIN_TICKS_PER_S = 1_000
TRANSPORT_P99_BUDGET_NS = 3_000_000
IN_PROCESS_REPS = 5
TRANSPORT_REPS = 3
REQUIRED_FAMILIES = (
    ("baseline", "main"),
    ("daemon-live", "main"),
    ("daemon-live", "zeroed"),
    ("daemon-live", "unmasked"),
    ("daemon-simulation", "main"),
)
DRIVEN_SEGMENTS = ("single", "pair", "masked")
MODULATED_NEURONS = {12, 13}
MASKED_NEURONS = {4, 5}

OK_TEXT = {
    "I1": "pre-registered run inventory complete (5 reps x 5 in-process families, 3 transport reps)",
    "C1": "ticks == accepted == N, zero rejected/errors/dropped in every daemon run",
    "C2": "sensory accepted == N, reward accepted == modulated ticks, no class loss",
    "C3": "post-run health not fatal",
    "C4": "daemon-live raster == baseline raster in every repetition",
    "C5": "all repetitions of each mode/variant identical",
    "C6": "driven ticks fire exactly the mapped neurons",
    "C7": "every spike within the quiet window of its channel's input",
    "C8": "no spikes in quiet periods",
    "C9": "zeroed raster == main raster",
    "C10": "unmasked raster differs; neurons 4,5 fire",
    "C11": "modulated segment fires on fewer than 10/10 ticks in every cycle",
    "C12": "simulation control emits zero spikes",
    "T1": "SIGTERM exit 0 within 2 s with termination log, every rep",
    "T2": "every probe matched exactly once, no unexpected nonempty batches",
    "T3": f"probe p99 <= {TRANSPORT_P99_BUDGET_NS} ns in every rep",
    "P1": f"daemon-live p99 <= {TICK_BUDGET_NS} ns in every run",
    "P2": f"daemon-live >= {MIN_TICKS_PER_S} ticks/s in every run",
}


def load_json(path):
    """Read one JSON document."""
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def load_jsonl(path):
    """Read one JSON record per nonblank line."""
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def percentile(values, q):
    """Return the nearest-rank value at rank q of 100, or None when empty."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(q / 100.0 * len(ordered)))
    return ordered[rank - 1]


def latency_stats(values):
    """Return n, p50, p99, and max of a latency sample in ns."""
    return {
        "n": len(values),
        "p50_ns": percentile(values, 50),
        "p99_ns": percentile(values, 99),
        "max_ns": max(values) if values else None,
    }


class Checks:
    """Accumulate pass/fail evidence per pre-registered check."""

    def __init__(self):
        """Start with no evaluated checks."""
        self.items = {}

    def add(self, key, ok, detail):
        """Record one evaluation of `key`; keep the detail only on failure."""
        item = self.items.setdefault(key, {RESULT_FIELD: True, "details": []})
        if not ok:
            item[RESULT_FIELD] = False
            item["details"].append(detail)

    def result(self, key):
        """Return the final result; an unevaluated check fails."""
        item = self.items.get(key)
        if item is None:
            return {RESULT_FIELD: False, "details": ["not evaluated: required runs missing"]}
        return {RESULT_FIELD: item[RESULT_FIELD], "details": item["details"][:10] or [OK_TEXT[key]]}


# ── Workload re-expansion (independent of the Rust drivers) ──────────────────


def segment_inputs(seg, variant):
    """Return (values, mask) for one segment under `variant`."""
    values = list(seg["values"])
    mask = seg.get("valid_mask")
    if variant == "unmasked" or mask is None:
        return values, None
    if variant == "zeroed":
        return [v if ok else 0.0 for v, ok in zip(values, mask)], None
    return values, mask


def effective_values(values, mask):
    """Apply the mask: masked slots become 0.0."""
    if mask is None:
        return values
    return [v if ok else 0.0 for v, ok in zip(values, mask)]


def expand(workload, variant):
    """Return per-tick (segment, effective_values, has_modulators)."""
    cycle = []
    for seg in workload["segments"]:
        effective = effective_values(*segment_inputs(seg, variant))
        modulated = seg.get("neuromodulators") is not None
        cycle.extend([(seg["name"], effective, modulated)] * seg["ticks"])
    return cycle * workload["cycles"]


def input_history(ticks, channels):
    """Return per tick (last input tick per channel, last tick with any input)."""
    last = [None] * channels
    last_any = None
    history = []
    for t, (_, effective, _) in enumerate(ticks):
        active = [c for c, v in enumerate(effective) if v != 0.0]
        for c in active:
            last[c] = t
        last_any = t if active else last_any
        history.append((list(last), last_any))
    return history


def age(t, last):
    """Return ticks since `last`, or infinity when there was never input."""
    return math.inf if last is None else t - last


class Expectations:
    """Fixture-derived expectations for the main workload."""

    def __init__(self, workload, manifest):
        """Precompute the main workload and its input history."""
        self.ticks = expand(workload, "main")
        self.history = input_history(self.ticks, workload["channels"])
        self.quiet = workload["quiet_window_ticks"]
        self.drive = workload["drive_threshold"]
        self.cycle_len = sum(s["ticks"] for s in workload["segments"])
        self.neurons = {int(c): set(ns) for c, ns in manifest["channel_to_neurons"].items()}
        self.channel_of = {n: c for c, ns in self.neurons.items() for n in ns}

    def driven_neurons(self, effective):
        """Return neurons of channels at or above the drive threshold."""
        out = set()
        for c, v in enumerate(effective):
            out |= self.neurons[c] if abs(v) >= self.drive else set()
        return out

    def modulated_ticks(self):
        """Return the count of ticks carrying neuromodulators."""
        return sum(1 for t in self.ticks if t[2])


# ── In-process runs ───────────────────────────────────────────────────────────


def load_run(raw, name):
    """Load one in-process run summary plus its per-tick records."""
    stem = name[: -len(SUMMARY_SUFFIX)]
    records = load_jsonl(os.path.join(raw, stem + ".jsonl"))
    return {
        "stem": stem,
        "summary": load_json(os.path.join(raw, name)),
        "raster": [tuple(r["channels"]) for r in records],
        "latency": [r["latency_ns"] for r in records],
    }


def load_inprocess_runs(raw):
    """Load every non-transport run in `raw`."""
    names = sorted(os.listdir(raw))
    return [load_run(raw, n) for n in names if n.endswith(SUMMARY_SUFFIX) and not n.startswith("transport-")]


def family(run):
    """Return (mode, variant) of a run."""
    return run["summary"]["mode"], run["summary"]["variant"]


def rasters(runs, mode, variant):
    """Map rep -> raster for one mode/variant family."""
    return {r["summary"]["rep"]: r["raster"] for r in runs if family(r) == (mode, variant)}


def stats_mismatches(summary, n_ticks):
    """Return runtime counters that differ from the C1 expectations."""
    stats = dict(summary["details"]["runtime_stats"], ticks_recorded=summary["ticks_recorded"])
    zero = ("rejected_batches", "receive_errors", "emit_errors", "dropped_spikes")
    expected = dict({k: 0 for k in zero}, ticks=n_ticks, accepted_batches=n_ticks, ticks_recorded=n_ticks)
    return {k: stats[k] for k, v in expected.items() if stats[k] != v}


def ingress_ok(ingress, n_ticks, n_modulated):
    """Return whether C2 ingress counters hold."""
    no_loss = all(m["lost_or_coalesced"] == 0 for m in ingress.values())
    counts = (ingress["sensory"]["accepted"], ingress["reward"]["accepted"])
    return no_loss and counts == (n_ticks, n_modulated)


def check_accounting(checks, run, expect):
    """Check C1–C3 for one daemon run."""
    summary = run["summary"]
    n_ticks = len(expect.ticks)
    mismatched = stats_mismatches(summary, n_ticks)
    checks.add("C1", not mismatched, f"{run['stem']}: {mismatched}")
    ingress = summary["details"]["ingress"]
    checks.add("C2", ingress_ok(ingress, n_ticks, expect.modulated_ticks()), f"{run['stem']}: {ingress}")
    health = summary["details"]["health"]
    healthy = health["phase"] != "fatal" and health["fatal"] is None
    checks.add("C3", healthy, f"{run['stem']}: phase={health['phase']} fatal={health['fatal']}")


def check_inventory(checks, runs, transport_reps):
    """Check I1: every pre-registered run family has every repetition."""
    wanted = list(range(IN_PROCESS_REPS))
    for mode, variant in REQUIRED_FAMILIES:
        reps = sorted(rasters(runs, mode, variant))
        checks.add("I1", reps == wanted, f"{mode}/{variant}: reps {reps}, expected {wanted}")
    checks.add("I1", transport_reps == TRANSPORT_REPS, f"transport: {transport_reps} reps, expected {TRANSPORT_REPS}")


def check_baseline_equality(checks, live, base):
    """Check C4: daemon-live main equals baseline main per repetition."""
    for rep, raster in sorted(live.items()):
        other = base.get(rep, [])
        diff = [i for i, (a, b) in enumerate(zip(raster, other)) if a != b]
        same = not diff and len(raster) == len(other)
        detail = f"rep {rep}: {len(diff)} differing ticks, first {diff[:5]}, lengths {len(raster)}/{len(other)}"
        checks.add("C4", same, detail)


def check_determinism(checks, runs):
    """Check C5: all repetitions within each family are identical."""
    for mode, variant in sorted({family(r) for r in runs}):
        by_rep = rasters(runs, mode, variant)
        first = by_rep[min(by_rep)]
        for rep in sorted(by_rep):
            checks.add("C5", by_rep[rep] == first, f"{mode}/{variant}: rep {rep} differs from rep {min(by_rep)}")


def check_tick(checks, expect, t, spikes):
    """Check C6–C8 for one tick of a daemon-live main raster."""
    segment, effective, _ = expect.ticks[t]
    last, last_any = expect.history[t]
    spikes = set(spikes)
    if segment in DRIVEN_SEGMENTS:
        wanted = expect.driven_neurons(effective)
        checks.add("C6", spikes == wanted, f"tick {t} ({segment}): got {sorted(spikes)} expected {sorted(wanted)}")
    stray = sorted(n for n in spikes if age(t, last[expect.channel_of[n]]) > expect.quiet)
    checks.add("C7", not stray, f"tick {t}: neurons {stray} without input in {expect.quiet} ticks")
    if age(t, last_any) >= expect.quiet:
        checks.add("C8", not spikes, f"tick {t}: spikes {sorted(spikes)} in quiet period")


def check_expectations(checks, expect, live):
    """Check C6–C8 over every daemon-live main repetition."""
    for raster in live.values():
        for t, spikes in enumerate(raster):
            check_tick(checks, expect, t, spikes)


def check_masks(checks, expect, runs):
    """Check C9 mask equivalence and C10 mask sensitivity."""
    live = rasters(runs, "daemon-live", "main")
    zeroed = rasters(runs, "daemon-live", "zeroed")
    unmasked = rasters(runs, "daemon-live", "unmasked")
    masked_ticks = [t for t, tick in enumerate(expect.ticks) if tick[0] == "masked"]
    for rep, raster in sorted(live.items()):
        checks.add("C9", zeroed.get(rep) == raster, f"rep {rep}: zeroed raster differs from main")
        um = unmasked.get(rep, [])
        fired = any(MASKED_NEURONS <= set(um[t]) for t in masked_ticks if t < len(um))
        sensitive = bool(um) and um != raster and fired
        checks.add("C10", sensitive, f"rep {rep}: differs={um != raster} neurons 4,5 fired={fired}")


def modulated_fire_counts(expect, raster):
    """Count, per cycle, modulated ticks on which neurons 12 and 13 fired."""
    counts = []
    for k in range(len(expect.ticks) // expect.cycle_len):
        window = range(k * expect.cycle_len, (k + 1) * expect.cycle_len)
        ticks = [t for t in window if expect.ticks[t][0] == "modulated"]
        counts.append((sum(1 for t in ticks if MODULATED_NEURONS <= set(raster[t])), len(ticks)))
    return counts


def check_modulation(checks, expect, live):
    """Check C11: the modulated segment never fires on all of its ticks."""
    observed = []
    for rep, raster in sorted(live.items()):
        for cycle, (fired, total) in enumerate(modulated_fire_counts(expect, raster)):
            observed.append(fired)
            checks.add("C11", fired < total, f"rep {rep} cycle {cycle}: {fired}/{total} modulated ticks fired")
    return {"min": min(observed, default=None), "max": max(observed, default=None)}


def check_simulation(checks, runs):
    """Check C12: the simulation control emits zero spikes."""
    for rep, raster in sorted(rasters(runs, "daemon-simulation", "main").items()):
        total = sum(len(s) for s in raster)
        checks.add("C12", total == 0, f"daemon-simulation rep {rep}: {total} spikes")


def run_stats(run, warm):
    """Collect measured per-run latency, throughput, and memory."""
    summary = run["summary"]
    return {
        "run": run["stem"],
        "mode": summary["mode"],
        "variant": summary["variant"],
        "rep": summary["rep"],
        "latency_post_warmup": latency_stats(run["latency"][warm:]),
        "ticks_per_s": summary["ticks_per_s"],
        "vm_hwm_kb": summary["vm_hwm_kb"],
        "spike_ticks": sum(1 for s in run["raster"] if s),
    }


def check_performance(checks, per_run):
    """Check P1 and P2 on every daemon-live run."""
    for r in (r for r in per_run if r["mode"] == "daemon-live"):
        p99 = r["latency_post_warmup"]["p99_ns"]
        checks.add("P1", p99 is not None and p99 <= TICK_BUDGET_NS, f"{r['run']}: p99 {p99} ns")
        checks.add("P2", r["ticks_per_s"] >= MIN_TICKS_PER_S, f"{r['run']}: {r['ticks_per_s']:.0f} ticks/s")


def derived_overhead(per_run):
    """Derive daemon-live main minus baseline main overhead, matching reps."""
    by_key = {(r["mode"], r["variant"], r["rep"]): r for r in per_run}
    out = []
    for rep in sorted(k[2] for k in by_key if k[:2] == ("daemon-live", "main")):
        daemon, base = by_key[("daemon-live", "main", rep)], by_key.get(("baseline", "main", rep))
        if base is None:
            continue
        dl, bl = daemon["latency_post_warmup"], base["latency_post_warmup"]
        out.append({
            "rep": rep,
            "derived_p50_overhead_ns": dl["p50_ns"] - bl["p50_ns"],
            "derived_p99_overhead_ns": dl["p99_ns"] - bl["p99_ns"],
            "derived_throughput_ratio": daemon["ticks_per_s"] / base["ticks_per_s"],
        })
    return out


def summarize_inprocess(runs, workload, manifest, checks):
    """Evaluate C1–C12, P1–P2 and collect in-process metrics."""
    expect = Expectations(workload, manifest)
    warm = workload["warmup_cycles"] * expect.cycle_len
    for run in runs:
        if run["summary"]["mode"] != "baseline":
            check_accounting(checks, run, expect)
    live = rasters(runs, "daemon-live", "main")
    check_baseline_equality(checks, live, rasters(runs, "baseline", "main"))
    check_determinism(checks, runs)
    check_expectations(checks, expect, live)
    check_masks(checks, expect, runs)
    fire_counts = check_modulation(checks, expect, live)
    check_simulation(checks, runs)
    per_run = [run_stats(r, warm) for r in sorted(runs, key=lambda r: r["stem"])]
    check_performance(checks, per_run)
    return {
        "ticks_per_run": len(expect.ticks),
        "warmup_ticks_excluded": warm,
        "modulated_ticks_per_run": expect.modulated_ticks(),
        "modulated_fire_counts_per_cycle": fire_counts,
        "runs": per_run,
        "derived_overhead_vs_baseline": derived_overhead(per_run),
    }


# ── Transport runs ────────────────────────────────────────────────────────────


def assign_to_probes(sends, batches, end_ns):
    """Bucket nonempty batches into probe windows by receive time."""
    starts = [p["send_ns"] for p in sends]
    ends = starts[1:] + [end_ns]
    windows = [[] for _ in sends]
    for batch in batches:
        i = bisect.bisect_right(starts, batch["recv_ns"]) - 1
        if 0 <= i and batch["recv_ns"] < ends[i]:
            windows[i].append(batch)
    return windows


def match_probes(sends, batches, end_ns, manifest):
    """Return (latencies of matched probes, failing probes)."""
    latencies, bad = [], []
    for probe, window in zip(sends, assign_to_probes(sends, batches, end_ns)):
        expected = sorted(manifest["channel_to_neurons"][str(probe["channel"])])
        if len(window) == 1 and window[0]["channels"] == expected:
            latencies.append(window[0]["recv_ns"] - probe["send_ns"])
        else:
            bad.append({"probe": probe["probe"], "batches": [b["channels"] for b in window]})
    return latencies, bad


def batch_id_steps(ids):
    """Count consecutive batch_id steps; batch_id is the tick wall-clock ms."""
    diffs = [b - a for a, b in zip(ids, ids[1:])]
    gaps = [d for d in diffs if d > 1]
    return {
        "same_ms": diffs.count(0),
        "one_ms": diffs.count(1),
        "gap_gt_1ms": len(gaps),
        "missing_ms_in_gaps": sum(d - 1 for d in gaps),
    }


def load_transport(raw, name):
    """Load one transport repetition: summary, sends, measured receives."""
    stem = name[: -len(SUMMARY_SUFFIX)]
    recv = load_jsonl(os.path.join(raw, stem + ".recv.jsonl"))
    return {
        "stem": stem,
        "summary": load_json(os.path.join(raw, name)),
        "sends": load_jsonl(os.path.join(raw, stem + ".sends.jsonl")),
        "recv": [r for r in recv if r["phase"] == "measure"],
    }


def transport_rep(rep, workload, manifest):
    """Collect measured metrics for one transport repetition."""
    s, recv = rep["summary"], rep["recv"]
    nonempty = [r for r in recv if r["channels"]]
    latencies, bad = match_probes(rep["sends"], nonempty, s["measure_end_ns"], manifest)
    span_ms = (s["measure_end_ns"] - s["measure_start_ns"]) / 1e6
    expected_ticks = span_ms * workload["transport"]["tick_rate_hz"] / 1000.0
    keep = ("daemon_vm_hwm_kb", "ready_ms", "priming_frames", "exit_status_code", "exit_ms",
            "logged_termination", "undecodable_frames")
    return dict(
        {k: s[k] for k in keep},
        run=rep["stem"],
        probes_sent=len(rep["sends"]),
        probes_matched=len(latencies),
        bad_probes=bad[:10],
        unexpected_nonempty_batches=len(nonempty) - len(latencies),
        latencies=latencies,
        probe_latency=latency_stats(latencies),
        batches_received_measure=len(recv),
        measure_span_ms=span_ms,
        derived_batches_per_expected_tick=len(recv) / expected_ticks,
        batch_id_step_counts=batch_id_steps([r["batch_id"] for r in recv]),
    )


def check_transport_rep(checks, rep, s):
    """Check T1–T3 for one transport repetition."""
    lifecycle = s["exit_success"] and s["exit_within_budget"] and s["logged_termination"]
    exit_detail = f"exit={s['exit_status_code']} in {s['exit_ms']:.1f} ms logged={s['logged_termination']}"
    checks.add("T1", lifecycle, f"{rep['run']}: {exit_detail}")
    probes_ok = rep["probes_matched"] == rep["probes_sent"] and rep["unexpected_nonempty_batches"] == 0
    probe_detail = (f"matched {rep['probes_matched']}/{rep['probes_sent']}, "
                    f"unexpected {rep['unexpected_nonempty_batches']}")
    checks.add("T2", probes_ok, f"{rep['run']}: {probe_detail}")
    p99 = rep["probe_latency"]["p99_ns"]
    checks.add("T3", p99 is not None and p99 <= TRANSPORT_P99_BUDGET_NS, f"{rep['run']}: p99 {p99} ns")


def summarize_transport(raw, workload, manifest, checks):
    """Evaluate T1–T3 and collect transport metrics."""
    names = sorted(n for n in os.listdir(raw) if n.startswith("transport-") and n.endswith(SUMMARY_SUFFIX))
    reps, pooled = [], []
    for name in names:
        loaded = load_transport(raw, name)
        rep = transport_rep(loaded, workload, manifest)
        check_transport_rep(checks, rep, loaded["summary"])
        pooled.extend(rep.pop("latencies"))
        reps.append(rep)
    return {"reps": reps, "pooled_probe_latency": latency_stats(pooled)}


# ── Rendering ─────────────────────────────────────────────────────────────────


def fmt_us(ns):
    """Format nanoseconds as microseconds with one decimal."""
    return "n/a" if ns is None else f"{ns / 1000:.1f}"


def render_checks(summary):
    """Render the pre-registered checks table."""
    lines = ["## Pre-registered checks", "", "| Check | Result | Detail |", "|---|---|---|"]
    for key, item in summary["checks"].items():
        detail = "; ".join(item["details"])[:300].replace("|", "\\|")
        lines.append(f"| {key} | {'PASS' if item[RESULT_FIELD] else 'FAIL'} | {detail} |")
    return lines


def render_inprocess(ip):
    """Render in-process run and derived overhead tables."""
    lines = [
        "",
        f"## In-process (measured; {ip['ticks_per_run']} ticks/run, first "
        f"{ip['warmup_ticks_excluded']} excluded from latency/throughput)",
        "",
        "| Run | p50 µs | p99 µs | max µs | ticks/s | VmHWM kB | spike ticks |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in ip["runs"]:
        lat = r["latency_post_warmup"]
        lines.append(f"| {r['run']} | {fmt_us(lat['p50_ns'])} | {fmt_us(lat['p99_ns'])} | {fmt_us(lat['max_ns'])} | "
                     f"{r['ticks_per_s']:.0f} | {r['vm_hwm_kb']} | {r['spike_ticks']} |")
    lines += ["", "Derived daemon-live − baseline overhead (main variant, matching reps):", "",
              "| Rep | Δp50 µs | Δp99 µs | throughput ratio |", "|---|---|---|---|"]
    for o in ip["derived_overhead_vs_baseline"]:
        lines.append(f"| {o['rep']} | {fmt_us(o['derived_p50_overhead_ns'])} | "
                     f"{fmt_us(o['derived_p99_overhead_ns'])} | {o['derived_throughput_ratio']:.3f} |")
    return lines


def render_transport(tr):
    """Render the transport repetition table."""
    lines = ["", "## Transport (measured; binary over loopback ZeroMQ at 1 kHz)", "",
             "| Run | probes matched | unexpected | p50 µs | p99 µs | max µs | batches/expected tick (derived) "
             "| gaps >1 ms | daemon VmHWM kB | exit ms |",
             "|---|---|---|---|---|---|---|---|---|---|"]
    for r in tr["reps"]:
        lat = r["probe_latency"]
        lines.append(f"| {r['run']} | {r['probes_matched']}/{r['probes_sent']} | {r['unexpected_nonempty_batches']} | "
                     f"{fmt_us(lat['p50_ns'])} | {fmt_us(lat['p99_ns'])} | {fmt_us(lat['max_ns'])} | "
                     f"{r['derived_batches_per_expected_tick']:.3f} | {r['batch_id_step_counts']['gap_gt_1ms']} | "
                     f"{r['daemon_vm_hwm_kb']} | {r['exit_ms']:.1f} |")
    pooled = tr["pooled_probe_latency"]
    lines += ["", f"Pooled probe latency: n={pooled['n']} p50 {fmt_us(pooled['p50_ns'])} µs, "
              f"p99 {fmt_us(pooled['p99_ns'])} µs, max {fmt_us(pooled['max_ns'])} µs.", ""]
    return lines


def render_markdown(summary):
    """Render summary.md."""
    head = [f"# Usefulness experiment summary — `{summary['measured_commit']}`", "",
            f"Decision (mechanical, per PROTOCOL.md): **{summary['decision']}**", ""]
    body = render_checks(summary) + render_inprocess(summary["in_process"]) + render_transport(summary["transport"])
    return "\n".join(head + body)


def decide(checks):
    """Return (ordered check results, mechanical decision)."""
    order = ["I1"] + [f"C{i}" for i in range(1, 13)] + ["T1", "T2", "T3", "P1", "P2"]
    ordered = {k: checks.result(k) for k in order}
    failed = [k for k, v in ordered.items() if not v[RESULT_FIELD]]
    if not failed:
        return ordered, "supports further development"
    return ordered, "change of direction or blocked (failed: " + ", ".join(failed) + ")"


def main():
    """Summarize one results directory."""
    if len(sys.argv) != 2:
        raise SystemExit(USAGE)
    results = sys.argv[1]
    raw = os.path.join(results, "raw")
    workload, manifest = load_json(WORKLOAD), load_json(MANIFEST)
    checks = Checks()
    runs = load_inprocess_runs(raw)
    in_process = summarize_inprocess(runs, workload, manifest, checks)
    transport = summarize_transport(raw, workload, manifest, checks)
    check_inventory(checks, runs, len(transport["reps"]))
    ordered, decision = decide(checks)
    summary = {
        "measured_commit": os.path.basename(os.path.normpath(results)),
        "percentile_method": "nearest-rank",
        "decision": decision,
        "checks": ordered,
        "in_process": in_process,
        "transport": transport,
        "note": "All values measured from raw files except keys prefixed 'derived'. "
                "Trained-model usefulness is not tested: the fixture is synthetic and untrained.",
    }
    with open(os.path.join(results, "summary.json"), "w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=2)
        fh.write("\n")
    with open(os.path.join(results, "summary.md"), "w", encoding="utf-8") as fh:
        fh.write(render_markdown(summary))
    print(decision)


if __name__ == "__main__":
    main()
