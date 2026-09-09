# Pintail analytical benchmark results

Measured 2026-09-09T19:10:35.125Z with 20,000,000 orders.

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
| Q2: Filtered count | 586 ms | 4 ms | 146.5× | 15 ms | 15 ms | 3.75× | yes |
| Q3: Group by status | 34,866 ms | 4 ms | 8716.5× | 49 ms | 48 ms | 12.00× | yes |
| Q4: Region × status breakdown | 13,144 ms | 5 ms | 2628.8× | 232 ms | 231 ms | 46.20× | yes |
| Q5: Monthly revenue (2023) | 5,525 ms | 4 ms | 1381.3× | 34 ms | 35 ms | 8.75× | yes |
| Q6: Top 10 spenders | 897,953 ms | 66 ms | 13605.3× | 179 ms | 177 ms | 2.68× | yes |
| Q7: Regional analytics | 54,231 ms | 5 ms | 10846.2× | 125 ms | 153 ms | 30.60× | yes |
| Q8: Join users + orders | 894,286 ms | 4 ms | 223571.5× | 172 ms | 168 ms | 42.00× | yes |
| **Total** | **1,902,045 ms** | **96 ms** | **19813.0×** | **807 ms** | **832 ms** | **8.67×** | |

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
| 1 | 4.4 | 445 ms | 0 | 8.3 | 236 ms | 0 |
| 4 | 5.4 | 1435 ms | 0 | 3.5 | 2178 ms | 0 |
| 8 | 9.5 | 1914 ms | 0 | 3.2 | 4400 ms | 0 |
| 16 | 18.4 | 2018 ms | 0 | 2.9 | 12140 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 396 ms | 560 ms | 28 | 1404 ms | 1786 ms | 6 |
| Q3: Group by status | 825 ms | 1138 ms | 28 | 3632 ms | 3808 ms | 6 |
| Q4: Region × status breakdown | 657 ms | 1149 ms | 28 | 7141 ms | 9161 ms | 6 |
| Q5: Monthly revenue (2023) | 900 ms | 1377 ms | 28 | 2499 ms | 3604 ms | 6 |
| Q6: Top 10 spenders | 1062 ms | 1582 ms | 28 | 5112 ms | 7899 ms | 6 |
| Q7: Regional analytics | 1363 ms | 2527 ms | 27 | 5479 ms | 9492 ms | 6 |
| Q8: Join users + orders | 1200 ms | 2124 ms | 27 | 12140 ms | 13061 ms | 6 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 226.9 | 5 ms | 0 | 194 | 6 ms | 0 |
| 4 | 580.3 | 9 ms | 0 | 230.4 | 53 ms | 0 |
| 8 | 627.2 | 25 ms | 0 | 223.8 | 69 ms | 0 |
| 16 | 643.5 | 63 ms | 0 | 218.2 | 99 ms | 0 |

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
| Q1: Full table count | 1,454 ms | 4 ms | 2 ms | 1 ms | 5 ms | 1.25× |
| Q2: Filtered count | 586 ms | 47 ms | 45 ms | 16 ms | 16 ms | 0.34× |
| Q3: Group by status | 34,866 ms | 126 ms | 131 ms | 57 ms | 51 ms | 0.40× |
| Q4: Region × status breakdown | 13,144 ms | 157 ms | 151 ms | 233 ms | 235 ms | 1.50× |
| Q5: Monthly revenue (2023) | 5,525 ms | 101 ms | 93 ms | 35 ms | 37 ms | 0.37× |
| Q6: Top 10 spenders | 897,953 ms | 440 ms | 442 ms | 179 ms | 179 ms | 0.41× |
| Q7: Regional analytics | 54,231 ms | 410 ms | 400 ms | 127 ms | 158 ms | 0.39× |
| Q8: Join users + orders | 894,286 ms | 363 ms | 353 ms | 171 ms | 168 ms | 0.46× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,074 ms | 4 ms | 268.5× | 52 ms | 40 ms | 10.00× | yes |
| N2: Group by region (novel group column) | 13,455 ms | 305 ms | 44.1× | 83 ms | 78 ms | 0.26× | yes |
| N3: Monthly revenue, novel year | 8,582 ms | 5 ms | 1716.4× | 33 ms | 35 ms | 7.00× | yes |
| N4: Regional analytics, novel range | 55,518 ms | 407 ms | 136.4× | 127 ms | 170 ms | 0.42× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q2: Filtered count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q3: Group by status | n/a | n/a | 351% | 339 MB | n/a | n/a |
| Q4: Region × status breakdown | n/a | n/a | 699% | 377 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | n/a | n/a | 632% | 377 MB | n/a | n/a |
| Q6: Top 10 spenders | 263% | 300 MB | 712% | 504 MB | n/a | n/a |
| Q7: Regional analytics | n/a | n/a | 698% | 513 MB | n/a | n/a |
| Q8: Join users + orders | n/a | n/a | 694% | 570 MB | n/a | n/a |

