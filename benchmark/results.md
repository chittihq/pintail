# Pintail analytical benchmark results

Measured 2026-08-19T17:15:35.360Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-19T07:23:56.425Z.
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
| Q1: Full table count | 1,424 ms | 13 ms | 109.5× | 10 ms | 13 ms | 1.00× | yes |
| Q2: Filtered count | 593 ms | 13 ms | 45.6× | 31 ms | 30 ms | 2.31× | yes |
| Q3: Group by status | 36,073 ms | 14 ms | 2576.6× | 63 ms | 67 ms | 4.79× | yes |
| Q4: Region × status breakdown | 13,263 ms | 12 ms | 1105.3× | 237 ms | 265 ms | 22.08× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 13 ms | 436.6× | 42 ms | 44 ms | 3.38× | yes |
| Q6: Top 10 spenders | 806,748 ms | 89 ms | 9064.6× | 246 ms | 270 ms | 3.03× | yes |
| Q7: Regional analytics | 57,445 ms | 12 ms | 4787.1× | 141 ms | 162 ms | 13.50× | yes |
| Q8: Join users + orders | 1,430,931 ms | 13 ms | 110071.6× | 189 ms | 200 ms | 15.38× | yes |
| **Total** | **2,352,153 ms** | **179 ms** | **13140.5×** | **959 ms** | **1,051 ms** | **5.87×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 60.5 | 18 ms | 0 | 66.5 | 16 ms | 0 |
| 4 | 215.6 | 25 ms | 0 | 248.3 | 19 ms | 0 |
| 8 | 370.2 | 50 ms | 0 | 357.7 | 46 ms | 0 |
| 16 | 347.2 | 155 ms | 0 | 360.1 | 118 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 13 ms | 9 ms | 11 ms | 0.85× |
| Q2: Filtered count | 593 ms | 75 ms | 29 ms | 29 ms | 0.39× |
| Q3: Group by status | 36,073 ms | 179 ms | 62 ms | 63 ms | 0.35× |
| Q4: Region × status breakdown | 13,263 ms | 186 ms | 275 ms | 278 ms | 1.49× |
| Q5: Monthly revenue (2023) | 5,676 ms | 159 ms | 40 ms | 43 ms | 0.27× |
| Q6: Top 10 spenders | 806,748 ms | 523 ms | 248 ms | 283 ms | 0.54× |
| Q7: Regional analytics | 57,445 ms | 505 ms | 145 ms | 167 ms | 0.33× |
| Q8: Join users + orders | 1,430,931 ms | 533 ms | 190 ms | 194 ms | 0.36× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 411 ms | 2.6× | 59 ms | 51 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 819 ms | 17.2× | 88 ms | 81 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 134 ms | 68.5× | 41 ms | 49 ms | 0.37× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 523 ms | 111.2× | 156 ms | 173 ms | 0.33× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 81 MB | 4% | 455 MB | n/a | n/a |
| Q2: Filtered count | 0% | 92 MB | 3% | 480 MB | n/a | n/a |
| Q3: Group by status | 0% | 130 MB | 149% | 511 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 153 MB | 642% | 564 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 306 MB | 3% | 506 MB | n/a | n/a |
| Q6: Top 10 spenders | 79% | 603 MB | 717% | 661 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 640 MB | 655% | 590 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 620 MB | 723% | 771 MB | n/a | n/a |

