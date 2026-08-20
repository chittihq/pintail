# Pintail analytical benchmark results

Measured 2026-08-20T10:34:51.620Z with 20,000,000 orders.

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
| Q1: Full table count | 1,424 ms | 10 ms | 142.4× | 9 ms | 11 ms | 1.10× | yes |
| Q2: Filtered count | 593 ms | 9 ms | 65.9× | 29 ms | 28 ms | 3.11× | yes |
| Q3: Group by status | 36,073 ms | 10 ms | 3607.3× | 63 ms | 60 ms | 6.00× | yes |
| Q4: Region × status breakdown | 13,263 ms | 11 ms | 1205.7× | 238 ms | 274 ms | 24.91× | yes |
| Q5: Monthly revenue (2023) | 5,676 ms | 10 ms | 567.6× | 41 ms | 42 ms | 4.20× | yes |
| Q6: Top 10 spenders | 806,748 ms | 86 ms | 9380.8× | 250 ms | 252 ms | 2.93× | yes |
| Q7: Regional analytics | 57,445 ms | 10 ms | 5744.5× | 142 ms | 163 ms | 16.30× | yes |
| Q8: Join users + orders | 1,430,931 ms | 11 ms | 130084.6× | 188 ms | 191 ms | 17.36× | yes |
| **Total** | **2,352,153 ms** | **157 ms** | **14981.9×** | **960 ms** | **1,021 ms** | **6.50×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 56.6 | 31 ms | 0 | 72.3 | 18 ms | 0 |
| 4 | 233.2 | 33 ms | 0 | 179.2 | 96 ms | 0 |
| 8 | 268.5 | 102 ms | 0 | 200.7 | 113 ms | 0 |
| 16 | 374.7 | 118 ms | 0 | 219.4 | 333 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,424 ms | 10 ms | 9 ms | 10 ms | 1.00× |
| Q2: Filtered count | 593 ms | 71 ms | 28 ms | 27 ms | 0.38× |
| Q3: Group by status | 36,073 ms | 171 ms | 63 ms | 63 ms | 0.37× |
| Q4: Region × status breakdown | 13,263 ms | 186 ms | 253 ms | 247 ms | 1.33× |
| Q5: Monthly revenue (2023) | 5,676 ms | 156 ms | 40 ms | 45 ms | 0.29× |
| Q6: Top 10 spenders | 806,748 ms | 524 ms | 250 ms | 245 ms | 0.47× |
| Q7: Regional analytics | 57,445 ms | 522 ms | 143 ms | 167 ms | 0.32× |
| Q8: Join users + orders | 1,430,931 ms | 510 ms | 196 ms | 192 ms | 0.38× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,077 ms | 401 ms | 2.7× | 54 ms | 53 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 14,048 ms | 759 ms | 18.5× | 87 ms | 89 ms | 0.12× | yes |
| N3: Monthly revenue, novel year | 9,180 ms | 137 ms | 67.0× | 39 ms | 45 ms | 0.33× | yes |
| N4: Regional analytics, novel range | 58,183 ms | 519 ms | 112.1× | 175 ms | 180 ms | 0.35× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 83 MB | 3% | 426 MB | n/a | n/a |
| Q2: Filtered count | 0% | 92 MB | 3% | 461 MB | n/a | n/a |
| Q3: Group by status | 0% | 131 MB | 165% | 495 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 155 MB | 637% | 543 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 309 MB | 2% | 487 MB | n/a | n/a |
| Q6: Top 10 spenders | 82% | 571 MB | 719% | 631 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 628 MB | 648% | 561 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 719 MB | 743% | 770 MB | n/a | n/a |

