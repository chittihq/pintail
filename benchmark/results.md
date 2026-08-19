# Pintail analytical benchmark results

Measured 2026-08-19T13:34:28.090Z with 20,000,000 orders.

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
| Q2: Filtered count | 593 ms | 8 ms | 74.1× | 28 ms | 27 ms | 3.38× | yes |
| Q3: Group by status | 36,073 ms | 8 ms | 4509.1× | 62 ms | 62 ms | 7.75× | yes |
| Q4: Region × status breakdown | 13,263 ms | 8 ms | 1657.9× | 225 ms | 228 ms | 28.50× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 10 ms | 567.6× | 38 ms | 41 ms | 4.10× | yes |
| Q6: Top 10 spenders | 806,748 ms | 91 ms | 8865.4× | 236 ms | 237 ms | 2.60× | yes |
| Q7: Regional analytics | 57,445 ms | 9 ms | 6382.8× | 138 ms | 159 ms | 17.67× | yes |
| Q8: Join users + orders | 1,430,931 ms | 10 ms | 143093.1× | 190 ms | 188 ms | 18.80× | yes |
| **Total** | **2,352,153 ms** | **153 ms** | **15373.5×** | **924 ms** | **951 ms** | **6.22×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 105.7 | 12 ms | 0 | 92.7 | 16 ms | 0 |
| 4 | 325.2 | 20 ms | 0 | 332.4 | 18 ms | 0 |
| 8 | 492.9 | 30 ms | 0 | 397.9 | 48 ms | 0 |
| 16 | 589.2 | 56 ms | 0 | 382.8 | 75 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 9 ms | 7 ms | 10 ms | 1.11× |
| Q2: Filtered count | 593 ms | 69 ms | 26 ms | 28 ms | 0.41× |
| Q3: Group by status | 36,073 ms | 168 ms | 61 ms | 61 ms | 0.36× |
| Q4: Region × status breakdown | 13,263 ms | 189 ms | 227 ms | 225 ms | 1.19× |
| Q5: Monthly revenue (2023) | 5,676 ms | 152 ms | 36 ms | 38 ms | 0.25× |
| Q6: Top 10 spenders | 806,748 ms | 487 ms | 237 ms | 234 ms | 0.48× |
| Q7: Regional analytics | 57,445 ms | 496 ms | 141 ms | 161 ms | 0.32× |
| Q8: Join users + orders | 1,430,931 ms | 522 ms | 189 ms | 192 ms | 0.37× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 398 ms | 2.7× | 55 ms | 49 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 903 ms | 15.6× | 95 ms | 88 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 136 ms | 67.5× | 37 ms | 48 ms | 0.35× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 470 ms | 123.8× | 178 ms | 168 ms | 0.36× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 23 MB | 3% | 508 MB | n/a | n/a |
| Q2: Filtered count | 0% | 35 MB | 3% | 535 MB | n/a | n/a |
| Q3: Group by status | 0% | 73 MB | 39% | 558 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 97 MB | 767% | 617 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 269 MB | 5% | 559 MB | n/a | n/a |
| Q6: Top 10 spenders | 94% | 524 MB | 757% | 704 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 590 MB | 722% | 639 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 600 MB | 761% | 834 MB | n/a | n/a |

