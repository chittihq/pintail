# Pintail analytical benchmark results

Measured 2026-08-01T15:51:48.196Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-01T14:23:00.617Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,953 ms | 164 ms | 18.0× | 162 ms | 168 ms | yes |
| Q2: Filtered count | 1,318 ms | 469 ms | 2.8× | 186 ms | 189 ms | yes |
| Q3: Group by status | 61,962 ms | 1,905 ms | 32.5× | 286 ms | 289 ms | yes |
| Q4: Region × status breakdown | 23,291 ms | 3,661 ms | 6.4× | 339 ms | 315 ms | yes |
| Q5: Monthly revenue (2023) | 11,030 ms | 5,623 ms | 2.0× | 225 ms | 213 ms | yes |
| Q6: Top 10 spenders | 1,546,227 ms | 11,679 ms | 132.4× | 323 ms | 266 ms | yes |
| Q7: Regional analytics | 112,029 ms | 9,063 ms | 12.4× | 299 ms | 289 ms | yes |
| Q8: Join users + orders | 1,569,431 ms | 5,874 ms | 267.2× | 605 ms | 584 ms | yes |
| **Total** | **3,328,241 ms** | **38,438 ms** | **86.6×** | **2,425 ms** | **2,313 ms** | |

Release gate: PASS (required ≥50× and exact results).

