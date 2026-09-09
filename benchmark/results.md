# Pintail analytical benchmark results

Measured 2026-09-09T13:39:21.078Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB);
pintail's per-query memory ceiling is 4 GiB inside its container.
Canonical queries: 15 measured runs after 2 warmups; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-09-09T13:34:50.971Z.
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
| Q1: Full table count | 1,454 ms | 4 ms | 363.5× | 1 ms | 5 ms | 1.25× | yes |
| Q2: Filtered count | 586 ms | 5 ms | 117.2× | 16 ms | 16 ms | 3.20× | yes |
| Q3: Group by status | 34,866 ms | 5 ms | 6973.2× | 50 ms | 49 ms | 9.80× | yes |
| Q4: Region × status breakdown | 13,144 ms | 5 ms | 2628.8× | 232 ms | 233 ms | 46.60× | yes |
| Q5: Monthly revenue (2023) | 5,525 ms | 5 ms | 1105.0× | 35 ms | 35 ms | 7.00× | yes |
| Q6: Top 10 spenders | 897,953 ms | 67 ms | 13402.3× | 166 ms | 166 ms | 2.48× | yes |
| Q7: Regional analytics | 54,231 ms | 5 ms | 10846.2× | 117 ms | 142 ms | 28.40× | yes |
| Q8: Join users + orders | 894,286 ms | 5 ms | 178857.2× | 170 ms | 167 ms | 33.40× | yes |
| **Total** | **1,902,045 ms** | **101 ms** | **18832.1×** | **787 ms** | **813 ms** | **8.05×** | |

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
| 1 | 4.4 | 443 ms | 0 | 8.8 | 193 ms | 0 |
| 4 | 5.5 | 1418 ms | 0 | 3.5 | 2013 ms | 0 |
| 8 | 10.4 | 1808 ms | 0 | 3.2 | 4810 ms | 0 |
| 16 | 18.3 | 2035 ms | 0 | 3 | 11872 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 403 ms | 584 ms | 28 | 1113 ms | 1695 ms | 7 |
| Q3: Group by status | 820 ms | 1170 ms | 28 | 2598 ms | 3176 ms | 6 |
| Q4: Region × status breakdown | 779 ms | 1051 ms | 28 | 7885 ms | 9484 ms | 6 |
| Q5: Monthly revenue (2023) | 756 ms | 1265 ms | 28 | 2572 ms | 3242 ms | 6 |
| Q6: Top 10 spenders | 963 ms | 1651 ms | 28 | 5244 ms | 7877 ms | 6 |
| Q7: Regional analytics | 1263 ms | 2466 ms | 28 | 5999 ms | 6419 ms | 6 |
| Q8: Join users + orders | 1026 ms | 2035 ms | 27 | 11872 ms | 13483 ms | 6 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 212.2 | 5 ms | 0 | 190.3 | 6 ms | 0 |
| 4 | 573 | 9 ms | 0 | 237.1 | 52 ms | 0 |
| 8 | 635.1 | 39 ms | 0 | 218.8 | 71 ms | 0 |
| 16 | 651.1 | 55 ms | 0 | 213.4 | 104 ms | 0 |

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
| Q1: Full table count | 1,454 ms | 5 ms | 3 ms | 1 ms | 5 ms | 1.00× |
| Q2: Filtered count | 586 ms | 49 ms | 47 ms | 16 ms | 15 ms | 0.31× |
| Q3: Group by status | 34,866 ms | 122 ms | 123 ms | 49 ms | 49 ms | 0.40× |
| Q4: Region × status breakdown | 13,144 ms | 144 ms | 157 ms | 187 ms | 186 ms | 1.29× |
| Q5: Monthly revenue (2023) | 5,525 ms | 97 ms | 98 ms | 35 ms | 40 ms | 0.41× |
| Q6: Top 10 spenders | 897,953 ms | 438 ms | 438 ms | 166 ms | 166 ms | 0.38× |
| Q7: Regional analytics | 54,231 ms | 403 ms | 395 ms | 119 ms | 146 ms | 0.36× |
| Q8: Join users + orders | 894,286 ms | 351 ms | 349 ms | 174 ms | 171 ms | 0.49× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,074 ms | 5 ms | 214.8× | 49 ms | 44 ms | 8.80× | yes |
| N2: Group by region (novel group column) | 13,455 ms | 306 ms | 44.0× | 82 ms | 75 ms | 0.25× | yes |
| N3: Monthly revenue, novel year | 8,582 ms | 5 ms | 1716.4× | 32 ms | 38 ms | 7.60× | yes |
| N4: Regional analytics, novel range | 55,518 ms | 411 ms | 135.1× | 125 ms | 170 ms | 0.41× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | 0% | 1,541 MB |
| Q2: Filtered count | n/a | n/a | 0% | 299 MB | 101% | 1,541 MB |
| Q3: Group by status | n/a | n/a | 284% | 322 MB | 87% | 1,542 MB |
| Q4: Region × status breakdown | n/a | n/a | 711% | 379 MB | 107% | 1,554 MB |
| Q5: Monthly revenue (2023) | n/a | n/a | 381% | 332 MB | 110% | 1,554 MB |
| Q6: Top 10 spenders | 288% | 224 MB | 657% | 692 MB | 19% | 1,555 MB |
| Q7: Regional analytics | n/a | n/a | 572% | 462 MB | 64% | 1,554 MB |
| Q8: Join users + orders | n/a | n/a | 605% | 633 MB | 15% | 1,708 MB |

