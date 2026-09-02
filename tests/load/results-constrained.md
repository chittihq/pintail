# Pintail concurrency load results (constrained)

Measured 2026-09-02T05:13:22.582Z.

Per-query memory ceiling: 64 MB. Process budget: 512 MB. Admission: 16 concurrent.
Seed rows: 200000. Queries per client per level: 10. Connections: one per query. Side-loads: cdc, dashboard, http. RSS ceiling: 1024 MB.

| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors | HTTP queries | Dashboard | CDC |
|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| 16 | 160 | 0 | 35 | 627 | 706 | 740 | 730 | — | 53 ok / 0 failed, p99 724ms | 72 ok / 0 failed, p99 31ms | 800 rows, converged in 1524ms |
| 64 | 622 | 18 | 1387 | 2018 | 2201 | 2377 | 760 | admission-refused×18 | 180 ok / 8 failed, p99 2353ms (admission-refused×8) | 572 ok / 0 failed, p99 26ms | 6400 rows, converged in 7ms |
| 128 | 971 | 309 | 1985 | 2399 | 2523 | 2606 | 727 | admission-refused×309 | 310 ok / 81 failed, p99 2500ms (admission-refused×81) | 809 ok / 0 failed, p99 27ms | 8200 rows, converged in 3617ms |
