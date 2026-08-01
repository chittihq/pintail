# Pintail analytical benchmark results

Measured 2026-08-01T19:21:12.188Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 213 ms | 13.9× | 160 ms | 166 ms | 0.78× | yes |
| Q2: Filtered count | 1,318 ms | 396 ms | 3.3× | 198 ms | 194 ms | 0.49× | yes |
| Q3: Group by status | 61,962 ms | 164 ms | 377.8× | 284 ms | 248 ms | 1.51× | yes |
| Q4: Region × status breakdown | 23,291 ms | 178 ms | 130.8× | 314 ms | 314 ms | 1.76× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,537 ms | 7.2× | 205 ms | 208 ms | 0.14× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 212 ms | 7293.5× | 270 ms | 275 ms | 1.30× | yes |
| Q7: Regional analytics | 112,029 ms | 5,330 ms | 21.0× | 284 ms | 302 ms | 0.06× | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,613 ms | 434.4× | 494 ms | 568 ms | 0.16× | yes |
| **Total** | **3,328,241 ms** | **11,643 ms** | **285.9×** | **2,209 ms** | **2,275 ms** | **0.20×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 16 MB | 7% | 544 MB | n/a | n/a |
| Q2: Filtered count | 90% | 44 MB | 45% | 575 MB | n/a | n/a |
| Q3: Group by status | 296% | 161 MB | 178% | 591 MB | n/a | n/a |
| Q4: Region × status breakdown | 258% | 179 MB | 361% | 625 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 544% | 111 MB | 4% | 589 MB | n/a | n/a |
| Q6: Top 10 spenders | 261% | 241 MB | 342% | 717 MB | n/a | n/a |
| Q7: Regional analytics | 317% | 251 MB | 184% | 670 MB | n/a | n/a |
| Q8: Join users + orders | 674% | 521 MB | 231% | 837 MB | n/a | n/a |

