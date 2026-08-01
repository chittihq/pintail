# Pintail analytical benchmark results

Measured 2026-08-01T20:22:05.360Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 162 ms | 18.2× | 159 ms | 169 ms | 1.04× | yes |
| Q2: Filtered count | 1,318 ms | 317 ms | 4.2× | 186 ms | 197 ms | 0.62× | yes |
| Q3: Group by status | 61,962 ms | 174 ms | 356.1× | 286 ms | 255 ms | 1.47× | yes |
| Q4: Region × status breakdown | 23,291 ms | 169 ms | 137.8× | 332 ms | 332 ms | 1.96× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 1,584 ms | 7.0× | 204 ms | 276 ms | 0.17× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 215 ms | 7191.8× | 269 ms | 286 ms | 1.33× | yes |
| Q7: Regional analytics | 112,029 ms | 5,247 ms | 21.4× | 273 ms | 321 ms | 0.06× | yes |
| Q8: Join users + orders | 1,569,431 ms | 3,077 ms | 510.1× | 575 ms | 588 ms | 0.19× | yes |
| **Total** | **3,328,241 ms** | **10,945 ms** | **304.1×** | **2,284 ms** | **2,424 ms** | **0.22×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 16 MB | 13% | 565 MB | n/a | n/a |
| Q2: Filtered count | 227% | 40 MB | 5% | 584 MB | n/a | n/a |
| Q3: Group by status | 251% | 164 MB | 177% | 611 MB | n/a | n/a |
| Q4: Region × status breakdown | 57% | 197 MB | 360% | 639 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 440% | 339 MB | 109% | 618 MB | n/a | n/a |
| Q6: Top 10 spenders | 286% | 356 MB | 111% | 747 MB | n/a | n/a |
| Q7: Regional analytics | 314% | 365 MB | 184% | 696 MB | n/a | n/a |
| Q8: Join users + orders | 668% | 598 MB | 419% | 849 MB | n/a | n/a |

