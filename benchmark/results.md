# Pintail analytical benchmark results

Measured 2026-08-13T06:29:03.017Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-13T06:25:48.317Z.
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
| Q1: Full table count | 1,505 ms | 16 ms | 94.1× | 11 ms | 15 ms | 0.94× | yes |
| Q2: Filtered count | 596 ms | 15 ms | 39.7× | 41 ms | 34 ms | 2.27× | yes |
| Q3: Group by status | 35,395 ms | 16 ms | 2212.2× | 127 ms | 91 ms | 5.69× | yes |
| Q4: Region × status breakdown | 13,784 ms | 13 ms | 1060.3× | 273 ms | 256 ms | 19.69× | yes |
| Q5: Monthly revenue (2023) | 5,726 ms | 14 ms | 409.0× | 42 ms | 47 ms | 3.36× | yes |
| Q6: Top 10 spenders | 821,652 ms | 102 ms | 8055.4× | 255 ms | 249 ms | 2.44× | yes |
| Q7: Regional analytics | 56,606 ms | 13 ms | 4354.3× | 147 ms | 172 ms | 13.23× | yes |
| Q8: Join users + orders | 836,232 ms | 15 ms | 55748.8× | 209 ms | 211 ms | 14.07× | yes |
| **Total** | **1,771,496 ms** | **204 ms** | **8683.8×** | **1,105 ms** | **1,075 ms** | **5.27×** | |

Release gate: PASS (required ≥50× and exact results).

## Engine speed (memo DISABLED — both engines execute)

The canonical queries against a pintail restarted with its settled
aggregate memo off, on the same replica. This is the like-for-like
comparison: the table at the top measures a cache hit against
ClickHouse's execution, which is a different question.

| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1,505 ms | 13 ms | 10 ms | 13 ms | 1.00× |
| Q2: Filtered count | 596 ms | 93 ms | 34 ms | 32 ms | 0.34× |
| Q3: Group by status | 35,395 ms | 310 ms | 65 ms | 64 ms | 0.21× |
| Q4: Region × status breakdown | 13,784 ms | 394 ms | 242 ms | 285 ms | 0.72× |
| Q5: Monthly revenue (2023) | 5,726 ms | 521 ms | 44 ms | 46 ms | 0.09× |
| Q6: Top 10 spenders | 821,652 ms | 1,022 ms | 282 ms | 253 ms | 0.25× |
| Q7: Regional analytics | 56,606 ms | 1,050 ms | 148 ms | 175 ms | 0.17× |
| Q8: Join users + orders | 836,232 ms | 942 ms | 203 ms | 201 ms | 0.21× |

## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)

Both engines execute every run here. This is the comparison that speaks
to execution performance.

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,090 ms | 621 ms | 1.8× | 59 ms | 52 ms | 0.08× | yes |
| N2: Group by region (novel group column) | 13,998 ms | 1,243 ms | 11.3× | 197 ms | 88 ms | 0.07× | yes |
| N3: Monthly revenue, novel year | 9,192 ms | 493 ms | 18.6× | 49 ms | 45 ms | 0.09× | yes |
| N4: Regional analytics, novel range | 57,453 ms | 1,041 ms | 55.2× | 277 ms | 190 ms | 0.18× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 38 MB | 7% | 446 MB | 52% | 1,541 MB |
| Q2: Filtered count | 0% | 52 MB | 49% | 473 MB | 0% | 1,540 MB |
| Q3: Group by status | 4% | 139 MB | 124% | 519 MB | 83% | 1,541 MB |
| Q4: Region × status breakdown | 1% | 196 MB | 678% | 564 MB | 106% | 1,552 MB |
| Q5: Monthly revenue (2023) | 0% | 499 MB | 3% | 522 MB | 109% | 1,552 MB |
| Q6: Top 10 spenders | 62% | 698 MB | 707% | 753 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 98% | 871 MB | 573% | 659 MB | 60% | 1,553 MB |
| Q8: Join users + orders | 6% | 995 MB | 588% | 871 MB | 13% | 1,705 MB |

