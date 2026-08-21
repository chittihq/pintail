# Pintail analytical benchmark results

Measured 2026-08-21T04:37:18.142Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 15 ms | 94.9× | 11 ms | 14 ms | 0.93× | yes |
| Q2: Filtered count | 593 ms | 14 ms | 42.4× | 34 ms | 35 ms | 2.50× | yes |
| Q3: Group by status | 36,073 ms | 14 ms | 2576.6× | 69 ms | 70 ms | 5.00× | yes |
| Q4: Region × status breakdown | 13,263 ms | 14 ms | 947.4× | 250 ms | 247 ms | 17.64× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 18 ms | 315.3× | 49 ms | 49 ms | 2.72× | yes |
| Q6: Top 10 spenders | 806,748 ms | 92 ms | 8769.0× | 263 ms | 248 ms | 2.70× | yes |
| Q7: Regional analytics | 57,445 ms | 17 ms | 3379.1× | 163 ms | 185 ms | 10.88× | yes |
| Q8: Join users + orders | 1,430,931 ms | 16 ms | 89433.2× | 202 ms | 203 ms | 12.69× | yes |
| **Total** | **2,352,153 ms** | **200 ms** | **11760.8×** | **1,041 ms** | **1,051 ms** | **5.25×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 15.3 | 434 ms | 0 | 21.8 | 182 ms | 0 |
| 4 | 67.6 | 203 ms | 0 | 50.9 | 417 ms | 0 |
| 8 | 128.6 | 201 ms | 0 | 145.1 | 201 ms | 0 |
| 16 | 280.3 | 179 ms | 0 | 297.8 | 171 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 11 ms | 10 ms | 13 ms | 1.18× |
| Q2: Filtered count | 593 ms | 77 ms | 31 ms | 32 ms | 0.42× |
| Q3: Group by status | 36,073 ms | 195 ms | 73 ms | 72 ms | 0.37× |
| Q4: Region × status breakdown | 13,263 ms | 196 ms | 265 ms | 235 ms | 1.20× |
| Q5: Monthly revenue (2023) | 5,676 ms | 163 ms | 45 ms | 43 ms | 0.26× |
| Q6: Top 10 spenders | 806,748 ms | 502 ms | 259 ms | 248 ms | 0.49× |
| Q7: Regional analytics | 57,445 ms | 523 ms | 149 ms | 174 ms | 0.33× |
| Q8: Join users + orders | 1,430,931 ms | 525 ms | 203 ms | 210 ms | 0.40× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 449 ms | 2.4× | 58 ms | 191 ms | 0.43× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 827 ms | 17.0× | 91 ms | 121 ms | 0.15× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 140 ms | 65.6× | 42 ms | 54 ms | 0.39× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 561 ms | 103.7× | 176 ms | 177 ms | 0.32× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 82 MB | 2% | 487 MB | n/a | n/a |
| Q2: Filtered count | 1% | 93 MB | 61% | 518 MB | n/a | n/a |
| Q3: Group by status | 0% | 131 MB | 367% | 548 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 154 MB | 665% | 595 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 306 MB | 151% | 548 MB | n/a | n/a |
| Q6: Top 10 spenders | 56% | 604 MB | 720% | 735 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 634 MB | 570% | 633 MB | n/a | n/a |
| Q8: Join users + orders | 1% | 660 MB | 684% | 806 MB | n/a | n/a |

