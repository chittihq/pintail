# Pintail analytical benchmark results

Measured 2026-08-17T18:42:20.538Z with 20,000,000 orders.

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
| Q1: Full table count | 1,563 ms | 14 ms | 111.6× | 10 ms | 14 ms | 1.00× | yes |
| Q2: Filtered count | 587 ms | 13 ms | 45.2× | 31 ms | 30 ms | 2.31× | yes |
| Q3: Group by status | 35,782 ms | 13 ms | 2752.5× | 66 ms | 65 ms | 5.00× | yes |
| Q4: Region × status breakdown | 13,290 ms | 13 ms | 1022.3× | 242 ms | 244 ms | 18.77× | yes |
| Q5: Monthly revenue (2023) | 5,562 ms | 22 ms | 252.8× | 48 ms | 51 ms | 2.32× | yes |
| Q6: Top 10 spenders | 810,282 ms | 97 ms | 8353.4× | 249 ms | 249 ms | 2.57× | yes |
| Q7: Regional analytics | 57,104 ms | 12 ms | 4758.7× | 142 ms | 168 ms | 14.00× | yes |
| Q8: Join users + orders | 834,387 ms | 16 ms | 52149.2× | 239 ms | 211 ms | 13.19× | yes |
| **Total** | **1,758,557 ms** | **200 ms** | **8792.8×** | **1,027 ms** | **1,032 ms** | **5.16×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 30.1 | 150 ms | 0 | 27.8 | 153 ms | 0 |
| 4 | 129.8 | 149 ms | 0 | 130.1 | 144 ms | 0 |
| 8 | 244.9 | 153 ms | 0 | 274.1 | 118 ms | 0 |
| 16 | 391.3 | 149 ms | 0 | 331.4 | 123 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,563 ms | 14 ms | 11 ms | 13 ms | 0.93× |
| Q2: Filtered count | 587 ms | 76 ms | 31 ms | 33 ms | 0.43× |
| Q3: Group by status | 35,782 ms | 181 ms | 67 ms | 65 ms | 0.36× |
| Q4: Region × status breakdown | 13,290 ms | 187 ms | 267 ms | 242 ms | 1.29× |
| Q5: Monthly revenue (2023) | 5,562 ms | 160 ms | 41 ms | 43 ms | 0.27× |
| Q6: Top 10 spenders | 810,282 ms | 524 ms | 273 ms | 251 ms | 0.48× |
| Q7: Regional analytics | 57,104 ms | 515 ms | 146 ms | 168 ms | 0.33× |
| Q8: Join users + orders | 834,387 ms | 539 ms | 200 ms | 198 ms | 0.37× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,130 ms | 486 ms | 2.3× | 64 ms | 60 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 14,081 ms | 807 ms | 17.4× | 97 ms | 173 ms | 0.21× | yes |
| N3: Monthly revenue, novel year | 9,202 ms | 136 ms | 67.7× | 39 ms | 45 ms | 0.33× | yes |
| N4: Regional analytics, novel range | 57,980 ms | 479 ms | 121.0× | 191 ms | 174 ms | 0.36× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 3% | 439 MB | n/a | n/a |
| Q2: Filtered count | 1% | 36 MB | 19% | 465 MB | n/a | n/a |
| Q3: Group by status | 0% | 73 MB | 356% | 511 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 96 MB | 646% | 548 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 267 MB | 136% | 495 MB | n/a | n/a |
| Q6: Top 10 spenders | 61% | 553 MB | 718% | 639 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 582 MB | 599% | 577 MB | n/a | n/a |
| Q8: Join users + orders | 5% | 607 MB | 639% | 769 MB | n/a | n/a |

