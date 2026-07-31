# Pintail analytical benchmark results

Measured 2026-07-31T12:39:59.676Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,528 ms | 297 ms | 8.5× | 285 ms | 290 ms | yes |
| Q2: Filtered count | 1,424 ms | 2,642 ms | 0.5× | 308 ms | 316 ms | yes |
| Q3: Group by status | 66,992 ms | 6,157 ms | 10.9× | 383 ms | 370 ms | yes |
| Q4: Region × status breakdown | 22,561 ms | 9,718 ms | 2.3× | 477 ms | 458 ms | yes |
| Q5: Monthly revenue (2023) | 10,466 ms | 7,006 ms | 1.5× | 343 ms | 340 ms | yes |
| Q6: Top 10 spenders | 1,921,484 ms | 9,443 ms | 203.5× | 422 ms | 423 ms | MISMATCH |
| Q7: Regional analytics | 113,718 ms | 13,165 ms | 8.6× | 430 ms | 443 ms | yes |
| Q8: Join users + orders | 1,583,275 ms | 9,350 ms | 169.3× | 626 ms | 606 ms | yes |
| **Total** | **3,722,448 ms** | **57,778 ms** | **64.4×** | **3,274 ms** | **3,246 ms** | |

Release gate: FAIL (required ≥50× and exact results).

