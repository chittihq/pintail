# Pintail analytical benchmark results

Measured 2026-08-02T04:38:56.243Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 214 ms | 13.8× | 184 ms | 171 ms | 0.80× | yes |
| Q2: Filtered count | 1,318 ms | 398 ms | 3.3× | 202 ms | 203 ms | 0.51× | yes |
| Q3: Group by status | 61,962 ms | 179 ms | 346.2× | 348 ms | 265 ms | 1.48× | yes |
| Q4: Region × status breakdown | 23,291 ms | 169 ms | 137.8× | 374 ms | 327 ms | 1.93× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 829 ms | 13.3× | 208 ms | 1,418 ms | 1.71× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 214 ms | 7225.4× | 507 ms | 512 ms | 2.39× | yes |
| Q7: Regional analytics | 112,029 ms | 1,662 ms | 67.4× | 287 ms | 283 ms | 0.17× | yes |
| Q8: Join users + orders | 1,569,431 ms | 1,937 ms | 810.2× | 467 ms | 429 ms | 0.22× | yes |
| **Total** | **3,328,241 ms** | **5,602 ms** | **594.1×** | **2,577 ms** | **3,608 ms** | **0.64×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 21 MB | 6% | 558 MB | n/a | n/a |
| Q2: Filtered count | 201% | 47 MB | 64% | 629 MB | n/a | n/a |
| Q3: Group by status | 91% | 243 MB | 173% | 613 MB | n/a | n/a |
| Q4: Region × status breakdown | 1% | 360 MB | 350% | 657 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 456% | 495 MB | 95% | 611 MB | n/a | n/a |
| Q6: Top 10 spenders | 184% | 643 MB | 110% | 758 MB | n/a | n/a |
| Q7: Regional analytics | 358% | 908 MB | 307% | 690 MB | n/a | n/a |
| Q8: Join users + orders | 583% | 1,191 MB | 437% | 864 MB | n/a | n/a |

