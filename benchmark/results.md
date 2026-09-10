# Pintail analytical benchmark results

Measured 2026-09-10T07:30:19.953Z with 20,000,000 orders.

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
| Q1: Full table count | 1,454 ms | 5 ms | 290.8× | 1 ms | 5 ms | 1.00× | yes |
| Q2: Filtered count | 586 ms | 5 ms | 117.2× | 16 ms | 16 ms | 3.20× | yes |
| Q3: Group by status | 34,866 ms | 5 ms | 6973.2× | 49 ms | 48 ms | 9.60× | yes |
| Q4: Region × status breakdown | 13,144 ms | 5 ms | 2628.8× | 230 ms | 230 ms | 46.00× | yes |
| Q5: Monthly revenue (2023) | 5,525 ms | 5 ms | 1105.0× | 33 ms | 36 ms | 7.20× | yes |
| Q6: Top 10 spenders | 897,953 ms | 68 ms | 13205.2× | 175 ms | 176 ms | 2.59× | yes |
| Q7: Regional analytics | 54,231 ms | 5 ms | 10846.2× | 127 ms | 154 ms | 30.80× | yes |
| Q8: Join users + orders | 894,286 ms | 5 ms | 178857.2× | 171 ms | 168 ms | 33.60× | yes |
| **Total** | **1,902,045 ms** | **103 ms** | **18466.5×** | **802 ms** | **833 ms** | **8.09×** | |

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
| 1 | 4.1 | 456 ms | 0 | 8.3 | 235 ms | 0 |
| 4 | 4.9 | 1520 ms | 0 | 3.5 | 2091 ms | 0 |
| 8 | 9.4 | 1851 ms | 0 | 3.2 | 4215 ms | 0 |
| 16 | 17.3 | 2032 ms | 0 | 2.9 | 10662 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 394 ms | 602 ms | 27 | 1429 ms | 1688 ms | 6 |
| Q3: Group by status | 777 ms | 1079 ms | 27 | 3905 ms | 4679 ms | 6 |
| Q4: Region × status breakdown | 762 ms | 1103 ms | 27 | 6667 ms | 9300 ms | 6 |
| Q5: Monthly revenue (2023) | 904 ms | 1251 ms | 27 | 2643 ms | 2771 ms | 6 |
| Q6: Top 10 spenders | 1093 ms | 1764 ms | 27 | 6518 ms | 7880 ms | 6 |
| Q7: Regional analytics | 1256 ms | 2302 ms | 27 | 6289 ms | 7535 ms | 6 |
| Q8: Join users + orders | 1427 ms | 2216 ms | 26 | 10662 ms | 12162 ms | 6 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 214.2 | 5 ms | 0 | 189.6 | 6 ms | 0 |
| 4 | 581.7 | 9 ms | 0 | 227.1 | 54 ms | 0 |
| 8 | 641.1 | 38 ms | 0 | 216.6 | 71 ms | 0 |
| 16 | 642 | 55 ms | 0 | 209.7 | 104 ms | 0 |

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
| Q2: Filtered count | 586 ms | 50 ms | 48 ms | 17 ms | 16 ms | 0.32× |
| Q3: Group by status | 34,866 ms | 129 ms | 129 ms | 72 ms | 51 ms | 0.40× |
| Q4: Region × status breakdown | 13,144 ms | 155 ms | 151 ms | 234 ms | 234 ms | 1.51× |
| Q5: Monthly revenue (2023) | 5,525 ms | 107 ms | 99 ms | 34 ms | 37 ms | 0.35× |
| Q6: Top 10 spenders | 897,953 ms | 446 ms | 444 ms | 179 ms | 181 ms | 0.41× |
| Q7: Regional analytics | 54,231 ms | 398 ms | 403 ms | 127 ms | 158 ms | 0.40× |
| Q8: Join users + orders | 894,286 ms | 360 ms | 357 ms | 172 ms | 166 ms | 0.46× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,074 ms | 5 ms | 214.8× | 52 ms | 41 ms | 8.20× | yes |
| N2: Group by region (novel group column) | 13,455 ms | 303 ms | 44.4× | 81 ms | 75 ms | 0.25× | yes |
| N3: Monthly revenue, novel year | 8,582 ms | 6 ms | 1430.3× | 34 ms | 39 ms | 6.50× | yes |
| N4: Regional analytics, novel range | 55,518 ms | 414 ms | 134.1× | 130 ms | 171 ms | 0.41× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q2: Filtered count | n/a | n/a | 0% | 288 MB | n/a | n/a |
| Q3: Group by status | n/a | n/a | 370% | 317 MB | n/a | n/a |
| Q4: Region × status breakdown | n/a | n/a | 698% | 357 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | n/a | n/a | 587% | 353 MB | n/a | n/a |
| Q6: Top 10 spenders | 270% | 280 MB | 704% | 508 MB | n/a | n/a |
| Q7: Regional analytics | n/a | n/a | 674% | 459 MB | n/a | n/a |
| Q8: Join users + orders | n/a | n/a | 691% | 591 MB | n/a | n/a |

