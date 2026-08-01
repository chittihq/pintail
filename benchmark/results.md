# Pintail analytical benchmark results

Measured 2026-08-01T05:43:10.054Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,257 ms | 175 ms | 12.9× | 176 ms | 161 ms | yes |
| Q2: Filtered count | 1,294 ms | 566 ms | 2.3× | 180 ms | 184 ms | yes |
| Q3: Group by status | 68,196 ms | 6,040 ms | 11.3× | 238 ms | 284 ms | yes |
| Q4: Region × status breakdown | 23,082 ms | 8,393 ms | 2.8× | 312 ms | 309 ms | yes |
| Q5: Monthly revenue (2023) | 10,609 ms | 13,079 ms | 0.8× | 198 ms | 202 ms | yes |
| Q6: Top 10 spenders | 1,971,306 ms | 78,809 ms | 25.0× | 309 ms | 287 ms | yes |
| Q7: Regional analytics | 113,469 ms | 18,302 ms | 6.2× | 283 ms | 313 ms | yes |
| Q8: Join users + orders | 1,598,090 ms | 9,071 ms | 176.2× | 576 ms | 591 ms | yes |
| **Total** | **3,788,303 ms** | **134,435 ms** | **28.2×** | **2,272 ms** | **2,331 ms** | |

Release gate: FAIL (required ≥50× and exact results).

