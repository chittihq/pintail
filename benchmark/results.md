# Pintail analytical benchmark results

Measured 2026-08-19T07:28:12.006Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 11 ms | 129.5× | 10 ms | 12 ms | 1.09× | yes |
| Q2: Filtered count | 593 ms | 10 ms | 59.3× | 31 ms | 29 ms | 2.90× | yes |
| Q3: Group by status | 36,073 ms | 10 ms | 3607.3× | 64 ms | 63 ms | 6.30× | yes |
| Q4: Region × status breakdown | 13,263 ms | 11 ms | 1205.7× | 232 ms | 232 ms | 21.09× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 10 ms | 567.6× | 42 ms | 41 ms | 4.10× | yes |
| Q6: Top 10 spenders | 806,748 ms | 87 ms | 9273.0× | 244 ms | 239 ms | 2.75× | yes |
| Q7: Regional analytics | 57,445 ms | 11 ms | 5222.3× | 142 ms | 162 ms | 14.73× | yes |
| Q8: Join users + orders | 1,430,931 ms | 18 ms | 79496.2× | 207 ms | 194 ms | 10.78× | yes |
| **Total** | **2,352,153 ms** | **168 ms** | **14000.9×** | **972 ms** | **972 ms** | **5.79×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 14.4 | 130 ms | 0 | 13.6 | 142 ms | 0 |
| 4 | 54.5 | 141 ms | 0 | 55.8 | 151 ms | 0 |
| 8 | 111.3 | 143 ms | 0 | 91.7 | 205 ms | 0 |
| 16 | 182.4 | 138 ms | 0 | 96.4 | 349 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 50 ms | 95 ms | 92 ms | 1.84× |
| Q2: Filtered count | 593 ms | 105 ms | 159 ms | 117 ms | 1.11× |
| Q3: Group by status | 36,073 ms | 214 ms | 185 ms | 189 ms | 0.88× |
| Q4: Region × status breakdown | 13,263 ms | 235 ms | 375 ms | 394 ms | 1.68× |
| Q5: Monthly revenue (2023) | 5,676 ms | 195 ms | 82 ms | 87 ms | 0.45× |
| Q6: Top 10 spenders | 806,748 ms | 550 ms | 334 ms | 343 ms | 0.62× |
| Q7: Regional analytics | 57,445 ms | 573 ms | 189 ms | 217 ms | 0.38× |
| Q8: Join users + orders | 1,430,931 ms | 550 ms | 282 ms | 242 ms | 0.44× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 413 ms | 2.6× | 59 ms | 57 ms | 0.14× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 885 ms | 15.9× | 92 ms | 91 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 137 ms | 67.0× | 45 ms | 52 ms | 0.38× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 531 ms | 109.6× | 219 ms | 257 ms | 0.48× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 23 MB | 2% | 489 MB | 0% | 1,541 MB |
| Q2: Filtered count | 0% | 34 MB | 3% | 525 MB | 0% | 1,541 MB |
| Q3: Group by status | 0% | 72 MB | 88% | 558 MB | 82% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 96 MB | 752% | 624 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 268 MB | 2% | 570 MB | 109% | 1,553 MB |
| Q6: Top 10 spenders | 90% | 552 MB | 745% | 756 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 0% | 573 MB | 711% | 668 MB | 60% | 1,553 MB |
| Q8: Join users + orders | 0% | 553 MB | 656% | 884 MB | 65% | 1,822 MB |

