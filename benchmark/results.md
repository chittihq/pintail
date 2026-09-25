# Pintail analytical benchmark results

Measured 2026-09-25T18:17:48.739Z with 20,000,000 orders.

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
| Q2: Filtered count | 592 ms | 2 ms | 296.0× | 15 ms | 15 ms | 7.50× | yes |
| Q3: Group by status | 35,332 ms | 3 ms | 11777.3× | 47 ms | 47 ms | 15.67× | yes |
| Q4: Region × status breakdown | 13,151 ms | 3 ms | 4383.7× | 196 ms | 190 ms | 63.33× | yes |
| Q5: Monthly revenue (2023) | 5,529 ms | 3 ms | 1843.0× | 33 ms | 35 ms | 11.67× | yes |
| Q6: Top 10 spenders | 886,795 ms | 67 ms | 13235.7× | 181 ms | 179 ms | 2.67× | yes |
| Q7: Regional analytics | 55,329 ms | 3 ms | 18443.0× | 126 ms | 150 ms | 50.00× | yes |
| Q8: Join users + orders | 790,393 ms | 3 ms | 263464.3× | 174 ms | 170 ms | 56.67× | yes |
| **Total** | **1,788,570 ms** | **87 ms** | **20558.3×** | **773 ms** | **791 ms** | **9.09×** | |

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
| 1 | 4 | 477 ms | 0 | 8.7 | 200 ms | 0 |
| 4 | 5 | 1538 ms | 0 | 3.6 | 2105 ms | 0 |
| 8 | 9.1 | 2001 ms | 0 | 3.3 | 4123 ms | 0 |
| 16 | 17.5 | 2072 ms | 0 | 3 | 10100 ms | 0 |

Per query at 16 clients (every level is in results.json):

| Query | Pintail median | Pintail p95 | Pintail done | CH median | CH p95 | CH done |
|---|---:|---:|---:|---:|---:|---:|
| Q2: Filtered count | 378 ms | 455 ms | 28 | 1139 ms | 2304 ms | 6 |
| Q3: Group by status | 899 ms | 1139 ms | 28 | 3069 ms | 3406 ms | 6 |
| Q4: Region × status breakdown | 843 ms | 1272 ms | 28 | 6265 ms | 6612 ms | 6 |
| Q5: Monthly revenue (2023) | 702 ms | 1430 ms | 28 | 2195 ms | 3806 ms | 6 |
| Q6: Top 10 spenders | 1140 ms | 1672 ms | 28 | 6391 ms | 7290 ms | 6 |
| Q7: Regional analytics | 1350 ms | 2451 ms | 27 | 7267 ms | 8989 ms | 6 |
| Q8: Join users + orders | 1167 ms | 2091 ms | 27 | 10100 ms | 12288 ms | 5 |

### Q1: Full table count

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 391 | 3 ms | 0 | 184.7 | 6 ms | 0 |
| 4 | 648.9 | 11 ms | 0 | 232.3 | 52 ms | 0 |
| 8 | 677.1 | 37 ms | 0 | 226.4 | 68 ms | 0 |
| 16 | 658.4 | 54 ms | 0 | 216.1 | 99 ms | 0 |

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
| Q1: Full table count | 1,449 ms | 2 ms | 1 ms | 1 ms | 5 ms | 2.50× |
| Q2: Filtered count | 592 ms | 43 ms | 41 ms | 17 ms | 17 ms | 0.40× |
| Q3: Group by status | 35,332 ms | 173 ms | 164 ms | 50 ms | 48 ms | 0.28× |
| Q4: Region × status breakdown | 13,151 ms | 194 ms | 182 ms | 197 ms | 196 ms | 1.01× |
| Q5: Monthly revenue (2023) | 5,529 ms | 104 ms | 102 ms | 34 ms | 38 ms | 0.37× |
| Q6: Top 10 spenders | 886,795 ms | 473 ms | 469 ms | 182 ms | 186 ms | 0.39× |
| Q7: Regional analytics | 55,329 ms | 447 ms | 446 ms | 138 ms | 154 ms | 0.34× |
| Q8: Join users + orders | 790,393 ms | 310 ms | 299 ms | 174 ms | 170 ms | 0.55× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,094 ms | 3 ms | 364.7× | 54 ms | 40 ms | 13.33× | yes |
| N2: Group by region (novel group column) | 13,441 ms | 211 ms | 63.7× | 92 ms | 77 ms | 0.36× | yes |
| N3: Monthly revenue, novel year | 8,687 ms | 97 ms | 89.6× | 36 ms | 43 ms | 0.44× | yes |
| N4: Regional analytics, novel range | 56,051 ms | 500 ms | 112.1× | 128 ms | 165 ms | 0.33× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled from one long-lived `docker stats` stream per container
at the daemon's own update cadence while each engine ran. MySQL shows
n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | n/a | n/a | n/a | n/a | n/a | n/a |
| Q2: Filtered count | n/a | n/a | 0% | 292 MB | n/a | n/a |
| Q3: Group by status | 1% | 44 MB | 446% | 316 MB | n/a | n/a |
| Q4: Region × status breakdown | n/a | n/a | 693% | 347 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | n/a | n/a | 581% | 351 MB | n/a | n/a |
| Q6: Top 10 spenders | 190% | 389 MB | 713% | 524 MB | n/a | n/a |
| Q7: Regional analytics | n/a | n/a | 696% | 459 MB | n/a | n/a |
| Q8: Join users + orders | n/a | n/a | 686% | 520 MB | n/a | n/a |

