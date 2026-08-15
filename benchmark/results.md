# Pintail analytical benchmark results

Measured 2026-08-15T23:34:56.698Z with 20,000,000 orders.

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
| Q1: Full table count | 1,715 ms | 15 ms | 114.3× | 10 ms | 13 ms | 0.87× | yes |
| Q2: Filtered count | 692 ms | 30 ms | 23.1× | 32 ms | 40 ms | 1.33× | yes |
| Q3: Group by status | 34,411 ms | 20 ms | 1720.5× | 68 ms | 70 ms | 3.50× | yes |
| Q4: Region × status breakdown | 13,151 ms | 19 ms | 692.2× | 183 ms | 195 ms | 10.26× | yes |
| Q5: Monthly revenue (2023) | 5,686 ms | 13 ms | 437.4× | 41 ms | 49 ms | 3.77× | yes |
| Q6: Top 10 spenders | 879,223 ms | 96 ms | 9158.6× | 174 ms | 175 ms | 1.82× | yes |
| Q7: Regional analytics | 53,469 ms | 12 ms | 4455.8× | 185 ms | 157 ms | 13.08× | yes |
| Q8: Join users + orders | 879,719 ms | 21 ms | 41891.4× | 168 ms | 165 ms | 7.86× | yes |
| **Total** | **1,868,066 ms** | **226 ms** | **8265.8×** | **861 ms** | **864 ms** | **3.82×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 14 | 226 ms | 0 | 24.4 | 179 ms | 0 |
| 4 | 99.8 | 183 ms | 0 | 96.7 | 195 ms | 0 |
| 8 | 205.3 | 175 ms | 0 | 152.7 | 192 ms | 0 |
| 16 | 330.8 | 180 ms | 0 | 166.2 | 296 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,715 ms | 14 ms | 13 ms | 12 ms | 0.86× |
| Q2: Filtered count | 692 ms | 92 ms | 30 ms | 32 ms | 0.35× |
| Q3: Group by status | 34,411 ms | 275 ms | 67 ms | 69 ms | 0.25× |
| Q4: Region × status breakdown | 13,151 ms | 523 ms | 193 ms | 256 ms | 0.49× |
| Q5: Monthly revenue (2023) | 5,686 ms | 274 ms | 40 ms | 57 ms | 0.21× |
| Q6: Top 10 spenders | 879,223 ms | 530 ms | 181 ms | 178 ms | 0.34× |
| Q7: Regional analytics | 53,469 ms | 690 ms | 199 ms | 251 ms | 0.36× |
| Q8: Join users + orders | 879,719 ms | 741 ms | 177 ms | 175 ms | 0.24× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,088 ms | 523 ms | 2.1× | 68 ms | 56 ms | 0.11× | yes |
| N2: Group by region (novel group column) | 12,970 ms | 847 ms | 15.3× | 95 ms | 96 ms | 0.11× | yes |
| N3: Monthly revenue, novel year | 8,099 ms | 278 ms | 29.1× | 38 ms | 48 ms | 0.17× | yes |
| N4: Regional analytics, novel range | 54,023 ms | 733 ms | 73.7× | 150 ms | 152 ms | 0.21× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 9% | 973 MB | 92% | 1,541 MB |
| Q2: Filtered count | 8% | 36 MB | 7% | 1,009 MB | 1% | 1,541 MB |
| Q3: Group by status | 0% | 116 MB | 488% | 1,046 MB | 85% | 1,542 MB |
| Q4: Region × status breakdown | 0% | 139 MB | 666% | 1,139 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 1% | 474 MB | 221% | 1,069 MB | 110% | 1,553 MB |
| Q6: Top 10 spenders | 65% | 577 MB | 715% | 1,296 MB | 13% | 1,554 MB |
| Q7: Regional analytics | 1% | 587 MB | 554% | 1,142 MB | 64% | 1,554 MB |
| Q8: Join users + orders | 4% | 574 MB | 555% | 1,361 MB | 14% | 1,708 MB |

