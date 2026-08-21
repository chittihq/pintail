# Pintail analytical benchmark results

Measured 2026-08-21T19:16:28.391Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 13 ms | 109.5× | 12 ms | 14 ms | 1.08× | yes |
| Q2: Filtered count | 593 ms | 16 ms | 37.1× | 33 ms | 55 ms | 3.44× | yes |
| Q3: Group by status | 36,073 ms | 15 ms | 2404.9× | 84 ms | 81 ms | 5.40× | yes |
| Q4: Region × status breakdown | 13,263 ms | 19 ms | 698.1× | 237 ms | 256 ms | 13.47× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 14 ms | 405.4× | 51 ms | 50 ms | 3.57× | yes |
| Q6: Top 10 spenders | 806,748 ms | 92 ms | 8769.0× | 262 ms | 262 ms | 2.85× | yes |
| Q7: Regional analytics | 57,445 ms | 14 ms | 4103.2× | 156 ms | 170 ms | 12.14× | yes |
| Q8: Join users + orders | 1,430,931 ms | 15 ms | 95395.4× | 204 ms | 203 ms | 13.53× | yes |
| **Total** | **2,352,153 ms** | **198 ms** | **11879.6×** | **1,039 ms** | **1,091 ms** | **5.51×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 48.8 | 45 ms | 0 | 49.4 | 46 ms | 0 |
| 4 | 151.7 | 92 ms | 0 | 123.7 | 116 ms | 0 |
| 8 | 168.9 | 135 ms | 0 | 223.9 | 111 ms | 0 |
| 16 | 297.8 | 142 ms | 0 | 272.9 | 169 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 13 ms | 10 ms | 12 ms | 0.92× |
| Q2: Filtered count | 593 ms | 88 ms | 31 ms | 29 ms | 0.33× |
| Q3: Group by status | 36,073 ms | 197 ms | 73 ms | 71 ms | 0.36× |
| Q4: Region × status breakdown | 13,263 ms | 203 ms | 251 ms | 269 ms | 1.33× |
| Q5: Monthly revenue (2023) | 5,676 ms | 167 ms | 51 ms | 48 ms | 0.29× |
| Q6: Top 10 spenders | 806,748 ms | 504 ms | 252 ms | 248 ms | 0.49× |
| Q7: Regional analytics | 57,445 ms | 517 ms | 152 ms | 168 ms | 0.32× |
| Q8: Join users + orders | 1,430,931 ms | 534 ms | 200 ms | 202 ms | 0.38× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 387 ms | 2.8× | 59 ms | 55 ms | 0.14× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 850 ms | 16.5× | 94 ms | 94 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 138 ms | 66.5× | 171 ms | 54 ms | 0.39× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 477 ms | 122.0× | 206 ms | 182 ms | 0.38× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 2% | 477 MB | n/a | n/a |
| Q2: Filtered count | 1% | 37 MB | 3% | 514 MB | n/a | n/a |
| Q3: Group by status | 0% | 74 MB | 324% | 552 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 98 MB | 713% | 621 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 270 MB | 27% | 537 MB | n/a | n/a |
| Q6: Top 10 spenders | 79% | 560 MB | 694% | 682 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 595 MB | 596% | 636 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 575 MB | 629% | 791 MB | n/a | n/a |

