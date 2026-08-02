# Pintail analytical benchmark results

Measured 2026-08-02T06:05:14.508Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 231 ms | 12.8× | 163 ms | 171 ms | 0.74× | yes |
| Q2: Filtered count | 1,318 ms | 164 ms | 8.0× | 192 ms | 192 ms | 1.17× | yes |
| Q3: Group by status | 61,962 ms | 160 ms | 387.3× | 278 ms | 242 ms | 1.51× | yes |
| Q4: Region × status breakdown | 23,291 ms | 159 ms | 146.5× | 319 ms | 319 ms | 2.01× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 166 ms | 66.4× | 204 ms | 208 ms | 1.25× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 205 ms | 7542.6× | 386 ms | 293 ms | 1.43× | yes |
| Q7: Regional analytics | 112,029 ms | 159 ms | 704.6× | 290 ms | 326 ms | 2.05× | yes |
| Q8: Join users + orders | 1,569,431 ms | 2,043 ms | 768.2× | 459 ms | 655 ms | 0.32× | yes |
| **Total** | **3,328,241 ms** | **3,287 ms** | **1012.5×** | **2,291 ms** | **2,406 ms** | **0.73×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 15 MB | 6% | 549 MB | n/a | n/a |
| Q2: Filtered count | 1% | 30 MB | 52% | 577 MB | n/a | n/a |
| Q3: Group by status | 41% | 185 MB | 179% | 643 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 236 MB | 379% | 644 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 109% | 478 MB | 68% | 609 MB | n/a | n/a |
| Q6: Top 10 spenders | 133% | 641 MB | 204% | 742 MB | n/a | n/a |
| Q7: Regional analytics | 2% | 850 MB | 353% | 725 MB | n/a | n/a |
| Q8: Join users + orders | 576% | 1,093 MB | 452% | 829 MB | n/a | n/a |

