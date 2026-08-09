# Pintail analytical benchmark results

Measured 2026-08-09T17:39:58.990Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-08T07:55:28.496Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 16 ms | 88.9× | 14 ms | 14 ms | 0.88× | yes |
| Q2: Filtered count | 607 ms | 13 ms | 46.7× | 34 ms | 39 ms | 3.00× | yes |
| Q3: Group by status | 35,559 ms | 17 ms | 2091.7× | 77 ms | 69 ms | 4.06× | yes |
| Q4: Region × status breakdown | 13,522 ms | 18 ms | 751.2× | 246 ms | 257 ms | 14.28× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 14 ms | 417.6× | 50 ms | 45 ms | 3.21× | yes |
| Q6: Top 10 spenders | 837,988 ms | 76 ms | 11026.2× | 249 ms | 265 ms | 3.49× | yes |
| Q7: Regional analytics | 57,632 ms | 15 ms | 3842.1× | 148 ms | 169 ms | 11.27× | yes |
| Q8: Join users + orders | 849,244 ms | 13 ms | 65326.5× | 213 ms | 207 ms | 15.92× | yes |
| **Total** | **1,801,821 ms** | **182 ms** | **9900.1×** | **1,031 ms** | **1,065 ms** | **5.85×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 555 ms | 1.9× | 58 ms | 56 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,092 ms | 12.9× | 108 ms | 91 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 488 ms | 18.9× | 41 ms | 54 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,045 ms | 57.5× | 178 ms | 173 ms | 0.17× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 30 MB | 2% | 439 MB | n/a | n/a |
| Q2: Filtered count | 0% | 46 MB | 25% | 517 MB | n/a | n/a |
| Q3: Group by status | 0% | 134 MB | 4% | 474 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 182 MB | 338% | 528 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 493 MB | 4% | 487 MB | n/a | n/a |
| Q6: Top 10 spenders | 33% | 684 MB | 363% | 619 MB | n/a | n/a |
| Q7: Regional analytics | 49% | 886 MB | 7% | 542 MB | n/a | n/a |
| Q8: Join users + orders | 3% | 1,004 MB | 205% | 722 MB | n/a | n/a |

