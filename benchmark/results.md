# Pintail analytical benchmark results

Measured 2026-08-15T06:23:04.323Z with 20,000,000 orders.

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
| Q1: Full table count | 1,427 ms | 13 ms | 109.8× | 11 ms | 13 ms | 1.00× | yes |
| Q2: Filtered count | 638 ms | 13 ms | 49.1× | 33 ms | 32 ms | 2.46× | yes |
| Q3: Group by status | 35,102 ms | 12 ms | 2925.2× | 66 ms | 65 ms | 5.42× | yes |
| Q4: Region × status breakdown | 13,236 ms | 13 ms | 1018.2× | 285 ms | 249 ms | 19.15× | yes |
| Q5: Monthly revenue (2023) | 5,590 ms | 12 ms | 465.8× | 42 ms | 45 ms | 3.75× | yes |
| Q6: Top 10 spenders | 792,985 ms | 72 ms | 11013.7× | 266 ms | 257 ms | 3.57× | yes |
| Q7: Regional analytics | 55,420 ms | 13 ms | 4263.1× | 141 ms | 170 ms | 13.08× | yes |
| Q8: Join users + orders | 980,888 ms | 14 ms | 70063.4× | 204 ms | 206 ms | 14.71× | yes |
| **Total** | **1,885,286 ms** | **162 ms** | **11637.6×** | **1,048 ms** | **1,037 ms** | **6.40×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 27.5 | 153 ms | 0 | 23.9 | 152 ms | 0 |
| 4 | 121.5 | 151 ms | 0 | 145.5 | 117 ms | 0 |
| 8 | 232.2 | 146 ms | 0 | 279.2 | 109 ms | 0 |
| 16 | 394.6 | 164 ms | 0 | 361.3 | 114 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,427 ms | 21 ms | 9 ms | 20 ms | 0.95× |
| Q2: Filtered count | 638 ms | 96 ms | 30 ms | 29 ms | 0.30× |
| Q3: Group by status | 35,102 ms | 305 ms | 68 ms | 65 ms | 0.21× |
| Q4: Region × status breakdown | 13,236 ms | 425 ms | 257 ms | 244 ms | 0.57× |
| Q5: Monthly revenue (2023) | 5,590 ms | 509 ms | 38 ms | 47 ms | 0.09× |
| Q6: Top 10 spenders | 792,985 ms | 946 ms | 250 ms | 256 ms | 0.27× |
| Q7: Regional analytics | 55,420 ms | 1,058 ms | 154 ms | 170 ms | 0.16× |
| Q8: Join users + orders | 980,888 ms | 896 ms | 212 ms | 200 ms | 0.22× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,105 ms | 623 ms | 1.8× | 57 ms | 55 ms | 0.09× | yes |
| N2: Group by region (novel group column) | 13,720 ms | 1,202 ms | 11.4× | 85 ms | 93 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 8,800 ms | 495 ms | 17.8× | 44 ms | 46 ms | 0.09× | yes |
| N4: Regional analytics, novel range | 55,912 ms | 1,122 ms | 49.8× | 149 ms | 178 ms | 0.16× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 5% | 446 MB | n/a | n/a |
| Q2: Filtered count | 0% | 39 MB | 2% | 488 MB | n/a | n/a |
| Q3: Group by status | 0% | 126 MB | 336% | 495 MB | n/a | n/a |
| Q4: Region × status breakdown | 1% | 186 MB | 677% | 592 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 486 MB | 199% | 509 MB | n/a | n/a |
| Q6: Top 10 spenders | 81% | 701 MB | 677% | 643 MB | n/a | n/a |
| Q7: Regional analytics | 76% | 862 MB | 682% | 570 MB | n/a | n/a |
| Q8: Join users + orders | 43% | 972 MB | 636% | 777 MB | n/a | n/a |

