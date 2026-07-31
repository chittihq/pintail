# Pintail analytical benchmark results

Measured 2026-07-31T08:40:40.290Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,442 ms | 325 ms | 7.5× | 318 ms | 674 ms | yes |
| Q2: Filtered count | 1,467 ms | 2,102 ms | 0.7× | 218 ms | 204 ms | yes |
| Q3: Group by status | 67,183 ms | 5,992 ms | 11.2× | 247 ms | 237 ms | yes |
| Q4: Region × status breakdown | 23,070 ms | 9,517 ms | 2.4× | 344 ms | 337 ms | yes |
| Q5: Monthly revenue (2023) | 10,686 ms | 6,320 ms | 1.7× | 208 ms | 216 ms | yes |
| Q6: Top 10 spenders | 1,938,080 ms | 9,077 ms | 213.5× | 276 ms | 293 ms | yes |
| Q7: Regional analytics | 115,501 ms | 13,183 ms | 8.8× | 289 ms | 307 ms | yes |
| Q8: Join users + orders | 1,611,999 ms | 9,005 ms | 179.0× | 624 ms | 634 ms | yes |
| **Total** | **3,770,428 ms** | **55,521 ms** | **67.9×** | **2,524 ms** | **2,902 ms** | |

Release gate: PASS (required ≥50× and exact results).

