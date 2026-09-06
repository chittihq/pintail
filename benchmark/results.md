# Pintail analytical benchmark results

Measured 2026-09-06T17:35:43.854Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB);
pintail's per-query memory ceiling is 4 GiB inside its container.
Canonical queries: 15 measured runs after 2 warmups; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-09-03T08:22:24.512Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty. It is charged WITHOUT a live
update tail (the snapshot is fully merged before the timed queries), so it
is a lower bound on ClickHouse's merge-on-read cost; issue #31 tracks the
phase that keeps writes flowing while the queries run.

NOT like for like: the canonical table is served from pintail's settled
aggregate memo, while ClickHouse's query cache is off and it executes every
run. It measures what a repeated dashboard query costs, not engine speed.
The novel-query table below is the engine-speed comparison - both engines
execute there, and ClickHouse is currently faster.

> Historical evidence warning: the 2026-08-11 run banked by `de974db` is
> withdrawn. Pintail minima regressed on Q1/Q3 while unchanged MySQL and
> ClickHouse controls did not, so the repository's host-noise rule did not apply.
> The current artifact supersedes it; the harness now rejects that signature.

## Repeated queries (memo-served — dashboard refresh cost, not engine speed)

| Query | MySQL | Pintail (memo) | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,437 ms | 13 ms | 110.5× | 11 ms | 14 ms | 1.08× | yes |
| Q2: Filtered count | 587 ms | 12 ms | 48.9× | 26 ms | 29 ms | 2.42× | yes |
| Q3: Group by status | 34,398 ms | 13 ms | 2646.0× | 62 ms | 61 ms | 4.69× | yes |
| Q4: Region × status breakdown | 13,054 ms | 14 ms | 932.4× | 202 ms | 195 ms | 13.93× | yes |
| Q5: Monthly revenue (2023) | 5,462 ms | 20 ms | 273.1× | 43 ms | 49 ms | 2.45× | yes |
| Q6: Top 10 spenders | 889,417 ms | 75 ms | 11858.9× | 148 ms | 146 ms | 1.95× | yes |
| Q7: Regional analytics | 53,410 ms | 13 ms | 4108.5× | 112 ms | 141 ms | 10.85× | yes |
| Q8: Join users + orders | 796,769 ms | 12 ms | 66397.4× | 178 ms | 178 ms | 14.83× | yes |
| **Total** | **1,794,534 ms** | **172 ms** | **10433.3×** | **782 ms** | **813 ms** | **4.73×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work. The mixed workload
round-robins Q2 through Q8 per call across all clients, so no client
is pinned to one shape; the single-query row is the full-table count
alone, the cheapest shape, kept as a ceiling on request rate.

### mixed Q2–Q8

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 3.6 | 535 ms | 0 | 7.2 | 355 ms | 0 |
| 4 | 5 | 1289 ms | 0 | 4.7 | 1495 ms | 0 |
| 8 | 4.5 | 2767 ms | 0 | 4.3 | 2998 ms | 0 |
| 16 | 4.5 | 6320 ms | 0 | 4.1 | 8263 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 1815 ms | 2677 ms | 8 | 1364 ms | 1803 ms | 8 |
| Q3: Group by status | 3147 ms | 4056 ms | 8 | 2750 ms | 3980 ms | 8 |
| Q4: Region × status breakdown | 3671 ms | 4206 ms | 8 | 5460 ms | 8294 ms | 8 |
| Q5: Monthly revenue (2023) | 3186 ms | 3310 ms | 8 | 2103 ms | 2892 ms | 8 |
| Q6: Top 10 spenders | 3171 ms | 3648 ms | 8 | 4022 ms | 4513 ms | 8 |
| Q7: Regional analytics | 5828 ms | 6699 ms | 8 | 4439 ms | 5306 ms | 7 |
| Q8: Join users + orders | 3697 ms | 4109 ms | 7 | 7868 ms | 8596 ms | 7 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 22.4 | 195 ms | 0 | 26.9 | 153 ms | 0 |
| 4 | 131.4 | 146 ms | 0 | 98 | 150 ms | 0 |
| 8 | 246.6 | 150 ms | 0 | 190.4 | 131 ms | 0 |
| 16 | 317.1 | 176 ms | 0 | 224 | 159 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,437 ms | 29 ms | 33 ms | 57 ms | 1.97× |
| Q2: Filtered count | 587 ms | 87 ms | 44 ms | 218 ms | 2.51× |
| Q3: Group by status | 34,398 ms | 194 ms | 64 ms | 101 ms | 0.52× |
| Q4: Region × status breakdown | 13,054 ms | 193 ms | 188 ms | 203 ms | 1.05× |
| Q5: Monthly revenue (2023) | 5,462 ms | 135 ms | 42 ms | 49 ms | 0.36× |
| Q6: Top 10 spenders | 889,417 ms | 420 ms | 151 ms | 158 ms | 0.38× |
| Q7: Regional analytics | 53,410 ms | 405 ms | 110 ms | 140 ms | 0.35× |
| Q8: Join users + orders | 796,769 ms | 524 ms | 171 ms | 194 ms | 0.37× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,125 ms | 256 ms | 4.4× | 53 ms | 60 ms | 0.23× | yes |
| N2: Group by region (novel group column) | 12,949 ms | 436 ms | 29.7× | 92 ms | 165 ms | 0.38× | yes |
| N3: Monthly revenue, novel year | 8,226 ms | 152 ms | 54.1× | 126 ms | 66 ms | 0.43× | yes |
| N4: Regional analytics, novel range | 56,145 ms | 561 ms | 100.1× | 280 ms | 587 ms | 1.05× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 2% | 276 MB | n/a | n/a |
| Q2: Filtered count | 0% | 31 MB | 2% | 300 MB | n/a | n/a |
| Q3: Group by status | 0% | 34 MB | 106% | 330 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 34 MB | 701% | 431 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 1% | 36 MB | 26% | 322 MB | n/a | n/a |
| Q6: Top 10 spenders | 39% | 115 MB | 574% | 506 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 57 MB | 544% | 413 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 59 MB | 590% | 593 MB | n/a | n/a |

