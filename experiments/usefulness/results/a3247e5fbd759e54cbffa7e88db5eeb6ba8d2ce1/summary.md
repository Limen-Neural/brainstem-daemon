# Usefulness experiment summary — `a3247e5fbd759e54cbffa7e88db5eeb6ba8d2ce1`

Decision (derived): **supports_further_development**

## In-process runs (latency after 1,000 warm-up ticks; measured)

| run | checks | p50 | p99 | max | decode p50 | ticks/s | peak RSS | nonempty ticks |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| baseline-r1 | pass | 971 ns | 1.77 µs | 29.01 µs | — | 882,300 | 4336 kB | 150 |
| baseline-r2 | pass | 970 ns | 1.71 µs | 11.55 µs | — | 929,518 | 4508 kB | 150 |
| baseline-r3 | pass | 970 ns | 1.13 µs | 29.47 µs | — | 944,355 | 4388 kB | 150 |
| baseline-r4 | pass | 970 ns | 1.35 µs | 59.42 µs | — | 941,080 | 4496 kB | 150 |
| baseline-r5 | pass | 980 ns | 1.39 µs | 45.18 µs | — | 920,989 | 4392 kB | 150 |
| daemon-live-r1 | pass | 1.84 µs | 4.51 µs | 61.44 µs | 1.63 µs | 527,364 | 5652 kB | 150 |
| daemon-live-r2 | pass | 1.11 µs | 3.40 µs | 48.69 µs | 1.23 µs | 707,728 | 5652 kB | 150 |
| daemon-live-r3 | pass | 1.09 µs | 3.21 µs | 49.04 µs | 1.11 µs | 784,465 | 5664 kB | 150 |
| daemon-live-r4 | pass | 1.07 µs | 3.23 µs | 179.77 µs | 1.18 µs | 715,672 | 5640 kB | 150 |
| daemon-live-r5 | pass | 1.09 µs | 3.12 µs | 56.79 µs | 1.17 µs | 797,612 | 5636 kB | 150 |
| daemon-simulation-r1 | pass | 610 ns | 2.59 µs | 13.71 µs | 1.20 µs | 1,077,569 | 5140 kB | 0 |
| daemon-simulation-r2 | pass | 600 ns | 2.23 µs | 54.18 µs | 1.05 µs | 1,273,490 | 5636 kB | 0 |
| daemon-simulation-r3 | pass | 860 ns | 2.67 µs | 18.39 µs | 1.42 µs | 1,030,768 | 5660 kB | 0 |
| daemon-simulation-r4 | pass | 600 ns | 2.11 µs | 48.47 µs | 1.10 µs | 1,319,240 | 5644 kB | 0 |
| daemon-simulation-r5 | pass | 610 ns | 2.49 µs | 59.13 µs | 1.20 µs | 1,147,651 | 5132 kB | 0 |

## Transport runs (measured)

| run | checks | sent | matched | lost | e2e p50 | e2e p99 | e2e max | ready after | SIGTERM→exit | daemon peak RSS |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| transport-live-r1 | pass | 200 | 200 | 0 | 1.110 ms | 2.184 ms | 3.217 ms | 10.266 ms | 5.912 ms | 6964 kB |
| transport-live-r2 | pass | 200 | 200 | 0 | 1.109 ms | 2.577 ms | 4.137 ms | 10.234 ms | 8.550 ms | 7008 kB |
| transport-live-r3 | pass | 200 | 200 | 0 | 1.108 ms | 1.857 ms | 2.833 ms | 10.218 ms | 5.905 ms | 6988 kB |

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
