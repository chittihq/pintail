# Pintail analytical benchmark results

Measured 2026-09-10T07:51:56.721Z with 20,000,000 orders.

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
| Q1: Full table count | 1,437 ms | 4 ms | 359.3× | 1 ms | 5 ms | 1.25× | yes |
| Q2: Filtered count | 587 ms | 4 ms | 146.8× | 15 ms | 14 ms | 3.50× | yes |
| Q3: Group by status | 34,398 ms | 5 ms | 6879.6× | 47 ms | 47 ms | 9.40× | yes |
| Q4: Region × status breakdown | 13,054 ms | 4 ms | 3263.5× | 183 ms | 178 ms | 44.50× | yes |
| Q5: Monthly revenue (2023) | 5,462 ms | 5 ms | 1092.4× | 32 ms | 36 ms | 7.20× | yes |
| Q6: Top 10 spenders | 889,417 ms | 67 ms | 13274.9× | 132 ms | 133 ms | 1.99× | yes |
| Q7: Regional analytics | 53,410 ms | 7 ms | 7630.0× | 104 ms | 128 ms | 18.29× | yes |
| Q8: Join users + orders | 796,769 ms | 4 ms | 199192.3× | 145 ms | 145 ms | 36.25× | yes |
| **Total** | **1,794,534 ms** | **100 ms** | **17945.3×** | **659 ms** | **686 ms** | **6.86×** | |

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
| 1 | 4.2 | 449 ms | 0 | 10 | 184 ms | 0 |
| 4 | 6.6 | 1319 ms | 0 | 4.6 | 1507 ms | 0 |
| 8 | 10.5 | 1721 ms | 0 | 4.3 | 3025 ms | 0 |
| 16 | 19.5 | 1857 ms | 0 | 4 | 8362 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 362 ms | 514 ms | 30 | 1027 ms | 1520 ms | 8 |
| Q3: Group by status | 691 ms | 1046 ms | 30 | 1989 ms | 3606 ms | 8 |
| Q4: Region × status breakdown | 682 ms | 1057 ms | 30 | 5381 ms | 7428 ms | 8 |
| Q5: Monthly revenue (2023) | 748 ms | 1333 ms | 30 | 2187 ms | 2626 ms | 7 |
| Q6: Top 10 spenders | 911 ms | 1526 ms | 29 | 3965 ms | 6194 ms | 7 |
| Q7: Regional analytics | 1327 ms | 2294 ms | 29 | 4499 ms | 5993 ms | 7 |
| Q8: Join users + orders | 1187 ms | 1861 ms | 29 | 6592 ms | 10208 ms | 7 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 232.7 | 5 ms | 0 | 193 | 6 ms | 0 |
| 4 | 583.3 | 10 ms | 0 | 231.5 | 52 ms | 0 |
| 8 | 655.6 | 39 ms | 0 | 206 | 72 ms | 0 |
| 16 | 643 | 56 ms | 0 | 220.9 | 99 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

Pintail (wire) reaches the same query over Pintail's MySQL wire
protocol - the path a BI tool actually uses - timed beside the
HTTP call, not instead of it; the gap between the two is HTTP's
own fixed cost (auth, JSON, connection setup).

| Query | MySQL | Pintail (no memo) | Pintail (wire) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,437 ms | 4 ms | 2 ms | 1 ms | 5 ms | 1.25× |
| Q2: Filtered count | 587 ms | 44 ms | 43 ms | 15 ms | 16 ms | 0.36× |
| Q3: Group by status | 34,398 ms | 131 ms | 158 ms | 52 ms | 51 ms | 0.39× |
| Q4: Region × status breakdown | 13,054 ms | 158 ms | 150 ms | 183 ms | 185 ms | 1.17× |
| Q5: Monthly revenue (2023) | 5,462 ms | 94 ms | 94 ms | 36 ms | 36 ms | 0.38× |
| Q6: Top 10 spenders | 889,417 ms | 407 ms | 406 ms | 134 ms | 136 ms | 0.33× |
| Q7: Regional analytics | 53,410 ms | 370 ms | 366 ms | 101 ms | 127 ms | 0.34× |
| Q8: Join users + orders | 796,769 ms | 370 ms | 346 ms | 147 ms | 149 ms | 0.40× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,125 ms | 4 ms | 281.3× | 46 ms | 40 ms | 10.00× | yes |
| N2: Group by region (novel group column) | 12,949 ms | 307 ms | 42.2× | 78 ms | 79 ms | 0.26× | yes |
| N3: Monthly revenue, novel year | 8,226 ms | 4 ms | 2056.5× | 32 ms | 36 ms | 9.00× | yes |
| N4: Regional analytics, novel range | 56,145 ms | 369 ms | 152.2× | 108 ms | 138 ms | 0.37× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q2: Filtered count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q3: Group by status | n/a | n/a | 0% | 348 MB | n/a | n/a |
| Q4: Region × status breakdown | n/a | n/a | 695% | 414 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | n/a | n/a | 562% | 381 MB | n/a | n/a |
| Q6: Top 10 spenders | 177% | 380 MB | 700% | 582 MB | n/a | n/a |
| Q7: Regional analytics | n/a | n/a | 632% | 526 MB | n/a | n/a |
| Q8: Join users + orders | n/a | n/a | 691% | 573 MB | n/a | n/a |

