# Pintail analytical benchmark results

Measured 2026-08-18T15:17:44.006Z with 20,000,000 orders.

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
| Q1: Full table count | 1,563 ms | 13 ms | 120.2× | 11 ms | 13 ms | 1.00× | yes |
| Q2: Filtered count | 587 ms | 13 ms | 45.2× | 33 ms | 32 ms | 2.46× | yes |
| Q3: Group by status | 35,782 ms | 16 ms | 2236.4× | 66 ms | 69 ms | 4.31× | yes |
| Q4: Region × status breakdown | 13,290 ms | 12 ms | 1107.5× | 236 ms | 269 ms | 22.42× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 38 ms | 146.4× | 44 ms | 46 ms | 1.21× | yes |
| Q6: Top 10 spenders | 810,282 ms | 96 ms | 8440.4× | 253 ms | 254 ms | 2.65× | yes |
| Q7: Regional analytics | 57,104 ms | 15 ms | 3806.9× | 152 ms | 171 ms | 11.40× | yes |
| Q8: Join users + orders | 834,387 ms | 15 ms | 55625.8× | 197 ms | 201 ms | 13.40× | yes |
| **Total** | **1,758,557 ms** | **218 ms** | **8066.8×** | **992 ms** | **1,055 ms** | **4.84×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 26.8 | 154 ms | 0 | 30 | 148 ms | 0 |
| 4 | 130.4 | 149 ms | 0 | 134 | 144 ms | 0 |
| 8 | 226 | 158 ms | 0 | 249.7 | 144 ms | 0 |
| 16 | 360.3 | 174 ms | 0 | 237.7 | 174 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 14 ms | 11 ms | 14 ms | 1.00× |
| Q2: Filtered count | 587 ms | 69 ms | 32 ms | 32 ms | 0.46× |
| Q3: Group by status | 35,782 ms | 192 ms | 68 ms | 72 ms | 0.38× |
| Q4: Region × status breakdown | 13,290 ms | 202 ms | 247 ms | 241 ms | 1.19× |
| Q5: Monthly revenue (2023) | 5,562 ms | 153 ms | 42 ms | 47 ms | 0.31× |
| Q6: Top 10 spenders | 810,282 ms | 521 ms | 254 ms | 269 ms | 0.52× |
| Q7: Regional analytics | 57,104 ms | 510 ms | 145 ms | 172 ms | 0.34× |
| Q8: Join users + orders | 834,387 ms | 546 ms | 195 ms | 207 ms | 0.38× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 413 ms | 2.7× | 57 ms | 51 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 833 ms | 16.9× | 91 ms | 94 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 137 ms | 67.2× | 50 ms | 53 ms | 0.39× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 471 ms | 123.1× | 161 ms | 178 ms | 0.38× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 2% | 439 MB | n/a | n/a |
| Q2: Filtered count | 0% | 33 MB | 2% | 472 MB | n/a | n/a |
| Q3: Group by status | 0% | 73 MB | 279% | 499 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 99 MB | 663% | 556 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 268 MB | 54% | 502 MB | n/a | n/a |
| Q6: Top 10 spenders | 69% | 571 MB | 671% | 636 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 592 MB | 559% | 575 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 603 MB | 635% | 742 MB | n/a | n/a |

