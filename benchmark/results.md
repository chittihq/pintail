# Pintail analytical benchmark results

Measured 2026-08-15T18:49:40.762Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-13T22:45:56.812Z.
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
| Q1: Full table count | 1,427 ms | 15 ms | 95.1× | 11 ms | 26 ms | 1.73× | yes |
| Q2: Filtered count | 638 ms | 15 ms | 42.5× | 38 ms | 34 ms | 2.27× | yes |
| Q3: Group by status | 35,102 ms | 14 ms | 2507.3× | 80 ms | 67 ms | 4.79× | yes |
| Q4: Region × status breakdown | 13,236 ms | 15 ms | 882.4× | 242 ms | 244 ms | 16.27× | yes |
| Q5: Monthly revenue (2023) | 5,590 ms | 16 ms | 349.4× | 48 ms | 52 ms | 3.25× | yes |
| Q6: Top 10 spenders | 792,985 ms | 91 ms | 8714.1× | 276 ms | 275 ms | 3.02× | yes |
| Q7: Regional analytics | 55,420 ms | 14 ms | 3958.6× | 172 ms | 180 ms | 12.86× | yes |
| Q8: Join users + orders | 980,888 ms | 14 ms | 70063.4× | 303 ms | 226 ms | 16.14× | yes |
| **Total** | **1,885,286 ms** | **194 ms** | **9718.0×** | **1,170 ms** | **1,104 ms** | **5.69×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29.7 | 165 ms | 0 | 28.3 | 171 ms | 0 |
| 4 | 85.8 | 211 ms | 0 | 91 | 179 ms | 0 |
| 8 | 231.1 | 168 ms | 0 | 205.8 | 171 ms | 0 |
| 16 | 324.9 | 178 ms | 0 | 282 | 192 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,427 ms | 14 ms | 11 ms | 16 ms | 1.14× |
| Q2: Filtered count | 638 ms | 81 ms | 35 ms | 38 ms | 0.47× |
| Q3: Group by status | 35,102 ms | 250 ms | 71 ms | 67 ms | 0.27× |
| Q4: Region × status breakdown | 13,236 ms | 314 ms | 247 ms | 245 ms | 0.78× |
| Q5: Monthly revenue (2023) | 5,590 ms | 269 ms | 49 ms | 46 ms | 0.17× |
| Q6: Top 10 spenders | 792,985 ms | 799 ms | 274 ms | 253 ms | 0.32× |
| Q7: Regional analytics | 55,420 ms | 704 ms | 148 ms | 185 ms | 0.26× |
| Q8: Join users + orders | 980,888 ms | 712 ms | 220 ms | 208 ms | 0.29× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,105 ms | 524 ms | 2.1× | 63 ms | 57 ms | 0.11× | yes |
| N2: Group by region (novel group column) | 13,720 ms | 1,042 ms | 13.2× | 101 ms | 94 ms | 0.09× | yes |
| N3: Monthly revenue, novel year | 8,800 ms | 276 ms | 31.9× | 127 ms | 49 ms | 0.18× | yes |
| N4: Regional analytics, novel range | 55,912 ms | 799 ms | 70.0× | 193 ms | 194 ms | 0.24× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 26 MB | 3% | 461 MB | n/a | n/a |
| Q2: Filtered count | 0% | 38 MB | 17% | 484 MB | n/a | n/a |
| Q3: Group by status | 0% | 119 MB | 486% | 505 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 166 MB | 670% | 561 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 476 MB | 117% | 519 MB | n/a | n/a |
| Q6: Top 10 spenders | 76% | 580 MB | 687% | 672 MB | n/a | n/a |
| Q7: Regional analytics | 3% | 595 MB | 475% | 586 MB | n/a | n/a |
| Q8: Join users + orders | 1% | 669 MB | 553% | 752 MB | n/a | n/a |

