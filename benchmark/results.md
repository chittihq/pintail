# Pintail analytical benchmark results

Measured 2026-08-18T20:17:03.550Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-18T20:12:55.853Z.
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
| Q1: Full table count | 1,515 ms | 43 ms | 35.2× | 25 ms | 33 ms | 0.77× | yes |
| Q2: Filtered count | 599 ms | 46 ms | 13.0× | 47 ms | 116 ms | 2.52× | yes |
| Q3: Group by status | 34,741 ms | 44 ms | 789.6× | 91 ms | 100 ms | 2.27× | yes |
| Q4: Region × status breakdown | 13,243 ms | 45 ms | 294.3× | 197 ms | 210 ms | 4.67× | yes |
| Q5: Monthly revenue (2023) | 6,021 ms | 37 ms | 162.7× | 65 ms | 77 ms | 2.08× | yes |
| Q6: Top 10 spenders | 874,288 ms | 136 ms | 6428.6× | 203 ms | 194 ms | 1.43× | yes |
| Q7: Regional analytics | 53,635 ms | 45 ms | 1191.9× | 134 ms | 175 ms | 3.89× | yes |
| Q8: Join users + orders | 880,361 ms | 28 ms | 31441.5× | 170 ms | 160 ms | 5.71× | yes |
| **Total** | **1,864,403 ms** | **424 ms** | **4397.2×** | **932 ms** | **1,065 ms** | **2.51×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 18.2 | 179 ms | 0 | 21.3 | 166 ms | 0 |
| 4 | 87.6 | 189 ms | 0 | 104 | 165 ms | 0 |
| 8 | 207.5 | 166 ms | 0 | 201.4 | 154 ms | 0 |
| 16 | 351 | 178 ms | 0 | 152.9 | 291 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,515 ms | 23 ms | 18 ms | 12 ms | 0.52× |
| Q2: Filtered count | 599 ms | 85 ms | 31 ms | 64 ms | 0.75× |
| Q3: Group by status | 34,741 ms | 179 ms | 64 ms | 76 ms | 0.42× |
| Q4: Region × status breakdown | 13,243 ms | 255 ms | 179 ms | 189 ms | 0.74× |
| Q5: Monthly revenue (2023) | 6,021 ms | 196 ms | 135 ms | 89 ms | 0.45× |
| Q6: Top 10 spenders | 874,288 ms | 523 ms | 225 ms | 173 ms | 0.33× |
| Q7: Regional analytics | 53,635 ms | 548 ms | 161 ms | 181 ms | 0.33× |
| Q8: Join users + orders | 880,361 ms | 578 ms | 191 ms | 175 ms | 0.30× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,094 ms | 417 ms | 2.6× | 70 ms | 61 ms | 0.15× | yes |
| N2: Group by region (novel group column) | 13,078 ms | 810 ms | 16.1× | 160 ms | 191 ms | 0.24× | yes |
| N3: Monthly revenue, novel year | 8,324 ms | 133 ms | 62.6× | 40 ms | 55 ms | 0.41× | yes |
| N4: Regional analytics, novel range | 54,342 ms | 510 ms | 106.6× | 137 ms | 156 ms | 0.31× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 5% | 27 MB | 7% | 419 MB | 100% | 1,541 MB |
| Q2: Filtered count | 9% | 38 MB | 123% | 445 MB | 1% | 1,541 MB |
| Q3: Group by status | 11% | 77 MB | 320% | 489 MB | 85% | 1,541 MB |
| Q4: Region × status breakdown | 16% | 101 MB | 588% | 545 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 3% | 274 MB | 350% | 502 MB | 111% | 1,553 MB |
| Q6: Top 10 spenders | 143% | 558 MB | 630% | 729 MB | 15% | 1,553 MB |
| Q7: Regional analytics | 9% | 583 MB | 548% | 665 MB | 64% | 1,553 MB |
| Q8: Join users + orders | 14% | 564 MB | 647% | 896 MB | 14% | 1,709 MB |

