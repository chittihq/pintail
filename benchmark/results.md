# Pintail analytical benchmark results

Measured 2026-08-01T17:32:58.310Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 159 ms | 18.6× | 165 ms | 170 ms | yes |
| Q2: Filtered count | 1,318 ms | 531 ms | 2.5× | 190 ms | 188 ms | yes |
| Q3: Group by status | 61,962 ms | 1,637 ms | 37.9× | 245 ms | 235 ms | yes |
| Q4: Region × status breakdown | 23,291 ms | 3,142 ms | 7.4× | 305 ms | 326 ms | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,641 ms | 6.7× | 268 ms | 301 ms | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 2,217 ms | 697.4× | 329 ms | 287 ms | yes |
| Q7: Regional analytics | 112,029 ms | 5,240 ms | 21.4× | 287 ms | 407 ms | yes |
| Q8: Join users + orders | 1,569,431 ms | 5,309 ms | 295.6× | 592 ms | 619 ms | yes |
| **Total** | **3,328,241 ms** | **19,876 ms** | **167.5×** | **2,381 ms** | **2,533 ms** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 31 MB | 6% | 542 MB | n/a | n/a |
| Q2: Filtered count | 186% | 59 MB | 43% | 571 MB | n/a | n/a |
| Q3: Group by status | 441% | 91 MB | 190% | 601 MB | n/a | n/a |
| Q4: Region × status breakdown | 347% | 110 MB | 334% | 627 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 630% | 118 MB | 124% | 610 MB | n/a | n/a |
| Q6: Top 10 spenders | 414% | 338 MB | 280% | 738 MB | n/a | n/a |
| Q7: Regional analytics | 318% | 289 MB | 312% | 713 MB | n/a | n/a |
| Q8: Join users + orders | 577% | 575 MB | 345% | 828 MB | n/a | n/a |

