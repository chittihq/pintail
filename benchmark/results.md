# Pintail analytical benchmark results

Measured 2026-10-03T06:09:14.254Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB);
pintail's per-query memory ceiling is 4 GiB inside its container.
Canonical queries: 15 measured runs after 2 warmups; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-10-03T06:06:15.028Z.
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
| Q1: Full table count | 739 ms | 1 ms | 739.0× | 1 ms | 3 ms | 3.00× | yes |
| Q2: Filtered count | 298 ms | 1 ms | 298.0× | 9 ms | 9 ms | 9.00× | yes |
| Q3: Group by status | 14,977 ms | 1 ms | 14977.0× | 31 ms | 30 ms | 30.00× | yes |
| Q4: Region × status breakdown | 6,444 ms | 1 ms | 6444.0× | 56 ms | 55 ms | 55.00× | yes |
| Q5: Monthly revenue (2023) | 3,051 ms | 1 ms | 3051.0× | 19 ms | 19 ms | 19.00× | yes |
| Q6: Top 10 spenders | 156,617 ms | 7 ms | 22373.9× | 48 ms | 50 ms | 7.14× | yes |
| Q7: Regional analytics | 21,502 ms | 1 ms | 21502.0× | 46 ms | 54 ms | 54.00× | yes |
| Q8: Join users + orders | 124,596 ms | 1 ms | 124596.0× | 78 ms | 79 ms | 79.00× | yes |
| **Total** | **328,224 ms** | **14 ms** | **23444.6×** | **288 ms** | **299 ms** | **21.36×** | |

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
| 1 | 61.4 | 33 ms | 0 | 23.7 | 78 ms | 0 |
| 4 | 79.4 | 113 ms | 0 | 18.8 | 384 ms | 0 |
| 8 | 132.5 | 140 ms | 0 | 17.1 | 821 ms | 0 |
| 16 | 260.5 | 144 ms | 0 | 13 | 2701 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 37 ms | 71 ms | 375 | 215 ms | 284 ms | 21 |
| Q3: Group by status | 55 ms | 94 ms | 375 | 700 ms | 861 ms | 21 |
| Q4: Region × status breakdown | 53 ms | 94 ms | 375 | 1105 ms | 1487 ms | 21 |
| Q5: Monthly revenue (2023) | 56 ms | 94 ms | 375 | 538 ms | 697 ms | 21 |
| Q6: Top 10 spenders | 71 ms | 131 ms | 375 | 1768 ms | 2689 ms | 21 |
| Q7: Regional analytics | 116 ms | 198 ms | 375 | 1540 ms | 1827 ms | 21 |
| Q8: Join users + orders | 55 ms | 85 ms | 374 | 2624 ms | 3218 ms | 21 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2827.1 | 0 ms | 0 | 340 | 3 ms | 0 |
| 4 | 5875.6 | 1 ms | 0 | 499.3 | 40 ms | 0 |
| 8 | 8952.4 | 1 ms | 0 | 548.5 | 50 ms | 0 |
| 16 | 10727.8 | 2 ms | 0 | 512.6 | 66 ms | 0 |

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
| Q1: Full table count | 739 ms | 1 ms | 1 ms | 1 ms | 3 ms | 3.00× |
| Q2: Filtered count | 298 ms | 3 ms | 2 ms | 9 ms | 9 ms | 3.00× |
| Q3: Group by status | 14,977 ms | 10 ms | 10 ms | 30 ms | 30 ms | 3.00× |
| Q4: Region × status breakdown | 6,444 ms | 14 ms | 13 ms | 56 ms | 56 ms | 4.00× |
| Q5: Monthly revenue (2023) | 3,051 ms | 12 ms | 12 ms | 19 ms | 19 ms | 1.58× |
| Q6: Top 10 spenders | 156,617 ms | 21 ms | 20 ms | 50 ms | 51 ms | 2.43× |
| Q7: Regional analytics | 21,502 ms | 31 ms | 31 ms | 40 ms | 51 ms | 1.65× |
| Q8: Join users + orders | 124,596 ms | 15 ms | 14 ms | 79 ms | 78 ms | 5.20× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 520 ms | 1 ms | 520.0× | 23 ms | 23 ms | 23.00× | yes |
| N2: Group by region (novel group column) | 5,677 ms | 17 ms | 333.9× | 43 ms | 44 ms | 2.59× | yes |
| N3: Monthly revenue, novel year | 3,382 ms | 13 ms | 260.2× | 20 ms | 21 ms | 1.62× | yes |
| N4: Regional analytics, novel range | 20,960 ms | 37 ms | 566.5× | 50 ms | 52 ms | 1.41× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | 2% | 1,547 MB |
| Q2: Filtered count | n/a | n/a | n/a | n/a | 2% | 1,547 MB |
| Q3: Group by status | n/a | n/a | 2% | 358 MB | 94% | 1,547 MB |
| Q4: Region × status breakdown | 10% | 355 MB | 558% | 441 MB | 108% | 1,560 MB |
| Q5: Monthly revenue (2023) | n/a | n/a | 37% | 483 MB | 100% | 1,560 MB |
| Q6: Top 10 spenders | 0% | 329 MB | 443% | 600 MB | 111% | 1,560 MB |
| Q7: Regional analytics | n/a | n/a | 6% | 394 MB | 91% | 1,560 MB |
| Q8: Join users + orders | n/a | n/a | 575% | 687 MB | 56% | 1,713 MB |

