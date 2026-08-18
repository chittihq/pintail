# Pintail analytical benchmark results

Measured 2026-08-18T08:32:17.049Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-16T18:01:03.714Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty.

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
| Q1: Full table count | 1,563 ms | 13 ms | 120.2× | 11 ms | 12 ms | 0.92× | yes |
| Q2: Filtered count | 587 ms | 12 ms | 48.9× | 32 ms | 30 ms | 2.50× | yes |
| Q3: Group by status | 35,782 ms | 14 ms | 2555.9× | 64 ms | 69 ms | 4.93× | yes |
| Q4: Region × status breakdown | 13,290 ms | 12 ms | 1107.5× | 248 ms | 266 ms | 22.17× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 14 ms | 397.3× | 40 ms | 46 ms | 3.29× | yes |
| Q6: Top 10 spenders | 810,282 ms | 99 ms | 8184.7× | 259 ms | 254 ms | 2.57× | yes |
| Q7: Regional analytics | 57,104 ms | 12 ms | 4758.7× | 145 ms | 181 ms | 15.08× | yes |
| Q8: Join users + orders | 834,387 ms | 12 ms | 69532.3× | 204 ms | 202 ms | 16.83× | yes |
| **Total** | **1,758,557 ms** | **188 ms** | **9354.0×** | **1,003 ms** | **1,060 ms** | **5.64×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 22.2 | 162 ms | 0 | 24.7 | 155 ms | 0 |
| 4 | 115.8 | 152 ms | 0 | 157.6 | 105 ms | 0 |
| 8 | 227.9 | 160 ms | 0 | 276.7 | 105 ms | 0 |
| 16 | 260.8 | 193 ms | 0 | 217.8 | 241 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 97 ms | 10 ms | 12 ms | 0.12× |
| Q2: Filtered count | 587 ms | 77 ms | 29 ms | 33 ms | 0.43× |
| Q3: Group by status | 35,782 ms | 183 ms | 68 ms | 64 ms | 0.35× |
| Q4: Region × status breakdown | 13,290 ms | 198 ms | 241 ms | 268 ms | 1.35× |
| Q5: Monthly revenue (2023) | 5,562 ms | 159 ms | 42 ms | 42 ms | 0.26× |
| Q6: Top 10 spenders | 810,282 ms | 524 ms | 257 ms | 250 ms | 0.48× |
| Q7: Regional analytics | 57,104 ms | 522 ms | 151 ms | 167 ms | 0.32× |
| Q8: Join users + orders | 834,387 ms | 615 ms | 225 ms | 200 ms | 0.33× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 434 ms | 2.6× | 55 ms | 58 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 825 ms | 17.1× | 125 ms | 89 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 142 ms | 64.8× | 64 ms | 51 ms | 0.36× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 486 ms | 119.3× | 150 ms | 183 ms | 0.38× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 26 MB | 2% | 431 MB | n/a | n/a |
| Q2: Filtered count | 0% | 35 MB | 3% | 461 MB | n/a | n/a |
| Q3: Group by status | 0% | 74 MB | 203% | 489 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 99 MB | 597% | 563 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 271 MB | 89% | 481 MB | n/a | n/a |
| Q6: Top 10 spenders | 66% | 529 MB | 704% | 648 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 554 MB | 690% | 572 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 702 MB | 633% | 764 MB | n/a | n/a |

