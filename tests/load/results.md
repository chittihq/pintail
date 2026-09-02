# Pintail concurrency load results

Measured 2026-09-02T04:43:50.653Z.

Per-query memory ceiling: 64 MB. Process budget: server default. Admission: server default.
Seed rows: 200000. Queries per client per level: 10. Connections: one per client. Side-loads: none. RSS ceiling: unchecked.

| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors | HTTP queries | Dashboard | CDC |
|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| 1 | 10 | 0 | 72 | 243 | 243 | 243 | 141 | — | — | — | — |
| 16 | 160 | 0 | 3 | 646 | 764 | 771 | 663 | — | — | — | — |
| 64 | 640 | 0 | 301 | 2457 | 2877 | 3357 | 1471 | — | — | — | — |
| 128 | 1267 | 13 | 722 | 2194 | 2897 | 3134 | 1331 | admission-refused×13 | — | — | — |
