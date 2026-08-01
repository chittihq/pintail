# Pintail analytical benchmark results

Measured 2026-08-01T18:34:41.809Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 159 ms | 18.6× | 163 ms | 168 ms | yes |
| Q2: Filtered count | 1,318 ms | 531 ms | 2.5× | 191 ms | 512 ms | yes |
| Q3: Group by status | 61,962 ms | 1,715 ms | 36.1× | 278 ms | 281 ms | yes |
| Q4: Region × status breakdown | 23,291 ms | 2,099 ms | 11.1× | 312 ms | 316 ms | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,574 ms | 7.0× | 276 ms | 301 ms | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 1,958 ms | 789.7× | 278 ms | 280 ms | yes |
| Q7: Regional analytics | 112,029 ms | 5,404 ms | 20.7× | 277 ms | 302 ms | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,286 ms | 477.6× | 547 ms | 528 ms | yes |
| **Total** | **3,328,241 ms** | **16,726 ms** | **199.0×** | **2,322 ms** | **2,688 ms** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 39 MB | 7% | 557 MB | n/a | n/a |
| Q2: Filtered count | 240% | 71 MB | 64% | 577 MB | n/a | n/a |
| Q3: Group by status | 322% | 263 MB | 236% | 660 MB | n/a | n/a |
| Q4: Region × status breakdown | 251% | 294 MB | 285% | 649 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 552% | 236 MB | 126% | 614 MB | n/a | n/a |
| Q6: Top 10 spenders | 404% | 394 MB | 343% | 763 MB | n/a | n/a |
| Q7: Regional analytics | 316% | 406 MB | 263% | 708 MB | n/a | n/a |
| Q8: Join users + orders | 675% | 525 MB | 380% | 843 MB | n/a | n/a |

