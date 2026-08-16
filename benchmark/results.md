# Pintail analytical benchmark results

Measured 2026-08-16T16:00:48.670Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-15T23:30:45.751Z.
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
| Q1: Full table count | 1,715 ms | 21 ms | 81.7× | 79 ms | 15 ms | 0.71× | yes |
| Q2: Filtered count | 692 ms | 14 ms | 49.4× | 31 ms | 33 ms | 2.36× | yes |
| Q3: Group by status | 34,411 ms | 13 ms | 2647.0× | 89 ms | 72 ms | 5.54× | yes |
| Q4: Region × status breakdown | 13,151 ms | 15 ms | 876.7× | 272 ms | 244 ms | 16.27× | yes |
| Q5: Monthly revenue (2023) | 5,686 ms | 15 ms | 379.1× | 47 ms | 50 ms | 3.33× | yes |
| Q6: Top 10 spenders | 879,223 ms | 104 ms | 8454.1× | 188 ms | 184 ms | 1.77× | yes |
| Q7: Regional analytics | 53,469 ms | 14 ms | 3819.2× | 156 ms | 158 ms | 11.29× | yes |
| Q8: Join users + orders | 879,719 ms | 17 ms | 51748.2× | 206 ms | 234 ms | 13.76× | yes |
| **Total** | **1,868,066 ms** | **213 ms** | **8770.3×** | **1,068 ms** | **990 ms** | **4.65×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 19.7 | 258 ms | 0 | 17.7 | 262 ms | 0 |
| 4 | 91 | 216 ms | 0 | 108 | 168 ms | 0 |
| 8 | 177.9 | 189 ms | 0 | 199.3 | 172 ms | 0 |
| 16 | 280.9 | 203 ms | 0 | 237.1 | 198 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,715 ms | 13 ms | 12 ms | 13 ms | 1.00× |
| Q2: Filtered count | 692 ms | 84 ms | 36 ms | 33 ms | 0.39× |
| Q3: Group by status | 34,411 ms | 270 ms | 93 ms | 85 ms | 0.31× |
| Q4: Region × status breakdown | 13,151 ms | 276 ms | 289 ms | 226 ms | 0.82× |
| Q5: Monthly revenue (2023) | 5,686 ms | 187 ms | 55 ms | 57 ms | 0.30× |
| Q6: Top 10 spenders | 879,223 ms | 563 ms | 196 ms | 196 ms | 0.35× |
| Q7: Regional analytics | 53,469 ms | 677 ms | 166 ms | 166 ms | 0.25× |
| Q8: Join users + orders | 879,719 ms | 647 ms | 229 ms | 201 ms | 0.31× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,088 ms | 589 ms | 1.8× | 68 ms | 57 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 12,970 ms | 1,048 ms | 12.4× | 100 ms | 100 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 8,099 ms | 147 ms | 55.1× | 44 ms | 53 ms | 0.36× | yes |
| N4: Regional analytics, novel range | 54,023 ms | 638 ms | 84.7× | 166 ms | 183 ms | 0.29× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 27 MB | 3% | 461 MB | n/a | n/a |
| Q2: Filtered count | 0% | 37 MB | 55% | 501 MB | n/a | n/a |
| Q3: Group by status | 2% | 75 MB | 407% | 524 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 99 MB | 562% | 572 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 2% | 269 MB | 2% | 518 MB | n/a | n/a |
| Q6: Top 10 spenders | 71% | 524 MB | 533% | 657 MB | n/a | n/a |
| Q7: Regional analytics | 5% | 570 MB | 589% | 603 MB | n/a | n/a |
| Q8: Join users + orders | 150% | 587 MB | 641% | 803 MB | n/a | n/a |

