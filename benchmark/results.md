# Pintail analytical benchmark results

Measured 2026-08-01T19:01:37.428Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 166 ms | 17.8× | 166 ms | 167 ms | 1.01× | yes |
| Q2: Filtered count | 1,318 ms | 498 ms | 2.6× | 188 ms | 209 ms | 0.42× | yes |
| Q3: Group by status | 61,962 ms | 160 ms | 387.3× | 244 ms | 279 ms | 1.74× | yes |
| Q4: Region × status breakdown | 23,291 ms | 160 ms | 145.6× | 321 ms | 304 ms | 1.90× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,568 ms | 7.0× | 208 ms | 210 ms | 0.13× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 204 ms | 7579.5× | 286 ms | 301 ms | 1.48× | yes |
| Q7: Regional analytics | 112,029 ms | 5,390 ms | 20.8× | 284 ms | 290 ms | 0.05× | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,227 ms | 486.3× | 608 ms | 646 ms | 0.20× | yes |
| **Total** | **3,328,241 ms** | **11,373 ms** | **292.6×** | **2,305 ms** | **2,406 ms** | **0.21×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 21 MB | 7% | 554 MB | n/a | n/a |
| Q2: Filtered count | 253% | 50 MB | 42% | 582 MB | n/a | n/a |
| Q3: Group by status | 308% | 168 MB | 186% | 596 MB | n/a | n/a |
| Q4: Region × status breakdown | 263% | 201 MB | 306% | 650 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 637% | 152 MB | 102% | 605 MB | n/a | n/a |
| Q6: Top 10 spenders | 162% | 259 MB | 147% | 734 MB | n/a | n/a |
| Q7: Regional analytics | 313% | 268 MB | 292% | 740 MB | n/a | n/a |
| Q8: Join users + orders | 666% | 558 MB | 431% | 869 MB | n/a | n/a |

