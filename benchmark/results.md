# Pintail analytical benchmark results

Measured 2026-08-10T07:54:06.765Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-08T07:55:28.496Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 13 ms | 109.4× | 14 ms | 15 ms | 1.15× | yes |
| Q2: Filtered count | 607 ms | 12 ms | 50.6× | 35 ms | 31 ms | 2.58× | yes |
| Q3: Group by status | 35,559 ms | 12 ms | 2963.3× | 76 ms | 71 ms | 5.92× | yes |
| Q4: Region × status breakdown | 13,522 ms | 13 ms | 1040.2× | 240 ms | 242 ms | 18.62× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 13 ms | 449.8× | 41 ms | 46 ms | 3.54× | yes |
| Q6: Top 10 spenders | 837,988 ms | 75 ms | 11173.2× | 246 ms | 259 ms | 3.45× | yes |
| Q7: Regional analytics | 57,632 ms | 15 ms | 3842.1× | 145 ms | 164 ms | 10.93× | yes |
| Q8: Join users + orders | 849,244 ms | 15 ms | 56616.3× | 222 ms | 208 ms | 13.87× | yes |
| **Total** | **1,801,821 ms** | **168 ms** | **10725.1×** | **1,019 ms** | **1,036 ms** | **6.17×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 634 ms | 1.7× | 61 ms | 62 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,189 ms | 11.9× | 93 ms | 92 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 483 ms | 19.1× | 42 ms | 53 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,061 ms | 56.6× | 182 ms | 176 ms | 0.17× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 27 MB | 2% | 432 MB | n/a | n/a |
| Q2: Filtered count | 0% | 40 MB | 3% | 458 MB | n/a | n/a |
| Q3: Group by status | 0% | 130 MB | 2% | 478 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 188 MB | 345% | 527 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 489 MB | 3% | 481 MB | n/a | n/a |
| Q6: Top 10 spenders | 39% | 683 MB | 354% | 603 MB | n/a | n/a |
| Q7: Regional analytics | 53% | 797 MB | 3% | 531 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 959 MB | 265% | 723 MB | n/a | n/a |

