# Pintail concurrency load results

Measured 2026-08-09T18:15:01.529Z.

Per-query memory ceiling: 64 MB. Seed rows: 1000000.
Queries per client per level: 6.

| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 32 | 192 | 0 | 12 | 2337 | 2505 | 2587 | 1029 | — |
| 128 | 768 | 0 | 151 | 9117 | 10068 | 10728 | 1162 | — |
| 256 | 1536 | 0 | 537 | 19305 | 22038 | 25467 | 1910 | — |
