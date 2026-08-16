# Pintail analytical benchmark results

Measured 2026-08-16T05:36:56.621Z with 20,000,000 orders.

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
| Q1: Full table count | 1,715 ms | 16 ms | 107.2× | 11 ms | 12 ms | 0.75× | yes |
| Q2: Filtered count | 692 ms | 14 ms | 49.4× | 30 ms | 32 ms | 2.29× | yes |
| Q3: Group by status | 34,411 ms | 13 ms | 2647.0× | 87 ms | 75 ms | 5.77× | yes |
| Q4: Region × status breakdown | 13,151 ms | 29 ms | 453.5× | 177 ms | 268 ms | 9.24× | yes |
| Q5: Monthly revenue (2023) | 5,686 ms | 36 ms | 157.9× | 43 ms | 45 ms | 1.25× | yes |
| Q6: Top 10 spenders | 879,223 ms | 90 ms | 9769.1× | 259 ms | 261 ms | 2.90× | yes |
| Q7: Regional analytics | 53,469 ms | 23 ms | 2324.7× | 205 ms | 204 ms | 8.87× | yes |
| Q8: Join users + orders | 879,719 ms | 28 ms | 31418.5× | 207 ms | 168 ms | 6.00× | yes |
| **Total** | **1,868,066 ms** | **249 ms** | **7502.3×** | **1,019 ms** | **1,065 ms** | **4.28×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 25.1 | 188 ms | 0 | 30.4 | 173 ms | 0 |
| 4 | 98.2 | 184 ms | 0 | 107.9 | 168 ms | 0 |
| 8 | 189.4 | 186 ms | 0 | 184.1 | 174 ms | 0 |
| 16 | 324.2 | 188 ms | 0 | 216.9 | 227 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,715 ms | 17 ms | 10 ms | 13 ms | 0.76× |
| Q2: Filtered count | 692 ms | 99 ms | 42 ms | 30 ms | 0.30× |
| Q3: Group by status | 34,411 ms | 284 ms | 65 ms | 118 ms | 0.42× |
| Q4: Region × status breakdown | 13,151 ms | 297 ms | 177 ms | 240 ms | 0.81× |
| Q5: Monthly revenue (2023) | 5,686 ms | 261 ms | 40 ms | 44 ms | 0.17× |
| Q6: Top 10 spenders | 879,223 ms | 556 ms | 178 ms | 192 ms | 0.35× |
| Q7: Regional analytics | 53,469 ms | 564 ms | 166 ms | 194 ms | 0.34× |
| Q8: Join users + orders | 879,719 ms | 733 ms | 176 ms | 168 ms | 0.23× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,088 ms | 526 ms | 2.1× | 69 ms | 50 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 12,970 ms | 1,029 ms | 12.6× | 84 ms | 211 ms | 0.21× | yes |
| N3: Monthly revenue, novel year | 8,099 ms | 301 ms | 26.9× | 51 ms | 168 ms | 0.56× | yes |
| N4: Regional analytics, novel range | 54,023 ms | 571 ms | 94.6× | 199 ms | 136 ms | 0.24× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 26 MB | 13% | 444 MB | n/a | n/a |
| Q2: Filtered count | 1% | 39 MB | 12% | 483 MB | n/a | n/a |
| Q3: Group by status | 1% | 91 MB | 487% | 497 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 118 MB | 669% | 555 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 9% | 330 MB | 196% | 506 MB | n/a | n/a |
| Q6: Top 10 spenders | 22% | 566 MB | 486% | 626 MB | n/a | n/a |
| Q7: Regional analytics | 6% | 570 MB | 396% | 568 MB | n/a | n/a |
| Q8: Join users + orders | 11% | 580 MB | 533% | 756 MB | n/a | n/a |

