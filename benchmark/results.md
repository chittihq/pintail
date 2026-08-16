# Pintail analytical benchmark results

Measured 2026-08-16T18:04:52.030Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-16T18:01:03.714Z.
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
| Q1: Full table count | 1,563 ms | 19 ms | 82.3× | 17 ms | 19 ms | 1.00× | yes |
| Q2: Filtered count | 587 ms | 14 ms | 41.9× | 33 ms | 32 ms | 2.29× | yes |
| Q3: Group by status | 35,782 ms | 12 ms | 2981.8× | 66 ms | 65 ms | 5.42× | yes |
| Q4: Region × status breakdown | 13,290 ms | 20 ms | 664.5× | 240 ms | 244 ms | 12.20× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 14 ms | 397.3× | 44 ms | 47 ms | 3.36× | yes |
| Q6: Top 10 spenders | 810,282 ms | 98 ms | 8268.2× | 246 ms | 245 ms | 2.50× | yes |
| Q7: Regional analytics | 57,104 ms | 13 ms | 4392.6× | 156 ms | 173 ms | 13.31× | yes |
| Q8: Join users + orders | 834,387 ms | 14 ms | 59599.1× | 197 ms | 203 ms | 14.50× | yes |
| **Total** | **1,758,557 ms** | **204 ms** | **8620.4×** | **999 ms** | **1,028 ms** | **5.04×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 28.6 | 147 ms | 0 | 26.3 | 151 ms | 0 |
| 4 | 114.4 | 153 ms | 0 | 121 | 149 ms | 0 |
| 8 | 191.2 | 166 ms | 0 | 164.1 | 190 ms | 0 |
| 16 | 299.2 | 198 ms | 0 | 279 | 198 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 13 ms | 12 ms | 13 ms | 1.00× |
| Q2: Filtered count | 587 ms | 74 ms | 30 ms | 29 ms | 0.39× |
| Q3: Group by status | 35,782 ms | 189 ms | 65 ms | 65 ms | 0.34× |
| Q4: Region × status breakdown | 13,290 ms | 190 ms | 251 ms | 257 ms | 1.35× |
| Q5: Monthly revenue (2023) | 5,562 ms | 161 ms | 41 ms | 123 ms | 0.76× |
| Q6: Top 10 spenders | 810,282 ms | 505 ms | 272 ms | 244 ms | 0.48× |
| Q7: Regional analytics | 57,104 ms | 531 ms | 144 ms | 178 ms | 0.34× |
| Q8: Join users + orders | 834,387 ms | 537 ms | 205 ms | 202 ms | 0.38× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 408 ms | 2.8× | 66 ms | 180 ms | 0.44× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 887 ms | 15.9× | 87 ms | 90 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 136 ms | 67.7× | 41 ms | 95 ms | 0.70× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 516 ms | 112.4× | 168 ms | 178 ms | 0.34× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 25 MB | 7% | 443 MB | 63% | 1,541 MB |
| Q2: Filtered count | 0% | 36 MB | 19% | 477 MB | 0% | 1,541 MB |
| Q3: Group by status | 0% | 75 MB | 419% | 511 MB | 83% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 101 MB | 704% | 564 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 273 MB | 174% | 527 MB | 109% | 1,553 MB |
| Q6: Top 10 spenders | 44% | 578 MB | 627% | 745 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 2% | 594 MB | 450% | 674 MB | 61% | 1,553 MB |
| Q8: Join users + orders | 2% | 604 MB | 683% | 866 MB | 14% | 1,707 MB |

