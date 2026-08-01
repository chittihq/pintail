# Pintail analytical benchmark results

Measured 2026-08-01T16:59:57.821Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T15:50:58.622Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 195 ms | 15.1× | 420 ms | 296 ms | yes |
| Q2: Filtered count | 1,318 ms | 524 ms | 2.5× | 186 ms | 203 ms | yes |
| Q3: Group by status | 61,962 ms | 1,641 ms | 37.8× | 302 ms | 283 ms | yes |
| Q4: Region × status breakdown | 23,291 ms | 3,137 ms | 7.4× | 411 ms | 378 ms | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 2,932 ms | 3.8× | 318 ms | 298 ms | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 2,078 ms | 744.1× | 288 ms | 277 ms | yes |
| Q7: Regional analytics | 112,029 ms | 7,562 ms | 14.8× | 355 ms | 399 ms | yes |
| Q8: Join users + orders | 1,569,431 ms | 5,369 ms | 292.3× | 525 ms | 525 ms | yes |
| **Total** | **3,328,241 ms** | **23,438 ms** | **142.0×** | **2,805 ms** | **2,659 ms** | |

Release gate: PASS (required ≥50× and exact results).

