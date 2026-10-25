#!/usr/bin/env bash
# Reproducible runner for the secondary usefulness experiment (see PROTOCOL.md).
#
# Usage (from anywhere inside a clean checkout):
#   experiments/usefulness-secondary/run.sh
#
# Writes experiments/usefulness-secondary/results/<sha>/ with identity.json (explicit
# final status), per-step command logs, raw JSONL, summary.json and summary.md.
# Partial results are kept when a step fails.
set -uo pipefail

IN_PROCESS_REPS=5
TRANSPORT_REPS=3

repo=$(git rev-parse --show-toplevel) || exit 1
cd "$repo" || exit 1

# Earlier result sets (committed or not) do not make the tree dirty; anything
# else does. The per-SHA directory check below prevents overwriting results.
clean_pathspec=(-- . ':(exclude)experiments/usefulness-secondary/results')
if [[ -n "$(git status --porcelain --untracked-files=all "${clean_pathspec[@]}")" ]]; then
    echo "refusing to run: working tree is dirty (commit or stash first)" >&2
    git status --short "${clean_pathspec[@]}" >&2
    exit 1
fi

sha=$(git rev-parse HEAD)
out="experiments/usefulness-secondary/results/$sha"
if [[ -e "$out" ]]; then
    echo "refusing to overwrite existing results in $out" >&2
    exit 1
fi
mkdir -p "$out/logs" "$out/raw"
steps="$out/steps.tsv"
printf 'index\tlabel\texit_code\tmilliseconds\tcommand\n' >"$steps"
step_index=0

write_identity() {
    python3 - "$out" "$sha" "$1" "$IN_PROCESS_REPS" "$TRANSPORT_REPS" <<'PY'
import hashlib, json, os, platform, subprocess, sys, time
out, sha, status, reps_in, reps_tr = sys.argv[1:6]

def run(*cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, check=False).stdout.strip()
    except OSError as exc:
        return f"unavailable: {exc}"

def sha256(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()

def cpu_model():
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor()

def mem_total_kb():
    try:
        for line in open("/proc/meminfo"):
            if line.startswith("MemTotal:"):
                return int(line.split()[1])
    except OSError:
        return None

def os_release():
    try:
        for line in open("/etc/os-release"):
            if line.startswith("PRETTY_NAME="):
                return line.split("=", 1)[1].strip().strip('"')
    except OSError:
        return None

inputs = [
    "experiments/usefulness-secondary/PROTOCOL.md",
    "experiments/usefulness-secondary/fixtures/snn_model.json",
    "experiments/usefulness-secondary/fixtures/fixture-manifest.json",
    "experiments/usefulness-secondary/fixtures/workload.json",
    "experiments/usefulness-secondary/config/live.toml",
    "experiments/usefulness-secondary/config/simulation.toml",
    "experiments/usefulness-secondary/config/transport-live.toml",
    "Cargo.lock",
]
identity = {
    "status": status,
    "measured_commit": sha,
    "recorded_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "toolchain": {
        "rustc": run("rustc", "-V"),
        "cargo": run("cargo", "-V"),
        "active_toolchain": run("rustup", "show", "active-toolchain"),
        "libzmq_pkg_config": run("pkg-config", "--modversion", "libzmq"),
        "python": platform.python_version(),
    },
    "host": {
        "os": os_release(),
        "kernel": run("uname", "-srvm"),
        "cpu_model": cpu_model(),
        "logical_cpus": os.cpu_count(),
        "mem_total_kb": mem_total_kb(),
    },
    "build": {"profile": "release", "features": ["corpus-ipc"], "locked": True},
    "repetitions": {"in_process": int(reps_in), "transport": int(reps_tr)},
    "input_sha256": {path: sha256(path) for path in inputs},
}
with open(os.path.join(out, "identity.json"), "w") as fh:
    json.dump(identity, fh, indent=2)
    fh.write("\n")
PY
}

# step <label> <command...>: run, log stdout/stderr, record exit code; abort on failure.
step() {
    local label=$1
    shift
    step_index=$((step_index + 1))
    local log
    log=$(printf '%s/logs/%02d-%s.txt' "$out" "$step_index" "$label")
    local start_ns
    start_ns=$(date +%s%N)
    { printf '$'; printf ' %q' "$@"; printf '\n'; } >"$log"
    "$@" >>"$log" 2>&1
    local rc=$?
    local elapsed_ms=$((($(date +%s%N) - start_ns) / 1000000))
    printf '%d\t%s\t%d\t%d\t%s\n' "$step_index" "$label" "$rc" "$elapsed_ms" "$*" >>"$steps"
    if ((rc != 0)); then
        echo "step $label failed (exit $rc); see $log" >&2
        write_identity "failed:$label"
        exit 1
    fi
}

write_identity "running"

step build cargo build --release --locked --features corpus-ipc \
    --bin brainstem-daemon --example usefulness_secondary_inprocess --example usefulness_secondary_transport

inprocess=target/release/examples/usefulness_secondary_inprocess
transport=target/release/examples/usefulness_secondary_transport

for ((rep = 0; rep < IN_PROCESS_REPS; rep++)); do
    step "baseline-main-r$rep" "$inprocess" --mode baseline --variant main --rep "$rep" --out "$out/raw"
    for variant in main zeroed unmasked; do
        step "daemon-live-$variant-r$rep" "$inprocess" --mode daemon-live --variant "$variant" \
            --rep "$rep" --out "$out/raw"
    done
    step "daemon-simulation-main-r$rep" "$inprocess" --mode daemon-simulation --variant main \
        --rep "$rep" --out "$out/raw"
done

for ((rep = 0; rep < TRANSPORT_REPS; rep++)); do
    step "transport-r$rep" "$transport" --bin target/release/brainstem-daemon --rep "$rep" \
        --out "$out/raw"
done

step summarize python3 experiments/usefulness-secondary/summarize.py "$out"

write_identity "complete"
echo "results: $out"
