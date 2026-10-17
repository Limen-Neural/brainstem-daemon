#!/usr/bin/env bash
# Reproducible runner for the usefulness experiment. See PROTOCOL.md §7.
# Refuses a dirty tree, records identity, builds the release drivers and the
# corpus-ipc binary, runs every repetition as a separate process, gzips the
# raw per-tick JSONL, and summarizes. Partial results are kept on failure.
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
EXP=experiments/usefulness
TOOLCHAIN=1.98.1
R_INPROCESS=${R_INPROCESS:-5}
R_TRANSPORT=${R_TRANSPORT:-3}

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "refusing to run on a dirty working tree (commit first)" >&2
    exit 1
fi
SHA=$(git rev-parse HEAD)
OUT=$EXP/results/$SHA
mkdir -p "$OUT/logs"
STEPS="$OUT/steps.log"
: >"$STEPS"

status=failed
finish() {
    python3 -I - "$OUT/identity.json" "$status" <<'PY'
import json, sys, datetime
path, status = sys.argv[1], sys.argv[2]
identity = json.load(open(path))
identity["status"] = status
identity["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
json.dump(identity, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
    echo "status: $status (results in $OUT)"
}
trap finish EXIT

step() {
    local name=$1
    shift
    echo "== $name: $*" | tee -a "$STEPS"
    if "$@" >"$OUT/logs/$name.log" 2>&1; then
        echo "   ok" | tee -a "$STEPS"
    else
        local rc=$?
        echo "   FAILED (exit $rc); see $OUT/logs/$name.log" | tee -a "$STEPS"
        return "$rc"
    fi
}

python3 -I - "$OUT/identity.json" "$SHA" "$R_INPROCESS" "$R_TRANSPORT" "$TOOLCHAIN" <<'PY'
import hashlib, json, os, platform, subprocess, sys, datetime, glob
path, sha, r_in, r_tr, toolchain = sys.argv[1:6]
def run(*cmd):
    try:
        return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip()
    except Exception as exc:  # noqa: BLE001
        return f"unavailable: {exc}"
def sha256(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
cpu = ""
for line in open("/proc/cpuinfo"):
    if line.startswith("model name"):
        cpu = line.split(":", 1)[1].strip(); break
mem_kb = next((int(l.split()[1]) for l in open("/proc/meminfo") if l.startswith("MemTotal:")), None)
os_release = {}
try:
    for line in open("/etc/os-release"):
        if "=" in line:
            k, v = line.rstrip().split("=", 1); os_release[k] = v.strip('"')
except OSError:
    pass
files = sorted(glob.glob("experiments/usefulness/config/*.toml") + glob.glob("experiments/usefulness/fixtures/**/*.json", recursive=True)
               + ["examples/usefulness_inprocess.rs", "examples/usefulness_transport.rs", "Cargo.lock"])
identity = {
    "sha": sha,
    "status": "running",
    "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "toolchain": toolchain,
    "rustc": run("rustup", "run", toolchain, "rustc", "-V"),
    "cargo": run("rustup", "run", toolchain, "cargo", "-V"),
    "features": ["corpus-ipc"],
    "profile": "release",
    "kernel": platform.release(),
    "os": os_release.get("PRETTY_NAME", platform.platform()),
    "cpu_model": cpu,
    "cpu_count": os.cpu_count(),
    "mem_total_kb": mem_kb,
    "repetitions": {"inprocess": int(r_in), "transport": int(r_tr)},
    "file_sha256": {f: sha256(f) for f in files},
}
json.dump(identity, open(path, "w"), indent=2); open(path, "a").write("\n")
PY

step regenerate-fixture python3 -I "$EXP/make_fixture.py"
step fixture-unchanged git diff --exit-code -- "$EXP/fixtures"
step build env CC=gcc CXX=g++ cargo "+$TOOLCHAIN" build --locked --release --features corpus-ipc \
    --bin brainstem-daemon --example usefulness_inprocess --example usefulness_transport

WORKLOAD=$EXP/fixtures/workload.json
for rep in $(seq 1 "$R_INPROCESS"); do
    for mode in daemon-live baseline daemon-simulation; do
        case $mode in
            daemon-simulation) cfg=$EXP/config/simulation.toml ;;
            *) cfg=$EXP/config/live.toml ;;
        esac
        step "$mode-r$rep" target/release/examples/usefulness_inprocess "$mode" "$cfg" "$WORKLOAD" \
            "$OUT/$mode-r$rep.jsonl" "$OUT/$mode-r$rep.summary.json"
        gzip -9 -n -f "$OUT/$mode-r$rep.jsonl"
    done
done
for rep in $(seq 1 "$R_TRANSPORT"); do
    step "transport-live-r$rep" target/release/examples/usefulness_transport target/release/brainstem-daemon \
        "$EXP/config/transport-live.toml" "$WORKLOAD" "$OUT/transport-live-r$rep.jsonl" "$OUT/transport-live-r$rep.summary.json"
    gzip -9 -n -f "$OUT/transport-live-r$rep.jsonl"
done

step summarize python3 -I "$EXP/summarize.py" "$OUT"
status=passed
