# Pintail analytical benchmark results

Measured 2026-08-02T05:41:03.975Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 167 ms | 17.7× | 160 ms | 186 ms | 1.11× | yes |
| Q2: Filtered count | 1,318 ms | 316 ms | 4.2× | 186 ms | 187 ms | 0.59× | yes |
| Q3: Group by status | 61,962 ms | 159 ms | 389.7× | 274 ms | 236 ms | 1.48× | yes |
| Q4: Region × status breakdown | 23,291 ms | 161 ms | 144.7× | 320 ms | 314 ms | 1.95× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 781 ms | 14.1× | 203 ms | 207 ms | 0.27× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 212 ms | 7293.5× | 274 ms | 284 ms | 1.34× | yes |
| Q7: Regional analytics | 112,029 ms | 1,606 ms | 69.8× | 303 ms | 476 ms | 0.30× | yes |
| Q8: Join users + orders | 1,569,431 ms | 1,953 ms | 803.6× | 627 ms | 592 ms | 0.30× | yes |
| **Total** | **3,328,241 ms** | **5,355 ms** | **621.5×** | **2,347 ms** | **2,482 ms** | **0.46×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 5% | 532 MB | n/a | n/a |
| Q2: Filtered count | 110% | 41 MB | 43% | 559 MB | n/a | n/a |
| Q3: Group by status | 1% | 194 MB | 65% | 577 MB | n/a | n/a |
| Q4: Region × status breakdown | 1% | 239 MB | 328% | 629 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 446% | 499 MB | 91% | 586 MB | n/a | n/a |
| Q6: Top 10 spenders | 158% | 640 MB | 240% | 690 MB | n/a | n/a |
| Q7: Regional analytics | 363% | 880 MB | 195% | 643 MB | n/a | n/a |
| Q8: Join users + orders | 570% | 1,116 MB | 323% | 836 MB | n/a | n/a |

