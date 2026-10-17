# Usefulness experiment summary — `40cfd056c08abc86d0e29588e30d8fed119f52ea`

Decision (derived): **supports_further_development**

## In-process runs (latency after 1,000 warm-up ticks; measured)

| run | checks | p50 | p99 | max | decode p50 | ticks/s | peak RSS | nonempty ticks |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| baseline-r1 | pass | 950 ns | 1.76 µs | 34.11 µs | — | 912,079 | 4388 kB | 150 |
| baseline-r2 | pass | 1.69 µs | 1.99 µs | 134.73 µs | — | 649,958 | 4480 kB | 150 |
| baseline-r3 | pass | 1.54 µs | 2.00 µs | 74.59 µs | — | 638,084 | 4396 kB | 150 |
| baseline-r4 | pass | 970 ns | 1.92 µs | 792.05 µs | — | 783,058 | 4500 kB | 150 |
| baseline-r5 | pass | 960 ns | 1.87 µs | 57.54 µs | — | 767,895 | 4460 kB | 150 |
| daemon-live-r1 | pass | 1.07 µs | 3.23 µs | 3.019 ms | 1.22 µs | 655,976 | 5644 kB | 150 |
| daemon-live-r2 | pass | 1.17 µs | 3.87 µs | 144.03 µs | 1.49 µs | 584,168 | 5644 kB | 150 |
| daemon-live-r3 | pass | 1.91 µs | 4.84 µs | 67.74 µs | 1.87 µs | 476,810 | 5640 kB | 150 |
| daemon-live-r4 | pass | 1.75 µs | 4.36 µs | 600.83 µs | 1.65 µs | 534,232 | 5168 kB | 150 |
| daemon-live-r5 | pass | 1.85 µs | 4.32 µs | 111.63 µs | 1.36 µs | 543,994 | 5656 kB | 150 |
| daemon-simulation-r1 | pass | 1.09 µs | 3.53 µs | 547.18 µs | 1.91 µs | 746,915 | 5636 kB | 0 |
| daemon-simulation-r2 | pass | 620 ns | 2.74 µs | 20.47 µs | 1.26 µs | 1,031,792 | 5164 kB | 0 |
| daemon-simulation-r3 | pass | 600 ns | 2.62 µs | 425.48 µs | 1.14 µs | 1,105,987 | 5132 kB | 0 |
| daemon-simulation-r4 | pass | 620 ns | 2.79 µs | 386.33 µs | 1.37 µs | 969,626 | 5140 kB | 0 |
| daemon-simulation-r5 | pass | 600 ns | 2.45 µs | 62.91 µs | 1.06 µs | 1,211,957 | 5144 kB | 0 |

## Transport runs (measured)

| run | checks | sent | matched | lost | e2e p50 | e2e p99 | e2e max | ready after | SIGTERM→exit | daemon peak RSS |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| transport-live-r1 | pass | 200 | 200 | 0 | 1.111 ms | 3.332 ms | 4.281 ms | 10.299 ms | 6.089 ms | 7052 kB |
| transport-live-r2 | pass | 200 | 200 | 0 | 1.109 ms | 3.459 ms | 4.233 ms | 12.051 ms | 6.227 ms | 7036 kB |
| transport-live-r3 | pass | 200 | 200 | 0 | 1.111 ms | 4.133 ms | 7.223 ms | 10.468 ms | 6.110 ms | 6968 kB |

## Per-run checks

- `baseline-r1`: all pass
- `baseline-r2`: all pass
- `baseline-r3`: all pass
- `baseline-r4`: all pass
- `baseline-r5`: all pass
- `daemon-live-r1`: all pass
- `daemon-live-r2`: all pass
- `daemon-live-r3`: all pass
- `daemon-live-r4`: all pass
- `daemon-live-r5`: all pass
- `daemon-simulation-r1`: all pass
- `daemon-simulation-r2`: all pass
- `daemon-simulation-r3`: all pass
- `daemon-simulation-r4`: all pass
- `daemon-simulation-r5`: all pass
- `transport-live-r1`: all pass
- `transport-live-r2`: all pass
- `transport-live-r3`: all pass
