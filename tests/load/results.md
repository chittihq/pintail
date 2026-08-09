# Pintail concurrency load results

Measured 2026-08-09T18:59:41.071Z.

Per-query memory ceiling: 64 MB. Seed rows: 1000000.
Queries per client per level: 6.

| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 32 | 192 | 0 | 14 | 2209 | 2365 | 2570 | 1185 | — |
| 128 | 529 | 239 | 1081 | 3872 | 4844 | 5060 | 1213 | admission-refused×239 |
| 256 | 699 | 837 | 1819 | 4832 | 5314 | 5746 | 1213 | admission-refused×837 |
