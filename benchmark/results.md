# Pintail analytical benchmark results

Measured 2026-09-03T08:25:58.423Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-09-03T08:22:24.512Z.
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
| Q1: Full table count | 1,437 ms | 12 ms | 119.8× | 13 ms | 13 ms | 1.08× | yes |
| Q2: Filtered count | 587 ms | 12 ms | 48.9× | 31 ms | 31 ms | 2.58× | yes |
| Q3: Group by status | 34,398 ms | 13 ms | 2646.0× | 71 ms | 69 ms | 5.31× | yes |
| Q4: Region × status breakdown | 13,054 ms | 13 ms | 1004.2× | 175 ms | 177 ms | 13.62× | yes |
| Q5: Monthly revenue (2023) | 5,462 ms | 11 ms | 496.5× | 40 ms | 43 ms | 3.91× | yes |
| Q6: Top 10 spenders | 889,417 ms | 77 ms | 11550.9× | 174 ms | 176 ms | 2.29× | yes |
| Q7: Regional analytics | 53,410 ms | 12 ms | 4450.8× | 122 ms | 130 ms | 10.83× | yes |
| Q8: Join users + orders | 796,769 ms | 15 ms | 53117.9× | 157 ms | 163 ms | 10.87× | yes |
| **Total** | **1,794,534 ms** | **165 ms** | **10876.0×** | **783 ms** | **802 ms** | **4.86×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29.5 | 149 ms | 0 | 28.4 | 151 ms | 0 |
| 4 | 124.8 | 150 ms | 0 | 117.9 | 151 ms | 0 |
| 8 | 229.8 | 155 ms | 0 | 221.5 | 162 ms | 0 |
| 16 | 379.4 | 169 ms | 0 | 344.1 | 177 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,437 ms | 13 ms | 12 ms | 14 ms | 1.08× |
| Q2: Filtered count | 587 ms | 92 ms | 33 ms | 32 ms | 0.35× |
| Q3: Group by status | 34,398 ms | 198 ms | 81 ms | 72 ms | 0.36× |
| Q4: Region × status breakdown | 13,054 ms | 229 ms | 196 ms | 201 ms | 0.88× |
| Q5: Monthly revenue (2023) | 5,462 ms | 140 ms | 44 ms | 48 ms | 0.34× |
| Q6: Top 10 spenders | 889,417 ms | 451 ms | 191 ms | 180 ms | 0.40× |
| Q7: Regional analytics | 53,410 ms | 523 ms | 134 ms | 137 ms | 0.26× |
| Q8: Join users + orders | 796,769 ms | 408 ms | 175 ms | 159 ms | 0.39× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,125 ms | 407 ms | 2.8× | 54 ms | 53 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 12,949 ms | 846 ms | 15.3× | 83 ms | 93 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 8,226 ms | 129 ms | 63.8× | 122 ms | 51 ms | 0.40× | yes |
| N4: Regional analytics, novel range | 56,145 ms | 544 ms | 103.2× | 160 ms | 139 ms | 0.26× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 23 MB | 2% | 472 MB | 100% | 1,541 MB |
| Q2: Filtered count | 0% | 31 MB | 3% | 505 MB | 0% | 1,541 MB |
| Q3: Group by status | 0% | 48 MB | 240% | 563 MB | 85% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 47 MB | 646% | 610 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 32 MB | 87% | 560 MB | 110% | 1,553 MB |
| Q6: Top 10 spenders | 63% | 96 MB | 654% | 715 MB | 17% | 1,553 MB |
| Q7: Regional analytics | 1% | 66 MB | 616% | 668 MB | 63% | 1,553 MB |
| Q8: Join users + orders | 3% | 54 MB | 536% | 848 MB | 16% | 1,708 MB |

