# Pintail analytical benchmark results

Measured 2026-09-05T19:03:27.814Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 15 measured runs after 2 warmups; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-09-03T08:22:24.512Z.
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
| Q1: Full table count | 1,437 ms | 12 ms | 119.8× | 10 ms | 12 ms | 1.00× | yes |
| Q2: Filtered count | 587 ms | 12 ms | 48.9× | 29 ms | 31 ms | 2.58× | yes |
| Q3: Group by status | 34,398 ms | 13 ms | 2646.0× | 66 ms | 69 ms | 5.31× | yes |
| Q4: Region × status breakdown | 13,054 ms | 12 ms | 1087.8× | 179 ms | 177 ms | 14.75× | yes |
| Q5: Monthly revenue (2023) | 5,462 ms | 12 ms | 455.2× | 41 ms | 45 ms | 3.75× | yes |
| Q6: Top 10 spenders | 889,417 ms | 76 ms | 11702.9× | 174 ms | 174 ms | 2.29× | yes |
| Q7: Regional analytics | 53,410 ms | 12 ms | 4450.8× | 116 ms | 132 ms | 11.00× | yes |
| Q8: Join users + orders | 796,769 ms | 12 ms | 66397.4× | 166 ms | 168 ms | 14.00× | yes |
| **Total** | **1,794,534 ms** | **161 ms** | **11146.2×** | **781 ms** | **808 ms** | **5.02×** | |

Memo-dashboard release gate: PASS (required ≥50× and exact results; not an engine-speed gate).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 32.3 | 154 ms | 0 | 45.4 | 101 ms | 0 |
| 4 | 142 | 121 ms | 0 | 185.7 | 97 ms | 0 |
| 8 | 268.6 | 142 ms | 0 | 308.3 | 103 ms | 0 |
| 16 | 419.1 | 122 ms | 0 | 354.8 | 144 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,437 ms | 12 ms | 10 ms | 12 ms | 1.00× |
| Q2: Filtered count | 587 ms | 81 ms | 31 ms | 28 ms | 0.35× |
| Q3: Group by status | 34,398 ms | 175 ms | 63 ms | 65 ms | 0.37× |
| Q4: Region × status breakdown | 13,054 ms | 183 ms | 175 ms | 174 ms | 0.95× |
| Q5: Monthly revenue (2023) | 5,462 ms | 129 ms | 40 ms | 44 ms | 0.34× |
| Q6: Top 10 spenders | 889,417 ms | 503 ms | 176 ms | 195 ms | 0.39× |
| Q7: Regional analytics | 53,410 ms | 432 ms | 133 ms | 151 ms | 0.35× |
| Q8: Join users + orders | 796,769 ms | 516 ms | 176 ms | 176 ms | 0.34× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,125 ms | 412 ms | 2.7× | 58 ms | 52 ms | 0.13× | yes |
| N2: Group by region (novel group column) | 12,949 ms | 815 ms | 15.9× | 84 ms | 84 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 8,226 ms | 133 ms | 61.8× | 42 ms | 46 ms | 0.35× | yes |
| N4: Regional analytics, novel range | 56,145 ms | 460 ms | 122.1× | 141 ms | 147 ms | 0.32× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 24 MB | 2% | 399 MB | n/a | n/a |
| Q2: Filtered count | 0% | 33 MB | 96% | 424 MB | n/a | n/a |
| Q3: Group by status | 0% | 39 MB | 397% | 500 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 40 MB | 613% | 539 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 1% | 33 MB | 116% | 458 MB | n/a | n/a |
| Q6: Top 10 spenders | 53% | 119 MB | 736% | 611 MB | n/a | n/a |
| Q7: Regional analytics | 1% | 56 MB | 617% | 552 MB | n/a | n/a |
| Q8: Join users + orders | 1% | 60 MB | 577% | 762 MB | n/a | n/a |

