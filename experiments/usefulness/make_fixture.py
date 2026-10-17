#!/usr/bin/env python3
"""Generate the synthetic live fixture, the deterministic workload, and the
pre-registered expected spike sets for the usefulness experiment.

Standard library only. Re-running this script must reproduce the committed
files byte-for-byte; `run.sh` checks that.

The expected spike sets come from `reference_model.py`, a from-the-docs
re-implementation of the LIF bank in `neuromod` 0.6.0 `SpikingNetwork::step`.
It is independent of the Rust engine, so the daemon and the direct-engine
baseline are both checked against a third, hand-written model.
"""

import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from reference_model import ReferenceNetwork  # noqa: E402

FIXTURE_VERSION = "lif8x16-v1"
CHANNELS = 8
NEURONS = 16
THRESHOLD = 0.25
DECAY = 0.15

TICKS_PER_CYCLE = 2000
CYCLES = 10
WARMUP_TICKS = 1000
PULSE_SPACING = 20

TRANSPORT_PULSES = 200
# Modulator snapshot used by the modulator segment, in corpus-ipc field order.
# The daemon maps these positionally onto neuromod (DA, 5-HT, ACh, NE), so
# `tempo` lands on norepinephrine; 0.5 is the smallest value corpus-ipc accepts.
MODULATORS = {"dopamine": 0.5, "cortisol": 0.2, "acetylcholine": 0.4, "tempo": 0.5}
# Minimum distance between a membrane potential and the firing threshold on
# every tick of the workload. Guards the f64 reference against f32 rounding.
MIN_MARGIN = 5e-3


def weight_rows():
    """Neuron i < 8 listens to channel i only (weight 2.0). Neuron 8+k listens
    to channels k and (k+1) % 8 (weight 1.0 each). Every row sums to the
    engine's L1 budget (2.0), so the per-step renormalization is a no-op."""
    rows = []
    for i in range(CHANNELS):
        row = [0.0] * CHANNELS
        row[i] = 2.0
        rows.append(row)
    for k in range(CHANNELS):
        row = [0.0] * CHANNELS
        row[k] = 1.0
        row[(k + 1) % CHANNELS] = 1.0
        rows.append(row)
    return rows


def expected_single_pulse(channel):
    """Neurons whose row has a nonzero weight on `channel`."""
    return sorted(
        i for i, row in enumerate(weight_rows()) if row[channel] != 0.0
    )


def sidecar_document():
    return {
        "source": "spikenaut_julia",
        "encoder": "synthetic-usefulness-experiment",
        "q88": "signed",
        "frozen_lineage": f"usefulness-{FIXTURE_VERSION}",
        "neurons": [
            {
                "decay_rate": DECAY,
                "membrane_potential": 0.0,
                "threshold": THRESHOLD,
                "last_spike": False,
                "weights": row,
            }
            for row in weight_rows()
        ],
    }


def cycle_events():
    """Events at offsets inside one 2,000-tick cycle. Every other tick is idle
    (the source returns `None`, the daemon zero-fills)."""
    ones = [True] * CHANNELS

    def frame(values, mask=None, modulators=None):
        return {"values": values, "valid_mask": mask, "modulators": modulators}

    def one_hot(channel, amplitude=1.0):
        values = [0.0] * CHANNELS
        values[channel] = amplitude
        return values

    events = {}
    # Segment A: one isolated pulse per channel, 20 ticks apart.
    for c in range(CHANNELS):
        events[PULSE_SPACING * c] = frame(one_hot(c), list(ones))
    # Segment B: two consecutive pulses on channel 3.
    for t in (200, 201):
        events[t] = frame(one_hot(3), list(ones))
    # Segment C: two channels in one frame.
    values = [0.0] * CHANNELS
    values[1] = 1.0
    values[5] = 1.0
    events[300] = frame(values, list(ones))
    # Segment D: a weak pulse that should drive the single-channel detector
    # but not the pair detectors.
    events[400] = frame(one_hot(2, 0.3), list(ones))
    # Segment M: channel 4 carries 1.0 but is masked out; channel 6 is live.
    values = [0.0] * CHANNELS
    values[4] = 1.0
    values[6] = 1.0
    mask = list(ones)
    mask[4] = False
    events[500] = frame(values, mask)
    # Segment Z: the zero-equivalent of M (channel 4 explicitly 0.0, all valid).
    values = [0.0] * CHANNELS
    values[6] = 1.0
    events[600] = frame(values, list(ones))
    # Segment N: a modulator-only packet, then a pulse carrying modulators.
    events[700] = frame([], None, dict(MODULATORS))
    events[701] = frame(one_hot(0), list(ones), dict(MODULATORS))
    # 702..1999: idle settle.
    return events


