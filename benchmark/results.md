# Pintail analytical benchmark results

Measured 2026-08-01T19:39:33.497Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 243 ms | 12.2× | 161 ms | 175 ms | 0.72× | yes |
| Q2: Filtered count | 1,318 ms | 345 ms | 3.8× | 188 ms | 187 ms | 0.54× | yes |
| Q3: Group by status | 61,962 ms | 161 ms | 384.9× | 238 ms | 242 ms | 1.50× | yes |
| Q4: Region × status breakdown | 23,291 ms | 161 ms | 144.7× | 322 ms | 321 ms | 1.99× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,583 ms | 7.0× | 207 ms | 205 ms | 0.13× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 219 ms | 7060.4× | 275 ms | 294 ms | 1.34× | yes |
| Q7: Regional analytics | 112,029 ms | 5,245 ms | 21.4× | 306 ms | 295 ms | 0.06× | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,362 ms | 466.8× | 588 ms | 636 ms | 0.19× | yes |
| **Total** | **3,328,241 ms** | **11,319 ms** | **294.0×** | **2,285 ms** | **2,355 ms** | **0.21×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 22 MB | 7% | 557 MB | n/a | n/a |
| Q2: Filtered count | 270% | 55 MB | 42% | 576 MB | n/a | n/a |
| Q3: Group by status | 159% | 76 MB | 238% | 595 MB | n/a | n/a |
| Q4: Region × status breakdown | 263% | 193 MB | 385% | 619 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 495% | 296 MB | 67% | 600 MB | n/a | n/a |
| Q6: Top 10 spenders | 398% | 352 MB | 283% | 763 MB | n/a | n/a |
| Q7: Regional analytics | 323% | 340 MB | 288% | 676 MB | n/a | n/a |
| Q8: Join users + orders | 670% | 594 MB | 407% | 841 MB | n/a | n/a |

