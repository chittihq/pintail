# Pintail analytical benchmark results

Measured 2026-08-02T06:48:32.046Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 163 ms | 18.1× | 167 ms | 166 ms | 1.02× | yes |
| Q2: Filtered count | 1,318 ms | 163 ms | 8.1× | 186 ms | 188 ms | 1.15× | yes |
| Q3: Group by status | 61,962 ms | 168 ms | 368.8× | 230 ms | 240 ms | 1.43× | yes |
| Q4: Region × status breakdown | 23,291 ms | 162 ms | 143.8× | 307 ms | 314 ms | 1.94× | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 162 ms | 68.1× | 205 ms | 215 ms | 1.33× | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 211 ms | 7328.1× | 266 ms | 279 ms | 1.32× | yes |
| Q7: Regional analytics | 112,029 ms | 162 ms | 691.5× | 289 ms | 306 ms | 1.89× | yes |
| Q8: Join users + orders | 1,569,431 ms | 163 ms | 9628.4× | 608 ms | 601 ms | 3.69× | yes |
| **Total** | **3,328,241 ms** | **1,354 ms** | **2458.1×** | **2,258 ms** | **2,309 ms** | **1.71×** | |

Release gate: PASS (required ≥50× and exact results).

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 19 MB | 8% | 551 MB | n/a | n/a |
| Q2: Filtered count | 1% | 33 MB | 42% | 581 MB | n/a | n/a |
| Q3: Group by status | 31% | 188 MB | 172% | 581 MB | n/a | n/a |
| Q4: Region × status breakdown | 42% | 239 MB | 410% | 646 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 94% | 479 MB | 103% | 593 MB | n/a | n/a |
| Q6: Top 10 spenders | 172% | 639 MB | 271% | 730 MB | n/a | n/a |
| Q7: Regional analytics | 338% | 838 MB | 299% | 661 MB | n/a | n/a |
| Q8: Join users + orders | 581% | 920 MB | 241% | 804 MB | n/a | n/a |

