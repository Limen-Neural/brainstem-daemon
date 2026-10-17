#!/usr/bin/env python3
"""
Generate the synthetic fixture, the deterministic workload, and the expected spike sets.

Standard library only. Re-running this script must reproduce the committed
files byte-for-byte; `run.sh` checks that.

The expected spike sets come from `reference_model.py`, a from-the-docs
re-implementation of the LIF bank in `neuromod` 0.7.0 `SpikingNetwork::step`.
It is independent of the Rust engine, so the daemon and the direct-engine
baseline are both checked against a third, hand-written model.
"""

import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from reference_model import ReferenceNetwork  # noqa: E402  pylint: disable=wrong-import-position

FIXTURE_VERSION = "lif8x16-v1"
CHANNELS = 8
NEURONS = 16
THRESHOLD = 0.25
DECAY = 0.15

TICKS_PER_CYCLE = 2000
CYCLES = 10
TICKS = TICKS_PER_CYCLE * CYCLES
WARMUP_TICKS = 1000
PULSE_SPACING = 20

TRANSPORT_PULSES = 200
# Extra pulses simulated past the measured count so the tail is covered too.
TRANSPORT_SLACK = 50
# Modulator snapshot used by the modulator segment, in corpus-ipc field order.
# The daemon maps these positionally onto neuromod (DA, 5-HT, ACh, NE), so
# `tempo` lands on norepinephrine; 0.5 is the smallest value corpus-ipc accepts.
MODULATORS = {"dopamine": 0.5, "cortisol": 0.2, "acetylcholine": 0.4, "tempo": 0.5}
NO_MODULATION = (0.0, 0.0, 0.0, 0.0)
# Minimum distance between a membrane potential and the firing threshold on
# every tick of the workload. Guards the f64 reference against f32 rounding.
MIN_MARGIN = 5e-3

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


def require(condition, message):
    """Fail loudly (and under `python -O`) when a pre-registration check breaks."""
    if not condition:
        raise AssertionError(message)


def weight_rows():
    """
    Structured weight rows that each sit exactly on the engine's L1 budget.

    Neuron i < 8 listens to channel i only (weight 2.0). Neuron 8+k listens
    to channels k and (k+1) % 8 (weight 1.0 each). Every row sums to 2.0, so
    the per-step renormalization is a no-op.
    """
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
    return sorted(i for i, row in enumerate(weight_rows()) if row[channel] != 0.0)


def channel_map():
    """Hand-derived single-pulse response per channel, keyed by channel string."""
    return {str(c): expected_single_pulse(c) for c in range(CHANNELS)}


def sidecar_document():
    """The Distill-style sidecar JSON the live checkpoint gate accepts."""
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


def frame(values, mask=None, modulators=None):
    """One workload event: stimulus values, optional validity mask, optional modulators."""
    return {"values": values, "valid_mask": mask, "modulators": modulators}


def valid_mask():
    """An all-valid mask."""
    return [True] * CHANNELS


def masked_except(channel):
    """An all-valid mask with one channel marked invalid."""
    mask = valid_mask()
    mask[channel] = False
    return mask


def multi_hot(channels, amplitude=1.0):
    """Stimulus vector with `amplitude` on the given channels and 0.0 elsewhere."""
    values = [0.0] * CHANNELS
    for channel in channels:
        values[channel] = amplitude
    return values


def one_hot(channel, amplitude=1.0):
    """Stimulus vector with `amplitude` on one channel."""
    return multi_hot((channel,), amplitude)


def cycle_events():
    """
    Events at their offsets inside one 2,000-tick cycle (see SEGMENTS).

    Every other tick is idle: the source returns `None` and the daemon
    zero-fills.
    """
    events = {}
    for channel in range(CHANNELS):
        events[PULSE_SPACING * channel] = frame(one_hot(channel), valid_mask())
    for tick in (200, 201):
        events[tick] = frame(one_hot(3), valid_mask())
    events[300] = frame(multi_hot((1, 5)), valid_mask())
    events[400] = frame(one_hot(2, 0.3), valid_mask())
    # M: channel 4 carries 1.0 but is masked out; Z is its zero-equivalent.
    events[500] = frame(multi_hot((4, 6)), masked_except(4))
    events[600] = frame(one_hot(6), valid_mask())
    # N: a modulator-only packet, then a pulse carrying the same modulators.
    events[700] = frame([], None, dict(MODULATORS))
    events[701] = frame(one_hot(0), valid_mask(), dict(MODULATORS))
    return events


def expand_cycles(per_cycle):
    """Repeat the per-cycle events over every cycle of the run."""
    events = {}
    for cycle in range(CYCLES):
        for offset, event in per_cycle.items():
            events[cycle * TICKS_PER_CYCLE + offset] = event
    return events


def to_engine_modulators(mods):
    """Positional corpus-ipc → neuromod mapping (DA, cortisol→5-HT, ACh, tempo→NE)."""
    if mods is None:
        return NO_MODULATION
    return (mods["dopamine"], mods["cortisol"], mods["acetylcholine"], mods["tempo"])


def decode_stimuli(event):
    """Mirror `decode_inputs`: copy, pad with zeros, apply mask -> 0."""
    values = list(event["values"])[:CHANNELS]
    values += [0.0] * (CHANNELS - len(values))
    mask = event["valid_mask"]
    if mask is None:
        return values
    for idx, valid in enumerate(mask[:CHANNELS]):
        if not valid:
            values[idx] = 0.0
    return values


def tick_inputs(event):
    """Decoded stimuli and engine modulators for one tick (idle when `event` is None)."""
    if event is None:
        return [0.0] * CHANNELS, NO_MODULATION
    return decode_stimuli(event), to_engine_modulators(event["modulators"])


