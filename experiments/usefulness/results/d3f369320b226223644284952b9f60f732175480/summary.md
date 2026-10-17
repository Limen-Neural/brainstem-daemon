# Usefulness experiment summary — `d3f369320b226223644284952b9f60f732175480`

Decision (derived): **supports_further_development**

## In-process runs (latency after 1,000 warm-up ticks; measured)

| run | checks | p50 | p99 | max | decode p50 | ticks/s | peak RSS | nonempty ticks |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| baseline-r1 | pass | 931 ns | 1.21 µs | 11.27 µs | — | 987,730 | 4804 kB | 150 |
| baseline-r2 | pass | 982 ns | 1.45 µs | 30.63 µs | — | 897,581 | 4924 kB | 150 |
| baseline-r3 | pass | 942 ns | 1.30 µs | 30.82 µs | — | 969,112 | 4788 kB | 150 |
| baseline-r4 | pass | 932 ns | 1.22 µs | 8.51 µs | — | 987,473 | 4844 kB | 150 |
| baseline-r5 | pass | 932 ns | 1.40 µs | 14.32 µs | — | 985,738 | 4764 kB | 150 |
| daemon-live-r1 | pass | 1.22 µs | 2.98 µs | 54.61 µs | 1.10 µs | 743,469 | 6192 kB | 150 |
| daemon-live-r2 | pass | 1.17 µs | 2.98 µs | 19.56 µs | 1.13 µs | 744,031 | 6184 kB | 150 |
| daemon-live-r3 | pass | 1.13 µs | 2.94 µs | 85.84 µs | 1.11 µs | 740,705 | 6192 kB | 150 |
| daemon-live-r4 | pass | 1.06 µs | 2.92 µs | 1.702 ms | 1.14 µs | 761,976 | 6160 kB | 150 |
| daemon-live-r5 | pass | 1.23 µs | 2.98 µs | 1.723 ms | 1.16 µs | 700,255 | 6160 kB | 150 |
| daemon-simulation-r1 | pass | 581 ns | 1.64 µs | 7.27 µs | 1.02 µs | 1,389,007 | 5660 kB | 0 |
| daemon-simulation-r2 | pass | 621 ns | 1.77 µs | 20.21 µs | 1.05 µs | 1,202,718 | 5676 kB | 0 |
| daemon-simulation-r3 | pass | 631 ns | 1.79 µs | 42.84 µs | 1.01 µs | 1,181,639 | 5632 kB | 0 |
| daemon-simulation-r4 | pass | 581 ns | 1.71 µs | 17.16 µs | 992 ns | 1,372,953 | 5652 kB | 0 |
| daemon-simulation-r5 | pass | 581 ns | 1.62 µs | 7.89 µs | 1.02 µs | 1,388,299 | 5640 kB | 0 |

## Transport runs (measured)

| run | checks | sent | matched | lost | e2e p50 | e2e p99 | e2e max | ready after | SIGTERM→exit | daemon peak RSS |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| transport-live-r1 | pass | 200 | 200 | 0 | 1.125 ms | 1.325 ms | 2.159 ms | 10.219 ms | 6.715 ms | 6996 kB |
| transport-live-r2 | pass | 200 | 200 | 0 | 1.113 ms | 2.022 ms | 4.056 ms | 10.230 ms | 6.054 ms | 6920 kB |
| transport-live-r3 | pass | 200 | 200 | 0 | 1.113 ms | 1.232 ms | 2.468 ms | 10.248 ms | 6.331 ms | 6976 kB |

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
