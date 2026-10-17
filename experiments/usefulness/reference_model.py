"""Hand-written reference of the `neuromod` 0.6.0 LIF bank, used only to
derive expected spike sets for the synthetic fixture.

It follows the documented order of work in `SpikingNetwork::step` for an
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


class ReferenceNetwork:
    def __init__(self, weights, threshold, decay):
        self.weights = [list(row) for row in weights]
        self.channels = len(weights[0])
        self.v = [0.0] * len(weights)
        self.threshold = [threshold] * len(weights)
        self.decay = [decay] * len(weights)
        self.last_spike = [False] * len(weights)
        self.pred = [0.0] * self.channels
        for row in self.weights:
            total = sum(row)
            assert abs(total - WEIGHT_BUDGET) < 1e-12, "rows must sum to the L1 budget"

    def step(self, stimuli, modulators):
        da, sht, ach, ne = modulators
        stress = max(1.0 - ne, 0.1)
        lr = 0.5 * da
        for i in range(len(self.weights)):
            self.decay[i] = 0.15 - 0.05 * ach
            target = 0.20 - 0.05 * da + 0.15 * ne - 0.05 * sht
            target += 0.005 if self.last_spike[i] else -0.001
            target = min(max(target, 0.05), 0.50)
            self.threshold[i] += (target - self.threshold[i]) * lr
            self.threshold[i] = min(max(self.threshold[i], 0.05), 0.50)
        s = [min(max(abs(x), 0.0), 1.0) for x in stimuli]
        err = [abs(s[c] - self.pred[c]) for c in range(self.channels)]
        self.pred = [PRED_ALPHA * s[c] + (1 - PRED_ALPHA) * self.pred[c] for c in range(self.channels)]
        margin = float("inf")
        for i, row in enumerate(self.weights):
            current = sum(row[c] * (s[c] + PRED_ERR_WEIGHT * err[c]) for c in range(self.channels))
            current *= 0.45 * stress
            self.v[i] += current
            self.v[i] -= self.v[i] * self.decay[i]
            margin = min(margin, abs(self.v[i] - self.threshold[i]))
        spikes = []
        for i in range(len(self.weights)):
            if self.v[i] >= self.threshold[i]:
                self.v[i] = 0.0
                self.last_spike[i] = True
                spikes.append(i)
            else:
                self.last_spike[i] = False
        if spikes:
            fired = set(spikes)
            for i in range(len(self.weights)):
                if i not in fired:
                    self.v[i] = max(self.v[i] - INHIBITION, 0.0)
        return spikes, margin
