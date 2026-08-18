# Pintail analytical benchmark results

Measured 2026-08-18T14:40:47.711Z with 20,000,000 orders.

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
| Q1: Full table count | 1,563 ms | 13 ms | 120.2× | 10 ms | 14 ms | 1.08× | yes |
| Q2: Filtered count | 587 ms | 15 ms | 39.1× | 31 ms | 36 ms | 2.40× | yes |
| Q3: Group by status | 35,782 ms | 12 ms | 2981.8× | 75 ms | 71 ms | 5.92× | yes |
| Q4: Region × status breakdown | 13,290 ms | 14 ms | 949.3× | 243 ms | 256 ms | 18.29× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 13 ms | 427.8× | 43 ms | 50 ms | 3.85× | yes |
| Q6: Top 10 spenders | 810,282 ms | 125 ms | 6482.3× | 266 ms | 252 ms | 2.02× | yes |
| Q7: Regional analytics | 57,104 ms | 13 ms | 4392.6× | 175 ms | 166 ms | 12.77× | yes |
| Q8: Join users + orders | 834,387 ms | 15 ms | 55625.8× | 205 ms | 214 ms | 14.27× | yes |
| **Total** | **1,758,557 ms** | **220 ms** | **7993.4×** | **1,048 ms** | **1,059 ms** | **4.81×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 24.4 | 161 ms | 0 | 20.7 | 161 ms | 0 |
| 4 | 127.7 | 152 ms | 0 | 115.9 | 147 ms | 0 |
| 8 | 235 | 117 ms | 0 | 207.7 | 119 ms | 0 |
| 16 | 343.7 | 115 ms | 0 | 270.6 | 155 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 14 ms | 12 ms | 13 ms | 0.93× |
| Q2: Filtered count | 587 ms | 76 ms | 30 ms | 31 ms | 0.41× |
| Q3: Group by status | 35,782 ms | 175 ms | 66 ms | 66 ms | 0.38× |
| Q4: Region × status breakdown | 13,290 ms | 196 ms | 269 ms | 284 ms | 1.45× |
| Q5: Monthly revenue (2023) | 5,562 ms | 158 ms | 42 ms | 47 ms | 0.30× |
| Q6: Top 10 spenders | 810,282 ms | 523 ms | 257 ms | 251 ms | 0.48× |
| Q7: Regional analytics | 57,104 ms | 512 ms | 157 ms | 167 ms | 0.33× |
| Q8: Join users + orders | 834,387 ms | 551 ms | 207 ms | 194 ms | 0.35× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 434 ms | 2.6× | 119 ms | 56 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 832 ms | 16.9× | 94 ms | 91 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 141 ms | 65.3× | 63 ms | 47 ms | 0.33× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 558 ms | 103.9× | 189 ms | 181 ms | 0.32× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 26 MB | 20% | 495 MB | n/a | n/a |
| Q2: Filtered count | 0% | 37 MB | 79% | 479 MB | n/a | n/a |
| Q3: Group by status | 0% | 75 MB | 369% | 503 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 99 MB | 718% | 550 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 271 MB | 201% | 489 MB | n/a | n/a |
| Q6: Top 10 spenders | 51% | 571 MB | 699% | 678 MB | n/a | n/a |
| Q7: Regional analytics | 1% | 598 MB | 468% | 577 MB | n/a | n/a |
| Q8: Join users + orders | 4% | 608 MB | 639% | 750 MB | n/a | n/a |

