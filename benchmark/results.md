# Pintail analytical benchmark results

Measured 2026-08-18T17:58:29.922Z with 20,000,000 orders.

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
| Q1: Full table count | 1,563 ms | 13 ms | 120.2× | 10 ms | 12 ms | 0.92× | yes |
| Q2: Filtered count | 587 ms | 32 ms | 18.3× | 32 ms | 30 ms | 0.94× | yes |
| Q3: Group by status | 35,782 ms | 13 ms | 2752.5× | 66 ms | 64 ms | 4.92× | yes |
| Q4: Region × status breakdown | 13,290 ms | 12 ms | 1107.5× | 242 ms | 275 ms | 22.92× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 13 ms | 427.8× | 42 ms | 44 ms | 3.38× | yes |
| Q6: Top 10 spenders | 810,282 ms | 91 ms | 8904.2× | 243 ms | 286 ms | 3.14× | yes |
| Q7: Regional analytics | 57,104 ms | 12 ms | 4758.7× | 158 ms | 169 ms | 14.08× | yes |
| Q8: Join users + orders | 834,387 ms | 13 ms | 64183.6× | 202 ms | 207 ms | 15.92× | yes |
| **Total** | **1,758,557 ms** | **199 ms** | **8837.0×** | **995 ms** | **1,087 ms** | **5.46×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29.5 | 150 ms | 0 | 27.2 | 149 ms | 0 |
| 4 | 127.3 | 148 ms | 0 | 149.8 | 141 ms | 0 |
| 8 | 258.4 | 149 ms | 0 | 294.7 | 107 ms | 0 |
| 16 | 392.2 | 165 ms | 0 | 349.4 | 124 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 15 ms | 11 ms | 13 ms | 0.87× |
| Q2: Filtered count | 587 ms | 79 ms | 33 ms | 29 ms | 0.37× |
| Q3: Group by status | 35,782 ms | 184 ms | 191 ms | 64 ms | 0.35× |
| Q4: Region × status breakdown | 13,290 ms | 184 ms | 251 ms | 239 ms | 1.30× |
| Q5: Monthly revenue (2023) | 5,562 ms | 174 ms | 41 ms | 48 ms | 0.28× |
| Q6: Top 10 spenders | 810,282 ms | 522 ms | 270 ms | 271 ms | 0.52× |
| Q7: Regional analytics | 57,104 ms | 508 ms | 177 ms | 168 ms | 0.33× |
| Q8: Join users + orders | 834,387 ms | 502 ms | 196 ms | 192 ms | 0.38× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 410 ms | 2.8× | 56 ms | 59 ms | 0.14× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 869 ms | 16.2× | 84 ms | 171 ms | 0.20× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 133 ms | 69.2× | 44 ms | 44 ms | 0.33× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 526 ms | 110.2× | 178 ms | 180 ms | 0.34× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 28 MB | 2% | 459 MB | n/a | n/a |
| Q2: Filtered count | 0% | 37 MB | 2% | 476 MB | n/a | n/a |
| Q3: Group by status | 0% | 76 MB | 3% | 499 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 98 MB | 598% | 546 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 269 MB | 161% | 508 MB | n/a | n/a |
| Q6: Top 10 spenders | 60% | 573 MB | 777% | 651 MB | n/a | n/a |
| Q7: Regional analytics | 1% | 594 MB | 493% | 582 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 577 MB | 715% | 820 MB | n/a | n/a |

