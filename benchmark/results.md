# Pintail analytical benchmark results

Measured 2026-08-02T04:21:31.337Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 158 ms | 18.7× | 160 ms | 167 ms | 1.06× | yes |
| Q2: Filtered count | 1,318 ms | 337 ms | 3.9× | 184 ms | 208 ms | 0.62× | yes |
| Q3: Group by status | 61,962 ms | 157 ms | 394.7× | 237 ms | 250 ms | 1.59× | yes |
| Q4: Region × status breakdown | 23,291 ms | 159 ms | 146.5× | 315 ms | 325 ms | 2.04× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 833 ms | 13.2× | 204 ms | 207 ms | 0.25× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 209 ms | 7398.2× | 274 ms | 274 ms | 1.31× | yes |
| Q7: Regional analytics | 112,029 ms | 1,623 ms | 69.0× | 290 ms | 302 ms | 0.19× | yes |
| Q8: Join users + orders | 1,569,431 ms | 2,939 ms | 534.0× | 468 ms | 624 ms | 0.21× | yes |
| **Total** | **3,328,241 ms** | **6,415 ms** | **518.8×** | **2,132 ms** | **2,357 ms** | **0.37×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 28 MB | 5% | 537 MB | n/a | n/a |
| Q2: Filtered count | 283% | 65 MB | 62% | 573 MB | n/a | n/a |
| Q3: Group by status | 159% | 244 MB | 244% | 621 MB | n/a | n/a |
| Q4: Region × status breakdown | 123% | 363 MB | 373% | 613 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 420% | 503 MB | 66% | 593 MB | n/a | n/a |
| Q6: Top 10 spenders | 258% | 666 MB | 347% | 744 MB | n/a | n/a |
| Q7: Regional analytics | 313% | 937 MB | 184% | 655 MB | n/a | n/a |
| Q8: Join users + orders | 632% | 1,152 MB | 535% | 874 MB | n/a | n/a |

