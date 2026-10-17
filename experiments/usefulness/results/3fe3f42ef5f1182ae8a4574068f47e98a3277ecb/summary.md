# Usefulness experiment summary — `3fe3f42ef5f1182ae8a4574068f47e98a3277ecb`

Decision (mechanical, per PROTOCOL.md): **supports further development**

## Pre-registered checks

| Check | Result | Detail |
|---|---|---|
| I1 | PASS | pre-registered run inventory complete (5 reps x 5 in-process families, 3 transport reps) |
| C1 | PASS | ticks == accepted == N, zero rejected/errors/dropped in every daemon run |
| C2 | PASS | sensory accepted == N, reward accepted == modulated ticks, no class loss |
| C3 | PASS | post-run health not fatal |
| C4 | PASS | daemon-live raster == baseline raster in every repetition |
| C5 | PASS | all repetitions of each mode/variant identical |
| C6 | PASS | driven ticks fire exactly the mapped neurons |
| C7 | PASS | every spike within the quiet window of its channel's input |
| C8 | PASS | no spikes in quiet periods |
| C9 | PASS | zeroed raster == main raster |
| C10 | PASS | unmasked raster differs; neurons 4,5 fire |
| C11 | PASS | modulated segment fires on fewer than 10/10 ticks in every cycle |
| C12 | PASS | simulation control emits zero spikes |
| T1 | PASS | SIGTERM exit 0 within 2 s with termination log, every rep |
| T2 | PASS | every probe matched exactly once, no unexpected nonempty batches |
| T3 | PASS | probe p99 <= 3000000 ns in every rep |
| P1 | PASS | daemon-live p99 <= 1000000 ns in every run |
| P2 | PASS | daemon-live >= 1000 ticks/s in every run |

## In-process (measured; 10000 ticks/run, first 200 excluded from latency/throughput)

| Run | p50 µs | p99 µs | max µs | ticks/s | VmHWM kB | spike ticks |
|---|---|---|---|---|---|---|
| baseline-main-r0 | 1.5 | 16.3 | 92.8 | 432563 | 8732 | 2500 |
| baseline-main-r1 | 0.9 | 1.5 | 44.5 | 944165 | 8800 | 2500 |
| baseline-main-r2 | 0.9 | 1.6 | 119.1 | 908878 | 8780 | 2500 |
| baseline-main-r3 | 0.9 | 1.5 | 52.7 | 937223 | 8676 | 2500 |
| baseline-main-r4 | 1.5 | 2.5 | 58.6 | 566082 | 8712 | 2500 |
| daemon-live-main-r0 | 3.0 | 16.8 | 87.7 | 191652 | 7996 | 2500 |
| daemon-live-main-r1 | 2.9 | 6.0 | 71.2 | 251443 | 7924 | 2500 |
| daemon-live-main-r2 | 2.9 | 5.5 | 71.8 | 256025 | 7928 | 2500 |
| daemon-live-main-r3 | 2.9 | 4.8 | 64.1 | 255757 | 7880 | 2500 |
| daemon-live-main-r4 | 4.4 | 9.3 | 62.9 | 181172 | 7936 | 2500 |
| daemon-live-unmasked-r0 | 3.0 | 5.8 | 45.9 | 250829 | 7960 | 2500 |
| daemon-live-unmasked-r1 | 3.0 | 5.7 | 369.8 | 234031 | 7876 | 2500 |
| daemon-live-unmasked-r2 | 3.0 | 5.4 | 46.7 | 247333 | 7952 | 2500 |
| daemon-live-unmasked-r3 | 3.0 | 5.1 | 56.4 | 247963 | 7972 | 2500 |
| daemon-live-unmasked-r4 | 3.0 | 6.2 | 179.6 | 246323 | 7916 | 2500 |
| daemon-live-zeroed-r0 | 3.0 | 15.9 | 54.7 | 190195 | 7880 | 2500 |
| daemon-live-zeroed-r1 | 2.9 | 5.6 | 54.4 | 257327 | 8016 | 2500 |
| daemon-live-zeroed-r2 | 2.9 | 7.6 | 86.2 | 226443 | 8012 | 2500 |
| daemon-live-zeroed-r3 | 3.0 | 6.9 | 77.5 | 219205 | 7904 | 2500 |
| daemon-live-zeroed-r4 | 2.9 | 4.8 | 79.0 | 258343 | 7904 | 2500 |
| daemon-simulation-main-r0 | 2.5 | 4.4 | 46.4 | 286156 | 7772 | 0 |
| daemon-simulation-main-r1 | 2.6 | 6.5 | 186.0 | 247955 | 7728 | 0 |
| daemon-simulation-main-r2 | 2.6 | 6.1 | 71.7 | 262614 | 7704 | 0 |
| daemon-simulation-main-r3 | 2.6 | 6.2 | 76.9 | 232649 | 7820 | 0 |
| daemon-simulation-main-r4 | 2.6 | 4.5 | 49.5 | 284106 | 7448 | 0 |

Derived daemon-live − baseline overhead (main variant, matching reps):

| Rep | Δp50 µs | Δp99 µs | throughput ratio |
|---|---|---|---|
| 0 | 1.4 | 0.5 | 0.443 |
| 1 | 2.0 | 4.6 | 0.266 |
| 2 | 2.0 | 3.9 | 0.282 |
| 3 | 2.0 | 3.3 | 0.273 |
| 4 | 2.9 | 6.7 | 0.320 |

## Transport (measured; binary over loopback ZeroMQ at 1 kHz)

| Run | probes matched | unexpected | p50 µs | p99 µs | max µs | batches/expected tick (derived) | gaps >1 ms | daemon VmHWM kB | exit ms |
|---|---|---|---|---|---|---|---|---|---|
| transport-r0 | 200/200 | 0 | 1164.7 | 1777.6 | 2025.1 | 0.999 | 1278 | 6628 | 3.2 |
| transport-r1 | 200/200 | 0 | 1017.2 | 1628.0 | 3134.4 | 1.000 | 1312 | 6556 | 3.8 |
| transport-r2 | 200/200 | 0 | 1097.5 | 1771.9 | 3510.5 | 1.000 | 1443 | 6592 | 3.1 |

Pooled probe latency: n=600 p50 1083.6 µs, p99 1771.9 µs, max 3510.5 µs.
