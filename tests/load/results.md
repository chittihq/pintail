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

## Reserved admission on a larger replica

Measured 2026-09-07 using `LOAD_PROFILE=isolation bun run tests/load/run.ts`.
Both binaries used the recovery profile, 16 admission slots, a 64 MiB per-query
memory limit, and a 200,000-row replicated table. Fourteen report clients and
two point-lookup clients each issued 40 queries, after warming the replica.
The baseline was commit `626ea40`; the comparison changes admission costing.
Runs were sequential with separately created source containers.

| Classifier | Point lookups completed | Failed | Point lookup p95 ms | All requests completed | Failed |
|---|---:|---:|---:|---:|---:|
| Whole-database size | 80 | 0 | 16.68 | 640 | 0 |
| Physical query cost | 80 | 0 | 5.26 | 640 | 0 |

Point-lookup p95 decreased by 68.5% in this run. This is one mixed-load sample,
not a latency guarantee; the clients still share execution resources. The
harness reports point latency separately in `results-isolation.json` and
`results-isolation.md`, including failed lookup counts.