SEGMENTS = {
    "A_isolated_pulses": [0, 160],
    "B_burst": [200, 202],
    "C_two_channels": [300, 301],
    "D_weak_pulse": [400, 401],
    "M_masked": [500, 520],
    "Z_zero_equivalent": [600, 620],
    "N_modulators": [700, 720],
    "idle_settle": [720, TICKS_PER_CYCLE],
}


def to_engine_modulators(mods):
    if mods is None:
        return (0.0, 0.0, 0.0, 0.0)
    return (mods["dopamine"], mods["cortisol"], mods["acetylcholine"], mods["tempo"])


def decode_stimuli(event):
    """Mirror `decode_inputs`: copy, pad with zeros, apply mask -> 0."""
    values = list(event["values"])[:CHANNELS]
    values += [0.0] * (CHANNELS - len(values))
    mask = event["valid_mask"]
    if mask is not None:
        for idx, valid in enumerate(mask[:CHANNELS]):
            if not valid:
                values[idx] = 0.0
    return values


def simulate(events_by_tick, ticks):
    net = ReferenceNetwork(weight_rows(), THRESHOLD, DECAY)
    expected = {}
    min_margin = float("inf")
    for t in range(ticks):
        event = events_by_tick.get(t)
        if event is None:
            stimuli = [0.0] * CHANNELS
            mods = (0.0, 0.0, 0.0, 0.0)
        else:
            stimuli = decode_stimuli(event)
            mods = to_engine_modulators(event["modulators"])
        spikes, margin = net.step(stimuli, mods)
        min_margin = min(min_margin, margin)
        if spikes:
            expected[t] = spikes
    return expected, min_margin


def inprocess_expectations():
    per_cycle = cycle_events()
    events = {}
    for cycle in range(CYCLES):
        for offset, event in per_cycle.items():
            events[cycle * TICKS_PER_CYCLE + offset] = event
    expected, margin = simulate(events, TICKS_PER_CYCLE * CYCLES)
    # Pre-registration sanity: the hand-derived single-pulse sets must match
    # the model on segment A of every cycle, and M must equal Z.
    for cycle in range(CYCLES):
        base = cycle * TICKS_PER_CYCLE
        for c in range(CHANNELS):
            t = base + PULSE_SPACING * c
            assert expected.get(t) == expected_single_pulse(c), (t, expected.get(t))
            for extra in range(1, PULSE_SPACING):
                assert (t + extra) not in expected, ("echo spike", t + extra)
        m = {k - 500: v for k, v in expected.items() if base + 500 <= k < base + 520}
        z = {k - 600: v for k, v in expected.items() if base + 600 <= k < base + 620}
        m = {k - base: v for k, v in m.items()}
        z = {k - base: v for k, v in z.items()}
        assert m == z, ("mask equivalence", cycle, m, z)
    return per_cycle, expected, margin


