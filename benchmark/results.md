# Pintail analytical benchmark results

Measured 2026-08-20T02:59:07.784Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 12 ms | 118.7× | 10 ms | 12 ms | 1.00× | yes |
| Q2: Filtered count | 593 ms | 13 ms | 45.6× | 30 ms | 31 ms | 2.38× | yes |
| Q3: Group by status | 36,073 ms | 12 ms | 3006.1× | 65 ms | 70 ms | 5.83× | yes |
| Q4: Region × status breakdown | 13,263 ms | 12 ms | 1105.3× | 269 ms | 257 ms | 21.42× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 12 ms | 473.0× | 48 ms | 46 ms | 3.83× | yes |
| Q6: Top 10 spenders | 806,748 ms | 95 ms | 8492.1× | 250 ms | 248 ms | 2.61× | yes |
| Q7: Regional analytics | 57,445 ms | 12 ms | 4787.1× | 149 ms | 174 ms | 14.50× | yes |
| Q8: Join users + orders | 1,430,931 ms | 12 ms | 119244.3× | 196 ms | 200 ms | 16.67× | yes |
| **Total** | **2,352,153 ms** | **180 ms** | **13067.5×** | **1,017 ms** | **1,038 ms** | **5.77×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 26.1 | 154 ms | 0 | 31.4 | 146 ms | 0 |
| 4 | 121.8 | 150 ms | 0 | 132.5 | 146 ms | 0 |
| 8 | 237.7 | 157 ms | 0 | 277.9 | 144 ms | 0 |
| 16 | 397.3 | 165 ms | 0 | 357.5 | 130 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 14 ms | 10 ms | 12 ms | 0.86× |
| Q2: Filtered count | 593 ms | 75 ms | 29 ms | 39 ms | 0.52× |
| Q3: Group by status | 36,073 ms | 186 ms | 69 ms | 68 ms | 0.37× |
| Q4: Region × status breakdown | 13,263 ms | 191 ms | 235 ms | 266 ms | 1.39× |
| Q5: Monthly revenue (2023) | 5,676 ms | 164 ms | 43 ms | 55 ms | 0.34× |
| Q6: Top 10 spenders | 806,748 ms | 520 ms | 246 ms | 238 ms | 0.46× |
| Q7: Regional analytics | 57,445 ms | 504 ms | 176 ms | 168 ms | 0.33× |
| Q8: Join users + orders | 1,430,931 ms | 530 ms | 194 ms | 198 ms | 0.37× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 412 ms | 2.6× | 56 ms | 52 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 773 ms | 18.2× | 84 ms | 95 ms | 0.12× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 136 ms | 67.5× | 38 ms | 49 ms | 0.36× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 481 ms | 121.0× | 148 ms | 187 ms | 0.39× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 80 MB | 3% | 456 MB | n/a | n/a |
| Q2: Filtered count | 0% | 89 MB | 75% | 490 MB | n/a | n/a |
| Q3: Group by status | 0% | 130 MB | 304% | 507 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 154 MB | 651% | 555 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 1% | 306 MB | 113% | 513 MB | n/a | n/a |
| Q6: Top 10 spenders | 62% | 603 MB | 735% | 635 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 628 MB | 572% | 594 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 719 MB | 674% | 756 MB | n/a | n/a |

