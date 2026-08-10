# Pintail analytical benchmark results

Measured 2026-08-10T05:44:32.689Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-08T07:55:28.496Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 13 ms | 109.4× | 13 ms | 16 ms | 1.23× | yes |
| Q2: Filtered count | 607 ms | 13 ms | 46.7× | 35 ms | 35 ms | 2.69× | yes |
| Q3: Group by status | 35,559 ms | 29 ms | 1226.2× | 71 ms | 77 ms | 2.66× | yes |
| Q4: Region × status breakdown | 13,522 ms | 13 ms | 1040.2× | 237 ms | 258 ms | 19.85× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 14 ms | 417.6× | 41 ms | 48 ms | 3.43× | yes |
| Q6: Top 10 spenders | 837,988 ms | 72 ms | 11638.7× | 286 ms | 282 ms | 3.92× | yes |
| Q7: Regional analytics | 57,632 ms | 13 ms | 4433.2× | 233 ms | 216 ms | 16.62× | yes |
| Q8: Join users + orders | 849,244 ms | 14 ms | 60660.3× | 200 ms | 198 ms | 14.14× | yes |
| **Total** | **1,801,821 ms** | **181 ms** | **9954.8×** | **1,116 ms** | **1,130 ms** | **6.24×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 635 ms | 1.7× | 70 ms | 57 ms | 0.09× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,168 ms | 12.1× | 90 ms | 96 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 485 ms | 19.0× | 46 ms | 53 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,033 ms | 58.1× | 196 ms | 188 ms | 0.18× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 29 MB | 2% | 490 MB | n/a | n/a |
| Q2: Filtered count | 0% | 41 MB | 7% | 477 MB | n/a | n/a |
| Q3: Group by status | 0% | 128 MB | 5% | 476 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 179 MB | 637% | 542 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 489 MB | 3% | 478 MB | n/a | n/a |
| Q6: Top 10 spenders | 31% | 705 MB | 407% | 620 MB | n/a | n/a |
| Q7: Regional analytics | 25% | 864 MB | 136% | 617 MB | n/a | n/a |
| Q8: Join users + orders | 3% | 956 MB | 198% | 705 MB | n/a | n/a |

