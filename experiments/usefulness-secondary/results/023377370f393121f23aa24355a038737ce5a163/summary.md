# Usefulness experiment summary — `023377370f393121f23aa24355a038737ce5a163`

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
| baseline-main-r0 | 0.9 | 1.5 | 58.8 | 1030012 | 8656 | 2500 |
| baseline-main-r1 | 0.9 | 1.5 | 39.0 | 1026323 | 8624 | 2500 |
| baseline-main-r2 | 0.7 | 1.1 | 27.7 | 1206136 | 8740 | 2500 |
| baseline-main-r3 | 0.7 | 1.2 | 37.7 | 1203326 | 8708 | 2500 |
| baseline-main-r4 | 0.9 | 1.3 | 32.5 | 1049170 | 8680 | 2500 |
| daemon-live-main-r0 | 2.9 | 5.0 | 51.2 | 265492 | 7864 | 2500 |
| daemon-live-main-r1 | 2.5 | 4.2 | 63.9 | 310967 | 8024 | 2500 |
| daemon-live-main-r2 | 2.2 | 3.9 | 54.8 | 337762 | 7920 | 2500 |
| daemon-live-main-r3 | 2.4 | 4.0 | 51.6 | 324567 | 7928 | 2500 |
| daemon-live-main-r4 | 2.5 | 4.2 | 189.1 | 300276 | 7960 | 2500 |
| daemon-live-unmasked-r0 | 2.9 | 5.3 | 46.1 | 269105 | 7912 | 2500 |
| daemon-live-unmasked-r1 | 2.6 | 4.7 | 46.3 | 295584 | 7880 | 2500 |
| daemon-live-unmasked-r2 | 2.4 | 3.9 | 39.5 | 321457 | 8036 | 2500 |
| daemon-live-unmasked-r3 | 2.2 | 3.6 | 262.7 | 337336 | 7828 | 2500 |
| daemon-live-unmasked-r4 | 2.6 | 4.3 | 116.1 | 290433 | 7968 | 2500 |
| daemon-live-zeroed-r0 | 2.9 | 4.8 | 66.0 | 269708 | 8032 | 2500 |
| daemon-live-zeroed-r1 | 2.5 | 4.1 | 48.5 | 311269 | 7992 | 2500 |
| daemon-live-zeroed-r2 | 2.3 | 4.1 | 54.4 | 326212 | 7868 | 2500 |
| daemon-live-zeroed-r3 | 2.5 | 4.2 | 58.1 | 311649 | 7980 | 2500 |
| daemon-live-zeroed-r4 | 2.2 | 4.0 | 47.3 | 333644 | 7928 | 2500 |
| daemon-simulation-main-r0 | 2.2 | 3.6 | 66.3 | 340897 | 7744 | 0 |
| daemon-simulation-main-r1 | 2.2 | 3.5 | 37.5 | 338857 | 7764 | 0 |
| daemon-simulation-main-r2 | 1.9 | 3.5 | 36.8 | 366408 | 7688 | 0 |
| daemon-simulation-main-r3 | 2.2 | 3.5 | 49.0 | 343180 | 7748 | 0 |
| daemon-simulation-main-r4 | 2.2 | 3.5 | 39.8 | 337696 | 7792 | 0 |

Derived daemon-live − baseline overhead (main variant, matching reps):

| Rep | Δp50 µs | Δp99 µs | throughput ratio |
|---|---|---|---|
| 0 | 2.0 | 3.5 | 0.258 |
| 1 | 1.6 | 2.7 | 0.303 |
| 2 | 1.4 | 2.8 | 0.280 |
| 3 | 1.7 | 2.8 | 0.270 |
| 4 | 1.6 | 2.9 | 0.286 |

## Transport (measured; binary over loopback ZeroMQ at 1 kHz)

| Run | probes matched | unexpected | p50 µs | p99 µs | max µs | batches/expected tick (derived) | gaps >1 ms | daemon VmHWM kB | exit ms |
|---|---|---|---|---|---|---|---|---|---|
| transport-r0 | 200/200 | 0 | 980.0 | 1693.7 | 1793.0 | 1.000 | 1316 | 6664 | 3.6 |
| transport-r1 | 200/200 | 0 | 847.4 | 1682.9 | 1709.3 | 1.000 | 1111 | 6608 | 4.1 |
| transport-r2 | 200/200 | 0 | 965.7 | 1710.4 | 4085.1 | 1.000 | 1351 | 6604 | 2.9 |

Pooled probe latency: n=600 p50 932.4 µs, p99 1693.7 µs, max 4085.1 µs.
