# Pintail analytical benchmark results

Measured 2026-07-31T10:38:38.860Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | Speedup | CH MergeTree | CH RMT+FINAL | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,300 ms | 153 ms | 15.0× | 181 ms | 167 ms | yes |
| Q2: Filtered count | 1,297 ms | 2,383 ms | 0.5× | 180 ms | 183 ms | yes |
| Q3: Group by status | 67,392 ms | 6,750 ms | 10.0× | 227 ms | 232 ms | yes |
| Q4: Region × status breakdown | 22,896 ms | 10,656 ms | 2.1× | 319 ms | 313 ms | yes |
| Q5: Monthly revenue (2023) | 10,163 ms | 7,131 ms | 1.4× | 192 ms | 324 ms | yes |
| Q6: Top 10 spenders | 1,927,578 ms | 8,914 ms | 216.2× | 274 ms | 263 ms | yes |
| Q7: Regional analytics | 113,438 ms | 13,946 ms | 8.1× | 336 ms | 303 ms | yes |
| Q8: Join users + orders | 1,594,787 ms | 9,758 ms | 163.4× | 554 ms | 470 ms | yes |
| **Total** | **3,739,851 ms** | **59,691 ms** | **62.7×** | **2,263 ms** | **2,255 ms** | |

Release gate: PASS (required ≥50× and exact results).

