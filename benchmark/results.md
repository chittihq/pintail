# Pintail analytical benchmark results

Measured 2026-09-24T19:13:54.629Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB);
pintail's per-query memory ceiling is 4 GiB inside its container.
Canonical queries: 15 measured runs after 2 warmups; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-09-24T19:09:20.650Z.
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
| Q1: Full table count | 1,449 ms | 3 ms | 483.0× | 1 ms | 5 ms | 1.67× | yes |
| Q2: Filtered count | 592 ms | 2 ms | 296.0× | 16 ms | 16 ms | 8.00× | yes |
| Q3: Group by status | 35,332 ms | 3 ms | 11777.3× | 47 ms | 47 ms | 15.67× | yes |
| Q4: Region × status breakdown | 13,151 ms | 3 ms | 4383.7× | 198 ms | 195 ms | 65.00× | yes |
| Q5: Monthly revenue (2023) | 5,529 ms | 3 ms | 1843.0× | 32 ms | 36 ms | 12.00× | yes |
| Q6: Top 10 spenders | 886,795 ms | 94 ms | 9434.0× | 177 ms | 179 ms | 1.90× | yes |
| Q7: Regional analytics | 55,329 ms | 3 ms | 18443.0× | 121 ms | 143 ms | 47.67× | yes |
| Q8: Join users + orders | 790,393 ms | 3 ms | 263464.3× | 171 ms | 168 ms | 56.00× | yes |
| **Total** | **1,788,570 ms** | **114 ms** | **15689.2×** | **763 ms** | **789 ms** | **6.92×** | |

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
| 1 | 3.9 | 508 ms | 0 | 9 | 179 ms | 0 |
| 4 | 4.9 | 1535 ms | 0 | 3.5 | 2207 ms | 0 |
| 8 | 9.6 | 1824 ms | 0 | 3.2 | 4590 ms | 0 |
| 16 | 16.5 | 2170 ms | 0 | 2.9 | 10430 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 379 ms | 646 ms | 26 | 711 ms | 1108 ms | 6 |
| Q3: Group by status | 840 ms | 1201 ms | 26 | 2833 ms | 4256 ms | 6 |
| Q4: Region × status breakdown | 810 ms | 1147 ms | 26 | 6282 ms | 9038 ms | 6 |
| Q5: Monthly revenue (2023) | 762 ms | 1482 ms | 26 | 2113 ms | 2670 ms | 6 |
| Q6: Top 10 spenders | 1111 ms | 1682 ms | 26 | 6247 ms | 8349 ms | 6 |
| Q7: Regional analytics | 1505 ms | 2644 ms | 26 | 6452 ms | 8710 ms | 6 |
| Q8: Join users + orders | 1380 ms | 2183 ms | 25 | 10430 ms | 13029 ms | 6 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 399 | 3 ms | 0 | 186.2 | 6 ms | 0 |
| 4 | 661.9 | 12 ms | 0 | 232.6 | 53 ms | 0 |
| 8 | 569.5 | 38 ms | 0 | 222 | 69 ms | 0 |
| 16 | 669.8 | 62 ms | 0 | 215.9 | 99 ms | 0 |

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
| Q1: Full table count | 1,449 ms | 3 ms | 1 ms | 1 ms | 5 ms | 1.67× |
| Q2: Filtered count | 592 ms | 45 ms | 43 ms | 17 ms | 16 ms | 0.36× |
| Q3: Group by status | 35,332 ms | 165 ms | 159 ms | 54 ms | 48 ms | 0.29× |
| Q4: Region × status breakdown | 13,151 ms | 185 ms | 190 ms | 154 ms | 156 ms | 0.84× |
| Q5: Monthly revenue (2023) | 5,529 ms | 104 ms | 98 ms | 37 ms | 40 ms | 0.38× |
| Q6: Top 10 spenders | 886,795 ms | 486 ms | 487 ms | 170 ms | 171 ms | 0.35× |
| Q7: Regional analytics | 55,329 ms | 444 ms | 455 ms | 121 ms | 151 ms | 0.34× |
| Q8: Join users + orders | 790,393 ms | 321 ms | 309 ms | 182 ms | 174 ms | 0.54× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,094 ms | 3 ms | 364.7× | 50 ms | 42 ms | 14.00× | yes |
| N2: Group by region (novel group column) | 13,441 ms | 210 ms | 64.0× | 88 ms | 73 ms | 0.35× | yes |
| N3: Monthly revenue, novel year | 8,687 ms | 3 ms | 2895.7× | 36 ms | 37 ms | 12.33× | yes |
| N4: Regional analytics, novel range | 56,051 ms | 446 ms | 125.7× | 129 ms | 172 ms | 0.39× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | 0% | 1,541 MB |
| Q2: Filtered count | n/a | n/a | 0% | 330 MB | 100% | 1,541 MB |
| Q3: Group by status | n/a | n/a | 2% | 324 MB | 87% | 1,541 MB |
| Q4: Region × status breakdown | n/a | n/a | 709% | 466 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | n/a | n/a | 246% | 366 MB | 110% | 1,553 MB |
| Q6: Top 10 spenders | 282% | 274 MB | 695% | 581 MB | 15% | 1,554 MB |
| Q7: Regional analytics | n/a | n/a | 579% | 459 MB | 64% | 1,553 MB |
| Q8: Join users + orders | n/a | n/a | 597% | 647 MB | 16% | 1,708 MB |

