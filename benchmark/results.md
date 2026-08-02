# Pintail analytical benchmark results

Measured 2026-08-02T05:20:31.081Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 177 ms | 16.7× | 160 ms | 196 ms | 1.11× | yes |
| Q2: Filtered count | 1,318 ms | 346 ms | 3.8× | 197 ms | 186 ms | 0.54× | yes |
| Q3: Group by status | 61,962 ms | 169 ms | 366.6× | 284 ms | 244 ms | 1.44× | yes |
| Q4: Region × status breakdown | 23,291 ms | 168 ms | 138.6× | 355 ms | 347 ms | 2.07× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 840 ms | 13.1× | 198 ms | 212 ms | 0.25× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 218 ms | 7092.8× | 295 ms | 285 ms | 1.31× | yes |
| Q7: Regional analytics | 112,029 ms | 1,669 ms | 67.1× | 316 ms | 316 ms | 0.19× | yes |
| Q8: Join users + orders | 1,569,431 ms | 2,061 ms | 761.5× | 571 ms | 607 ms | 0.29× | yes |
| **Total** | **3,328,241 ms** | **5,648 ms** | **589.3×** | **2,376 ms** | **2,393 ms** | **0.42×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 29 MB | 5% | 543 MB | n/a | n/a |
| Q2: Filtered count | 158% | 44 MB | 64% | 573 MB | n/a | n/a |
| Q3: Group by status | 81% | 197 MB | 125% | 592 MB | n/a | n/a |
| Q4: Region × status breakdown | 130% | 247 MB | 377% | 668 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 365% | 509 MB | 130% | 596 MB | n/a | n/a |
| Q6: Top 10 spenders | 256% | 654 MB | 324% | 754 MB | n/a | n/a |
| Q7: Regional analytics | 361% | 906 MB | 302% | 657 MB | n/a | n/a |
| Q8: Join users + orders | 583% | 1,196 MB | 398% | 818 MB | n/a | n/a |

