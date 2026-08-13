# Pintail analytical benchmark results

Measured 2026-08-13T13:19:47.200Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-13T13:15:20.746Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty.

NOT like for like: the canonical table is served from pintail's settled
aggregate memo, while ClickHouse's query cache is off and it executes every
run. It measures what a repeated dashboard query costs, not engine speed.
The novel-query table below is the engine-speed comparison - both engines
execute there, and ClickHouse is currently faster.

## Repeated queries (memo-served — dashboard refresh cost, not engine speed)

| Query | MySQL | Pintail (memo) | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,498 ms | 11 ms | 136.2× | 9 ms | 11 ms | 1.00× | yes |
| Q2: Filtered count | 592 ms | 12 ms | 49.3× | 29 ms | 28 ms | 2.33× | yes |
| Q3: Group by status | 34,602 ms | 9 ms | 3844.7× | 65 ms | 68 ms | 7.56× | yes |
| Q4: Region × status breakdown | 13,545 ms | 15 ms | 903.0× | 175 ms | 194 ms | 12.93× | yes |
| Q5: Monthly revenue (2023) | 5,730 ms | 10 ms | 573.0× | 41 ms | 42 ms | 4.20× | yes |
| Q6: Top 10 spenders | 881,461 ms | 73 ms | 12074.8× | 168 ms | 174 ms | 2.38× | yes |
| Q7: Regional analytics | 54,031 ms | 12 ms | 4502.6× | 119 ms | 133 ms | 11.08× | yes |
| Q8: Join users + orders | 880,985 ms | 13 ms | 67768.1× | 168 ms | 162 ms | 12.46× | yes |
| **Total** | **1,872,444 ms** | **155 ms** | **12080.3×** | **774 ms** | **812 ms** | **5.24×** | |

Release gate: PASS (required ≥50× and exact results).

## Concurrency (memo disabled — both engines executing)

One client measures an engine at rest. This is the shape a server
actually meets, and where admission, memory accounting and lock
contention appear. Throughput and p95 together: throughput alone can
rise while the slowest decile becomes unusable, and a flat p95 can
hide an engine that has stopped accepting work.

| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 43.9 | 59 ms | 0 | 71.7 | 22 ms | 0 |
| 4 | 233.1 | 38 ms | 0 | 200.4 | 50 ms | 0 |
| 8 | 239.6 | 106 ms | 0 | 217.4 | 116 ms | 0 |
| 16 | 375.1 | 108 ms | 0 | 235.9 | 304 ms | 0 |

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,498 ms | 11 ms | 10 ms | 10 ms | 0.91× |
| Q2: Filtered count | 592 ms | 145 ms | 33 ms | 31 ms | 0.21× |
| Q3: Group by status | 34,602 ms | 286 ms | 74 ms | 69 ms | 0.24× |
| Q4: Region × status breakdown | 13,545 ms | 443 ms | 180 ms | 181 ms | 0.41× |
| Q5: Monthly revenue (2023) | 5,730 ms | 518 ms | 39 ms | 58 ms | 0.11× |
| Q6: Top 10 spenders | 881,461 ms | 888 ms | 175 ms | 174 ms | 0.20× |
| Q7: Regional analytics | 54,031 ms | 1,067 ms | 130 ms | 133 ms | 0.12× |
| Q8: Join users + orders | 880,985 ms | 915 ms | 202 ms | 169 ms | 0.18× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,213 ms | 607 ms | 2.0× | 134 ms | 89 ms | 0.15× | yes |
| N2: Group by region (novel group column) | 13,038 ms | 1,103 ms | 11.8× | 87 ms | 88 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 8,074 ms | 500 ms | 16.1× | 69 ms | 63 ms | 0.13× | yes |
| N4: Regional analytics, novel range | 54,883 ms | 1,050 ms | 52.3× | 146 ms | 148 ms | 0.14× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 30 MB | 7% | 429 MB | 100% | 1,540 MB |
| Q2: Filtered count | 0% | 43 MB | 7% | 451 MB | 1% | 1,540 MB |
| Q3: Group by status | 0% | 131 MB | 152% | 508 MB | 85% | 1,540 MB |
| Q4: Region × status breakdown | 108% | 184 MB | 747% | 547 MB | 107% | 1,552 MB |
| Q5: Monthly revenue (2023) | 0% | 491 MB | 7% | 509 MB | 111% | 1,552 MB |
| Q6: Top 10 spenders | 80% | 691 MB | 743% | 730 MB | 13% | 1,552 MB |
| Q7: Regional analytics | 5% | 847 MB | 634% | 664 MB | 63% | 1,552 MB |
| Q8: Join users + orders | 9% | 1,010 MB | 553% | 886 MB | 14% | 1,707 MB |

