# Pintail analytical benchmark results

Measured 2026-08-20T14:16:52.522Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 9 ms | 158.2× | 7 ms | 9 ms | 1.00× | yes |
| Q2: Filtered count | 593 ms | 9 ms | 65.9× | 26 ms | 27 ms | 3.00× | yes |
| Q3: Group by status | 36,073 ms | 9 ms | 4008.1× | 62 ms | 62 ms | 6.89× | yes |
| Q4: Region × status breakdown | 13,263 ms | 9 ms | 1473.7× | 224 ms | 227 ms | 25.22× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 9 ms | 630.7× | 38 ms | 43 ms | 4.78× | yes |
| Q6: Top 10 spenders | 806,748 ms | 85 ms | 9491.2× | 235 ms | 235 ms | 2.76× | yes |
| Q7: Regional analytics | 57,445 ms | 10 ms | 5744.5× | 138 ms | 161 ms | 16.10× | yes |
| Q8: Join users + orders | 1,430,931 ms | 9 ms | 158992.3× | 187 ms | 189 ms | 21.00× | yes |
| **Total** | **2,352,153 ms** | **149 ms** | **15786.3×** | **917 ms** | **953 ms** | **6.40×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 106 | 11 ms | 0 | 99.8 | 12 ms | 0 |
| 4 | 358 | 17 ms | 0 | 368 | 14 ms | 0 |
| 8 | 525.2 | 29 ms | 0 | 383.4 | 51 ms | 0 |
| 16 | 609.8 | 52 ms | 0 | 358.2 | 79 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 10 ms | 8 ms | 10 ms | 1.00× |
| Q2: Filtered count | 593 ms | 70 ms | 27 ms | 27 ms | 0.39× |
| Q3: Group by status | 36,073 ms | 164 ms | 62 ms | 63 ms | 0.38× |
| Q4: Region × status breakdown | 13,263 ms | 182 ms | 226 ms | 226 ms | 1.24× |
| Q5: Monthly revenue (2023) | 5,676 ms | 150 ms | 38 ms | 40 ms | 0.27× |
| Q6: Top 10 spenders | 806,748 ms | 494 ms | 239 ms | 237 ms | 0.48× |
| Q7: Regional analytics | 57,445 ms | 497 ms | 139 ms | 160 ms | 0.32× |
| Q8: Join users + orders | 1,430,931 ms | 528 ms | 195 ms | 194 ms | 0.37× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 402 ms | 2.7× | 52 ms | 50 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 848 ms | 16.6× | 85 ms | 84 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 130 ms | 70.6× | 40 ms | 40 ms | 0.31× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 466 ms | 124.9× | 153 ms | 170 ms | 0.36× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 2% | 479 MB | n/a | n/a |
| Q2: Filtered count | 1% | 35 MB | 2% | 458 MB | n/a | n/a |
| Q3: Group by status | 0% | 72 MB | 54% | 476 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 97 MB | 759% | 530 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 270 MB | 3% | 476 MB | n/a | n/a |
| Q6: Top 10 spenders | 93% | 568 MB | 761% | 622 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 613 MB | 717% | 569 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 624 MB | 760% | 781 MB | n/a | n/a |

