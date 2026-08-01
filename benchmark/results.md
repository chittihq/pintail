# Pintail analytical benchmark results

Measured 2026-08-01T22:26:29.754Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 170 ms | 17.4× | 168 ms | 171 ms | 1.01× | yes |
| Q2: Filtered count | 1,318 ms | 365 ms | 3.6× | 200 ms | 198 ms | 0.54× | yes |
| Q3: Group by status | 61,962 ms | 195 ms | 317.8× | 298 ms | 239 ms | 1.23× | yes |
| Q4: Region × status breakdown | 23,291 ms | 166 ms | 140.3× | 318 ms | 341 ms | 2.05× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 896 ms | 12.3× | 219 ms | 234 ms | 0.26× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 229 ms | 6752.1× | 308 ms | 288 ms | 1.26× | yes |
| Q7: Regional analytics | 112,029 ms | 2,217 ms | 50.5× | 815 ms | 309 ms | 0.14× | yes |
| Q8: Join users + orders | 1,569,431 ms | 2,809 ms | 558.7× | 646 ms | 443 ms | 0.16× | yes |
| **Total** | **3,328,241 ms** | **7,047 ms** | **472.3×** | **2,972 ms** | **2,223 ms** | **0.32×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 32 MB | 5% | 553 MB | n/a | n/a |
| Q2: Filtered count | 76% | 58 MB | 41% | 566 MB | n/a | n/a |
| Q3: Group by status | 203% | 78 MB | 69% | 589 MB | n/a | n/a |
| Q4: Region × status breakdown | 178% | 93 MB | 370% | 641 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 453% | 509 MB | 95% | 600 MB | n/a | n/a |
| Q6: Top 10 spenders | 215% | 632 MB | 302% | 725 MB | n/a | n/a |
| Q7: Regional analytics | 259% | 764 MB | 266% | 666 MB | n/a | n/a |
| Q8: Join users + orders | 660% | 938 MB | 407% | 847 MB | n/a | n/a |

