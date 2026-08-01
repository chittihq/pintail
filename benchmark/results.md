# Pintail analytical benchmark results

Measured 2026-08-01T21:18:55.373Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 160 ms | 18.5× | 206 ms | 172 ms | 1.07× | yes |
| Q2: Filtered count | 1,318 ms | 319 ms | 4.1× | 203 ms | 191 ms | 0.60× | yes |
| Q3: Group by status | 61,962 ms | 218 ms | 284.2× | 256 ms | 235 ms | 1.08× | yes |
| Q4: Region × status breakdown | 23,291 ms | 159 ms | 146.5× | 312 ms | 313 ms | 1.97× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,572 ms | 7.0× | 202 ms | 211 ms | 0.13× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 210 ms | 7363.0× | 277 ms | 273 ms | 1.30× | yes |
| Q7: Regional analytics | 112,029 ms | 3,290 ms | 34.1× | 296 ms | 307 ms | 0.09× | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,083 ms | 509.1× | 685 ms | 617 ms | 0.20× | yes |
| **Total** | **3,328,241 ms** | **9,011 ms** | **369.4×** | **2,437 ms** | **2,319 ms** | **0.26×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 15 MB | 13% | 551 MB | n/a | n/a |
| Q2: Filtered count | 81% | 45 MB | 60% | 639 MB | n/a | n/a |
| Q3: Group by status | 302% | 178 MB | 187% | 599 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 144 MB | 349% | 644 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 505% | 274 MB | 94% | 647 MB | n/a | n/a |
| Q6: Top 10 spenders | 394% | 330 MB | 328% | 726 MB | n/a | n/a |
| Q7: Regional analytics | 318% | 465 MB | 165% | 716 MB | n/a | n/a |
| Q8: Join users + orders | 668% | 636 MB | 204% | 815 MB | n/a | n/a |

