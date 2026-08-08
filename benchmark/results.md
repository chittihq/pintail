# Pintail analytical benchmark results

Measured 2026-08-08T10:33:54.635Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-08T07:55:28.496Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 28 ms | 50.8× | 30 ms | 29 ms | 1.04× | yes |
| Q2: Filtered count | 607 ms | 26 ms | 23.3× | 60 ms | 60 ms | 2.31× | yes |
| Q3: Group by status | 35,559 ms | 25 ms | 1422.4× | 100 ms | 87 ms | 3.48× | yes |
| Q4: Region × status breakdown | 13,522 ms | 26 ms | 520.1× | 263 ms | 281 ms | 10.81× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 32 ms | 182.7× | 65 ms | 64 ms | 2.00× | yes |
| Q6: Top 10 spenders | 837,988 ms | 95 ms | 8820.9× | 262 ms | 272 ms | 2.86× | yes |
| Q7: Regional analytics | 57,632 ms | 31 ms | 1859.1× | 167 ms | 221 ms | 7.13× | yes |
| Q8: Join users + orders | 849,244 ms | 26 ms | 32663.2× | 279 ms | 240 ms | 9.23× | yes |
| **Total** | **1,801,821 ms** | **289 ms** | **6234.7×** | **1,226 ms** | **1,254 ms** | **4.34×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 589 ms | 1.8× | 98 ms | 82 ms | 0.14× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,130 ms | 12.5× | 120 ms | 115 ms | 0.10× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 482 ms | 19.1× | 55 ms | 89 ms | 0.18× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,014 ms | 59.2× | 192 ms | 201 ms | 0.20× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 27 MB | 2% | 437 MB | n/a | n/a |
| Q2: Filtered count | 0% | 42 MB | 8% | 458 MB | n/a | n/a |
| Q3: Group by status | 0% | 127 MB | 12% | 457 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 184 MB | 412% | 506 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 488 MB | 3% | 484 MB | n/a | n/a |
| Q6: Top 10 spenders | 38% | 698 MB | 465% | 631 MB | n/a | n/a |
| Q7: Regional analytics | 56% | 817 MB | 26% | 530 MB | n/a | n/a |
| Q8: Join users + orders | 38% | 988 MB | 341% | 708 MB | n/a | n/a |

