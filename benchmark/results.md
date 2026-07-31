# Pintail analytical benchmark results

Measured 2026-07-31T14:06:37.262Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,400 ms | 303 ms | 7.9× | 294 ms | 298 ms | yes |
| Q2: Filtered count | 1,327 ms | 2,560 ms | 0.5× | 305 ms | 320 ms | yes |
| Q3: Group by status | 66,651 ms | 5,935 ms | 11.2× | 380 ms | 381 ms | yes |
| Q4: Region × status breakdown | 22,669 ms | 9,687 ms | 2.3× | 453 ms | 455 ms | yes |
| Q5: Monthly revenue (2023) | 10,612 ms | 6,775 ms | 1.6× | 353 ms | 365 ms | yes |
| Q6: Top 10 spenders | 1,913,593 ms | 9,057 ms | 211.3× | 427 ms | 424 ms | yes |
| Q7: Regional analytics | 115,018 ms | 13,026 ms | 8.8× | 443 ms | 465 ms | yes |
| Q8: Join users + orders | 1,594,726 ms | 9,121 ms | 174.8× | 883 ms | 904 ms | yes |
| **Total** | **3,726,996 ms** | **56,464 ms** | **66.0×** | **3,538 ms** | **3,612 ms** | |

Release gate: PASS (required ≥50× and exact results).