def simulate(events_by_tick, ticks):
    """Run the reference model; return ({tick: spiking ids}, min threshold margin)."""
    net = ReferenceNetwork(weight_rows(), THRESHOLD, DECAY)
    expected = {}
    min_margin = float("inf")
    for tick in range(ticks):
        stimuli, modulators = tick_inputs(events_by_tick.get(tick))
        spikes, margin = net.step(stimuli, modulators)
        min_margin = min(min_margin, margin)
        if spikes:
            expected[tick] = spikes
    return expected, min_margin


def verify_single_pulse(expected, tick, channel):
    """An isolated pulse yields exactly its hand-derived set and no echo spike."""
    require(expected.get(tick) == expected_single_pulse(channel), ("pulse", tick, expected.get(tick)))
    for extra in range(1, PULSE_SPACING):
        require((tick + extra) not in expected, ("echo spike", tick + extra))


def segment_sets(expected, start, stop):
    """Nonempty expected sets inside [start, stop), keyed by offset from start."""
    return {t - start: ids for t, ids in expected.items() if start <= t < stop}


def verify_cycle(expected, base):
    """Segment A matches the hand-derived sets; segment M equals segment Z."""
    for channel in range(CHANNELS):
        verify_single_pulse(expected, base + PULSE_SPACING * channel, channel)
    masked = segment_sets(expected, base + 500, base + 520)
    zero = segment_sets(expected, base + 600, base + 620)
    require(masked == zero, ("mask equivalence", base, masked, zero))


def inprocess_expectations():
    """Per-cycle events, the full-run expected sets, and the minimum margin."""
    per_cycle = cycle_events()
    expected, margin = simulate(expand_cycles(per_cycle), TICKS)
    for cycle in range(CYCLES):
        verify_cycle(expected, cycle * TICKS_PER_CYCLE)
    return per_cycle, expected, margin


def transport_events():
    """Pulses that cycle through the channels every 20 ticks, forever."""
    events = {}
    for k in range(TRANSPORT_PULSES + TRANSPORT_SLACK):
        events[PULSE_SPACING * k] = frame(one_hot(k % CHANNELS), valid_mask())
    return events


def transport_expectations():
    """Each transport pulse must produce exactly one nonempty tick with the single-pulse set."""
    count = TRANSPORT_PULSES + TRANSPORT_SLACK
    expected, margin = simulate(transport_events(), PULSE_SPACING * count)
    for k in range(count):
        verify_single_pulse(expected, PULSE_SPACING * k, k % CHANNELS)
    return margin


def manifest_document(digest):
    """MANIFEST.json: fixture identity, dimensions, mapping, and the synthetic statement."""
    return {
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
        "channel_to_neurons": channel_map(),
        "weight_pattern": (
            "neuron i<8: weight 2.0 on channel i; neuron 8+k: weight 1.0 on "
            "channels k and (k+1)%8; every row sums to the engine L1 budget 2.0"
        ),
    }


def workload_document(per_cycle, digest):
    """workload.json: run shape, per-cycle events, and the transport pattern."""
    return {
        "version": "usefulness-workload-v1",
        "fixture_version": FIXTURE_VERSION,
        "fixture_sha256": digest,
        "channels": CHANNELS,
        "ticks_per_cycle": TICKS_PER_CYCLE,
        "cycles": CYCLES,
        "ticks": TICKS,
        "warmup_ticks": WARMUP_TICKS,
        "segments": SEGMENTS,
        "cycle_events": [{"offset": offset, **event} for offset, event in sorted(per_cycle.items())],
        "transport": {
            "pulse_spacing_ticks": PULSE_SPACING,
            "pulses": TRANSPORT_PULSES,
            "amplitude": 1.0,
            "expected_by_channel": channel_map(),
        },
    }


def expected_document(expected, digest, margin):
    """expected_spikes.json: the pre-registered nonempty ticks of the full run."""
    return {
        "version": "usefulness-expected-v1",
        "fixture_sha256": digest,
        "ticks": TICKS,
        "min_threshold_margin": margin,
        "nonempty_ticks": {str(t): ids for t, ids in sorted(expected.items())},
    }


def dump(path, obj):
    """Write `obj` as indented JSON with a trailing newline."""
    path.write_text(json.dumps(obj, indent=2) + "\n")


def print_preview(digest, expected, margins):
    """Print the fixture digest, margins, and the first cycle's expected sets."""
    print(f"fixture sha256 {digest}")
    print(
        f"nonempty expected ticks {len(expected)}; min margin {margins[0]:.4f}; "
        f"transport margin {margins[1]:.4f}"
    )
    for tick, ids in expected.items():
        if tick < TICKS_PER_CYCLE:
            print(f"  tick {tick:5d}: {ids}")


def main():
    """Write the sidecar, manifest, workload and expectations under fixtures/."""
    fixture_dir = HERE / "fixtures" / FIXTURE_VERSION
    fixture_dir.mkdir(parents=True, exist_ok=True)
    sidecar = fixture_dir / "snn_model.json"
    dump(sidecar, sidecar_document())
    digest = hashlib.sha256(sidecar.read_bytes()).hexdigest()

    per_cycle, expected, margin = inprocess_expectations()
    transport_margin = transport_expectations()
    require(margin >= MIN_MARGIN, f"in-process margin {margin} is below {MIN_MARGIN}")
    require(transport_margin >= MIN_MARGIN, f"transport margin {transport_margin} is below {MIN_MARGIN}")

    dump(fixture_dir / "MANIFEST.json", manifest_document(digest))
    dump(HERE / "fixtures" / "workload.json", workload_document(per_cycle, digest))
    dump(HERE / "fixtures" / "expected_spikes.json", expected_document(expected, digest, margin))
    print_preview(digest, expected, (margin, transport_margin))


if __name__ == "__main__":
    main()
