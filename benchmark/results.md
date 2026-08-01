# Pintail analytical benchmark results

Measured 2026-08-01T22:08:53.444Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 165 ms | 17.9× | 214 ms | 210 ms | 1.27× | yes |
| Q2: Filtered count | 1,318 ms | 321 ms | 4.1× | 249 ms | 232 ms | 0.72× | yes |
| Q3: Group by status | 61,962 ms | 169 ms | 366.6× | 527 ms | 512 ms | 3.03× | yes |
| Q4: Region × status breakdown | 23,291 ms | 168 ms | 138.6× | 619 ms | 527 ms | 3.14× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,129 ms | 9.8× | 431 ms | 498 ms | 0.44× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 232 ms | 6664.8× | 541 ms | 483 ms | 2.08× | yes |
| Q7: Regional analytics | 112,029 ms | 2,108 ms | 53.1× | 304 ms | 447 ms | 0.21× | yes |
| Q8: Join users + orders | 1,569,431 ms | 2,846 ms | 551.5× | 635 ms | 567 ms | 0.20× | yes |
| **Total** | **3,328,241 ms** | **7,138 ms** | **466.3×** | **3,520 ms** | **3,476 ms** | **0.49×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 22 MB | 7% | 556 MB | n/a | n/a |
| Q2: Filtered count | 224% | 61 MB | 84% | 586 MB | n/a | n/a |
| Q3: Group by status | 156% | 162 MB | 124% | 609 MB | n/a | n/a |
| Q4: Region × status breakdown | 201% | 114 MB | 211% | 641 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 269% | 247 MB | 66% | 613 MB | n/a | n/a |
| Q6: Top 10 spenders | 340% | 333 MB | 175% | 748 MB | n/a | n/a |
| Q7: Regional analytics | 249% | 422 MB | 198% | 695 MB | n/a | n/a |
| Q8: Join users + orders | 665% | 622 MB | 347% | 858 MB | n/a | n/a |

