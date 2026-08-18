# Pintail analytical benchmark results

Measured 2026-08-18T04:38:29.223Z with 20,000,000 orders.

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
| Q1: Full table count | 1,563 ms | 13 ms | 120.2× | 12 ms | 15 ms | 1.15× | yes |
| Q2: Filtered count | 587 ms | 13 ms | 45.2× | 33 ms | 31 ms | 2.38× | yes |
| Q3: Group by status | 35,782 ms | 13 ms | 2752.5× | 67 ms | 68 ms | 5.23× | yes |
| Q4: Region × status breakdown | 13,290 ms | 14 ms | 949.3× | 241 ms | 252 ms | 18.00× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 15 ms | 370.8× | 43 ms | 48 ms | 3.20× | yes |
| Q6: Top 10 spenders | 810,282 ms | 91 ms | 8904.2× | 253 ms | 254 ms | 2.79× | yes |
| Q7: Regional analytics | 57,104 ms | 13 ms | 4392.6× | 147 ms | 172 ms | 13.23× | yes |
| Q8: Join users + orders | 834,387 ms | 14 ms | 59599.1× | 227 ms | 202 ms | 14.43× | yes |
| **Total** | **1,758,557 ms** | **186 ms** | **9454.6×** | **1,023 ms** | **1,042 ms** | **5.60×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 28.4 | 152 ms | 0 | 23.9 | 165 ms | 0 |
| 4 | 121.1 | 144 ms | 0 | 112.7 | 153 ms | 0 |
| 8 | 224.1 | 159 ms | 0 | 207.8 | 121 ms | 0 |
| 16 | 360.5 | 170 ms | 0 | 298.2 | 136 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 15 ms | 14 ms | 14 ms | 0.93× |
| Q2: Filtered count | 587 ms | 80 ms | 40 ms | 40 ms | 0.50× |
| Q3: Group by status | 35,782 ms | 182 ms | 69 ms | 70 ms | 0.38× |
| Q4: Region × status breakdown | 13,290 ms | 196 ms | 251 ms | 251 ms | 1.28× |
| Q5: Monthly revenue (2023) | 5,562 ms | 170 ms | 44 ms | 49 ms | 0.29× |
| Q6: Top 10 spenders | 810,282 ms | 524 ms | 277 ms | 259 ms | 0.49× |
| Q7: Regional analytics | 57,104 ms | 521 ms | 151 ms | 202 ms | 0.39× |
| Q8: Join users + orders | 834,387 ms | 495 ms | 210 ms | 216 ms | 0.44× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 411 ms | 2.7× | 59 ms | 55 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 970 ms | 14.5× | 84 ms | 102 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 136 ms | 67.7× | 48 ms | 51 ms | 0.38× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 524 ms | 110.6× | 160 ms | 181 ms | 0.35× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 3% | 458 MB | n/a | n/a |
| Q2: Filtered count | 0% | 34 MB | 3% | 469 MB | n/a | n/a |
| Q3: Group by status | 0% | 73 MB | 377% | 503 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 96 MB | 751% | 556 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 269 MB | 136% | 488 MB | n/a | n/a |
| Q6: Top 10 spenders | 78% | 536 MB | 583% | 649 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 566 MB | 511% | 583 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 576 MB | 557% | 749 MB | n/a | n/a |

