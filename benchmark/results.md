# Pintail analytical benchmark results

Measured 2026-08-13T22:59:28.975Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-13T22:45:56.812Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty.

NOT like for like: the canonical table is served from pintail's settled
aggregate memo, while ClickHouse's query cache is off and it executes every
run. It measures what a repeated dashboard query costs, not engine speed.
The novel-query table below is the engine-speed comparison - both engines
execute there, and ClickHouse is currently faster.

## Repeated queries (memo-served — dashboard refresh cost, not engine speed)

| Query | MySQL | Pintail (memo) | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,427 ms | 14 ms | 101.9× | 13 ms | 15 ms | 1.07× | yes |
| Q2: Filtered count | 638 ms | 14 ms | 45.6× | 33 ms | 32 ms | 2.29× | yes |
| Q3: Group by status | 35,102 ms | 13 ms | 2700.2× | 71 ms | 69 ms | 5.31× | yes |
| Q4: Region × status breakdown | 13,236 ms | 13 ms | 1018.2× | 239 ms | 255 ms | 19.62× | yes |
| Q5: Monthly revenue (2023) | 5,590 ms | 15 ms | 372.7× | 44 ms | 47 ms | 3.13× | yes |
| Q6: Top 10 spenders | 792,985 ms | 75 ms | 10573.1× | 262 ms | 261 ms | 3.48× | yes |
| Q7: Regional analytics | 55,420 ms | 13 ms | 4263.1× | 156 ms | 170 ms | 13.08× | yes |
| Q8: Join users + orders | 980,888 ms | 13 ms | 75452.9× | 208 ms | 206 ms | 15.85× | yes |
| **Total** | **1,885,286 ms** | **170 ms** | **11089.9×** | **1,026 ms** | **1,055 ms** | **6.21×** | |

Release gate: PASS (required ≥50× and exact results).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 23.5 | 158 ms | 0 | 26.4 | 152 ms | 0 |
| 4 | 108.4 | 157 ms | 0 | 108 | 155 ms | 0 |
| 8 | 209.1 | 163 ms | 0 | 206.8 | 163 ms | 0 |
| 16 | 367.6 | 172 ms | 0 | 332.1 | 173 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,427 ms | 15 ms | 12 ms | 15 ms | 1.00× |
| Q2: Filtered count | 638 ms | 93 ms | 38 ms | 33 ms | 0.35× |
| Q3: Group by status | 35,102 ms | 309 ms | 69 ms | 68 ms | 0.22× |
| Q4: Region × status breakdown | 13,236 ms | 411 ms | 246 ms | 241 ms | 0.59× |
| Q5: Monthly revenue (2023) | 5,590 ms | 517 ms | 43 ms | 45 ms | 0.09× |
| Q6: Top 10 spenders | 792,985 ms | 975 ms | 258 ms | 256 ms | 0.26× |
| Q7: Regional analytics | 55,420 ms | 1,060 ms | 180 ms | 170 ms | 0.16× |
| Q8: Join users + orders | 980,888 ms | 858 ms | 195 ms | 198 ms | 0.23× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,105 ms | 615 ms | 1.8× | 56 ms | 56 ms | 0.09× | yes |
| N2: Group by region (novel group column) | 13,720 ms | 1,229 ms | 11.2× | 94 ms | 92 ms | 0.07× | yes |
| N3: Monthly revenue, novel year | 8,800 ms | 487 ms | 18.1× | 42 ms | 50 ms | 0.10× | yes |
| N4: Regional analytics, novel range | 55,912 ms | 1,039 ms | 53.8× | 175 ms | 201 ms | 0.19× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 2% | 429 MB | n/a | n/a |
| Q2: Filtered count | 0% | 40 MB | 2% | 465 MB | n/a | n/a |
| Q3: Group by status | 1% | 130 MB | 286% | 489 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 186 MB | 726% | 543 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 488 MB | 230% | 492 MB | n/a | n/a |
| Q6: Top 10 spenders | 19% | 675 MB | 688% | 630 MB | n/a | n/a |
| Q7: Regional analytics | 27% | 765 MB | 556% | 591 MB | n/a | n/a |
| Q8: Join users + orders | 6% | 901 MB | 659% | 739 MB | n/a | n/a |

