# Live sidecar fixture (`snn_model.json`)

- **Origin:** synthetic, hand-designed, generated in this repository for issue #82. Not a trained model and not a Hugging Face artifact.
- **Format:** Distill sidecar JSON, `source = "spikenaut_julia"`, LIF-only, no `output_weights` key.
- **Dimensions:** 16 LIF x 16 channels (`lif_count = 16`, `izh_count = 0`, `channels = 16`).
- **Derivation:** neuron `n` has weight `2.0` on channel `n` and `0.0` elsewhere (identity bank); `decay_rate = 0.15`, `threshold = 0.3`, `membrane_potential = 0.0`, `last_spike = false`. Each row sums to `2.0`, matching the row-sum convention of `experiments/usefulness-secondary/fixtures/`; that convention is chosen to match the engine's L1 weight budget, but this fixture's test only asserts that neuron 3 spikes, not that rows are unchanged after stepping.
- **SHA-256:** `f97771291000a3d805375d410b46a7232b54b003aaa3f1d9662ea5f9b09af848`
- **Lineage:** `live-fixture-v1` (model id `spikenaut-snn:live-fixture-v1`).

Regenerating the file changes the hash; update this manifest, the README, and `tests/live_fixture.rs` together.