def transport_expectations():
    """Pulses cycle through the channels every 20 ticks, forever. Each pulse
    must produce exactly one nonempty tick with the single-pulse set."""
    events = {}
    for k in range(TRANSPORT_PULSES + 50):
        c = k % CHANNELS
        values = [0.0] * CHANNELS
        values[c] = 1.0
        events[PULSE_SPACING * k] = {
            "values": values,
            "valid_mask": [True] * CHANNELS,
            "modulators": None,
        }
    ticks = PULSE_SPACING * (TRANSPORT_PULSES + 50)
    expected, margin = simulate(events, ticks)
    for k in range(TRANSPORT_PULSES + 50):
        t = PULSE_SPACING * k
        assert expected.get(t) == expected_single_pulse(k % CHANNELS), (t, expected.get(t))
        for extra in range(1, PULSE_SPACING):
            assert (t + extra) not in expected, ("transport echo spike", t + extra)
    return margin


def dump(path, obj):
    path.write_text(json.dumps(obj, indent=2) + "\n")


def main():
    fixture_dir = HERE / "fixtures" / FIXTURE_VERSION
    fixture_dir.mkdir(parents=True, exist_ok=True)
    sidecar = fixture_dir / "snn_model.json"
    dump(sidecar, sidecar_document())
    digest = hashlib.sha256(sidecar.read_bytes()).hexdigest()

    per_cycle, expected, margin = inprocess_expectations()
    transport_margin = transport_expectations()
    assert margin >= MIN_MARGIN, margin
    assert transport_margin >= MIN_MARGIN, transport_margin

    dump(
        fixture_dir / "MANIFEST.json",
        {
            "fixture_version": FIXTURE_VERSION,
            "sidecar": "snn_model.json",
            "sidecar_sha256": digest,
            "lif_count": NEURONS,
            "izh_count": 0,
            "channels": CHANNELS,
            "threshold": THRESHOLD,
            "decay_rate": DECAY,
            "statement": (
                "Synthetic, untrained, hand-designed fixture. It exercises the "
                "supported live checkpoint path and makes spike outputs "
                "derivable; it demonstrates no trained-model capability."
            ),
            "channel_to_neurons": {
                str(c): expected_single_pulse(c) for c in range(CHANNELS)
            },
            "weight_pattern": (
                "neuron i<8: weight 2.0 on channel i; neuron 8+k: weight 1.0 on "
                "channels k and (k+1)%8; every row sums to the engine L1 budget 2.0"
            ),
        },
    )
    dump(
        HERE / "fixtures" / "workload.json",
        {
            "version": "usefulness-workload-v1",
            "fixture_version": FIXTURE_VERSION,
            "fixture_sha256": digest,
            "channels": CHANNELS,
            "ticks_per_cycle": TICKS_PER_CYCLE,
            "cycles": CYCLES,
            "ticks": TICKS_PER_CYCLE * CYCLES,
            "warmup_ticks": WARMUP_TICKS,
            "segments": SEGMENTS,
            "cycle_events": [
                {"offset": offset, **event} for offset, event in sorted(per_cycle.items())
            ],
            "transport": {
                "pulse_spacing_ticks": PULSE_SPACING,
                "pulses": TRANSPORT_PULSES,
                "amplitude": 1.0,
                "expected_by_channel": {
                    str(c): expected_single_pulse(c) for c in range(CHANNELS)
                },
            },
        },
    )
    dump(
        HERE / "fixtures" / "expected_spikes.json",
        {
            "version": "usefulness-expected-v1",
            "fixture_sha256": digest,
            "ticks": TICKS_PER_CYCLE * CYCLES,
            "min_threshold_margin": margin,
            "nonempty_ticks": {str(t): ids for t, ids in sorted(expected.items())},
        },
    )
    print(f"fixture sha256 {digest}")
    print(f"nonempty expected ticks {len(expected)}; min margin {margin:.4f}; transport margin {transport_margin:.4f}")
    first = {t: ids for t, ids in expected.items() if t < TICKS_PER_CYCLE}
    for t, ids in first.items():
        print(f"  tick {t:5d}: {ids}")


if __name__ == "__main__":
    main()
