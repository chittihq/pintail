# Pintail analytical benchmark results

Measured 2026-07-30T19:32:58.706Z with 20,000,000 orders.

| Query | MySQL | Pintail | Speedup | ClickHouse reference |
|---|---:|---:|---:|---:|
| Q1: Full table count | 2,334 ms | 24 ms | 97.3× | 368 ms |
| Q2: Filtered count | 1,679 ms | 1,495 ms | 1.1× | 751 ms |
| Q3: Group by status | 68,947 ms | 2,972 ms | 23.2× | 543 ms |
| Q4: Region × status breakdown | 23,273 ms | 3,036 ms | 7.7× | 594 ms |
| Q5: Monthly revenue (2023) | 11,042 ms | 2,064 ms | 5.3× | 857 ms |
| Q6: Top 10 spenders | 1,977,816 ms | 3,146 ms | 628.7× | 852 ms |
| Q7: Regional analytics | 117,478 ms | 4,649 ms | 25.3× | 585 ms |
| Q8: Join users + orders | 1,638,868 ms | 4,819 ms | 340.1× | 641 ms |
| **Total** | **3,841,437 ms** | **22,205 ms** | **173.0×** | **5,191 ms** |

Release gate: PASS (required ≥50×).

