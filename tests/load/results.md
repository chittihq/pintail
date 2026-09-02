# Pintail concurrency load results

Measured 2026-09-02T04:24:16.954Z.

Per-query memory ceiling: 64 MB. Seed rows: 200000.
Queries per client per level: 10.

| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 10 | 0 | 30 | 119 | 119 | 119 | 147 | — |
| 16 | 160 | 0 | 5 | 573 | 652 | 669 | 708 | — |
| 64 | 640 | 0 | 227 | 1852 | 2295 | 2731 | 1431 | — |
| 128 | 1259 | 21 | 761 | 2182 | 2976 | 3659 | 1349 | admission-refused×21 |
