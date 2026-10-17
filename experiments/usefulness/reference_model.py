"""
Hand-written reference of the `neuromod` LIF bank used to derive expected spike sets.

It follows the documented order of work in `SpikingNetwork::step` (crates.io
`neuromod` 0.7.0; this normal-mode pipeline is unchanged from 0.6.0) for an
LIF-only network with `reward_lr = 0.0` (the loader's inference setting):

1. stress = max(1 - NE, 0.1); lr = 0.5 * DA
2. per neuron: decay = 0.15 - 0.05 * ACh; threshold blends toward
   0.20 - 0.05 DA + 0.15 NE - 0.05 5-HT (+0.005 if it spiked last tick,
   else -0.001), clamped to [0.05, 0.50], at rate lr
3. per channel: s = clamp(|stim|, 0, 1); err = |s - pred|; pred = 0.1 s + 0.9 pred
4. Bernoulli input encoding (RNG) only feeds STDP traces; with reward_lr = 0
   no weight ever changes, so it is omitted here
5. current = sum(w * (s + 0.5 err)) * 0.45 * stress; v += current; v -= v * decay
6. fire when v >= threshold (reset to 0); if anything fired, every other
   neuron loses 0.05 (floored at 0)
7. L1 renormalization to 2.0 per row is a no-op for rows that already sum to 2.0

Standard library only. Floats are f64; the generator asserts a margin so f32
rounding in the engine cannot flip a threshold crossing.
"""

WEIGHT_BUDGET = 2.0
PRED_ALPHA = 0.1
PRED_ERR_WEIGHT = 0.5
INHIBITION = 0.05
THRESHOLD_FLOOR = 0.05
THRESHOLD_CEIL = 0.50


def clamp(value, low, high):
    """Clamp `value` into [low, high]."""
    return min(max(value, low), high)


class ReferenceNetwork:
    """LIF bank with the engine's modulator retuning and lateral inhibition."""

    def __init__(self, weights, threshold, decay):
        """Start every neuron at rest with the given threshold and decay."""
        self.weights = [list(row) for row in weights]
        self.channels = len(weights[0])
        count = len(weights)
        self.v = [0.0] * count
        self.threshold = [threshold] * count
        self.decay = [decay] * count
        self.last_spike = [False] * count
        self.pred = [0.0] * self.channels
        for row in self.weights:
            if abs(sum(row) - WEIGHT_BUDGET) >= 1e-12:
                raise ValueError("rows must sum to the L1 budget")

    def step(self, stimuli, modulators):
        """Advance one tick; return (spiking neuron ids, min |v - threshold|)."""
        norepinephrine = modulators[3]
        self._retune(modulators)
        drive = self._encode(stimuli)
        margin = self._integrate(drive, max(1.0 - norepinephrine, 0.1))
        spikes = self._fire()
        self._inhibit(spikes)
        return spikes, margin

    def _retune(self, modulators):
        """Item 2: decay from ACh; threshold blends toward the target at rate 0.5·DA."""
        dopamine, serotonin, acetylcholine, norepinephrine = modulators
        rate = 0.5 * dopamine
        decay = 0.15 - 0.05 * acetylcholine
        base = 0.20 - 0.05 * dopamine + 0.15 * norepinephrine - 0.05 * serotonin
        for i, spiked in enumerate(self.last_spike):
            self.decay[i] = decay
            bonus = 0.005 if spiked else -0.001
            target = clamp(base + bonus, THRESHOLD_FLOOR, THRESHOLD_CEIL)
            blended = self.threshold[i] + (target - self.threshold[i]) * rate
            self.threshold[i] = clamp(blended, THRESHOLD_FLOOR, THRESHOLD_CEIL)

    def _encode(self, stimuli):
        """Item 3: clamp |stimulus|, update the EMA, return s + 0.5·err per channel."""
        drive = []
        for channel, value in enumerate(stimuli):
            s = clamp(abs(value), 0.0, 1.0)
            err = abs(s - self.pred[channel])
            self.pred[channel] = PRED_ALPHA * s + (1 - PRED_ALPHA) * self.pred[channel]
            drive.append(s + PRED_ERR_WEIGHT * err)
        return drive

    def _integrate(self, drive, stress):
        """Item 5: weighted current scaled by 0.45·stress, then leak; return min margin."""
        margin = float("inf")
        for i, row in enumerate(self.weights):
            current = sum(w * d for w, d in zip(row, drive))
            current *= 0.45 * stress
            self.v[i] += current
            self.v[i] -= self.v[i] * self.decay[i]
            margin = min(margin, abs(self.v[i] - self.threshold[i]))
        return margin

    def _fire(self):
        """Item 6a: neurons at or above threshold spike and reset."""
        spikes = []
        for i, potential in enumerate(self.v):
            fired = potential >= self.threshold[i]
            self.last_spike[i] = fired
            if fired:
                self.v[i] = 0.0
                spikes.append(i)
        return spikes

    def _inhibit(self, spikes):
        """Item 6b: when anything fired, every other neuron loses INHIBITION."""
        if not spikes:
            return
        fired = set(spikes)
        for i, potential in enumerate(self.v):
            if i not in fired:
                self.v[i] = max(potential - INHIBITION, 0.0)
