# Pintail analytical benchmark results

Measured 2026-08-07T23:34:48.687Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-07T22:21:31.450Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 13 ms | 113.2× | 11 ms | 13 ms | 1.00× | yes |
| Q2: Filtered count | 597 ms | 16 ms | 37.3× | 33 ms | 31 ms | 1.94× | yes |
| Q3: Group by status | 34,657 ms | 12 ms | 2888.1× | 71 ms | 67 ms | 5.58× | yes |
| Q4: Region × status breakdown | 13,449 ms | 13 ms | 1034.5× | 297 ms | 183 ms | 14.08× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 12 ms | 478.8× | 43 ms | 43 ms | 3.58× | yes |
| Q6: Top 10 spenders | 874,366 ms | 75 ms | 11658.2× | 177 ms | 173 ms | 2.31× | yes |
| Q7: Regional analytics | 53,420 ms | 12 ms | 4451.7× | 116 ms | 130 ms | 10.83× | yes |
| Q8: Join users + orders | 890,711 ms | 13 ms | 68516.2× | 163 ms | 181 ms | 13.92× | yes |
| **Total** | **1,874,417 ms** | **166 ms** | **11291.7×** | **911 ms** | **821 ms** | **4.95×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,086 ms | 523 ms | 2.1× | 62 ms | 50 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 10,732 ms | 1,069 ms | 10.0× | 96 ms | 210 ms | 0.20× | yes |
| N3: Monthly revenue, novel year | 5,893 ms | 420 ms | 14.0× | 42 ms | 45 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 52,533 ms | 979 ms | 53.7× | 138 ms | 144 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 25 MB | 3% | 579 MB | n/a | n/a |
| Q2: Filtered count | 0% | 40 MB | 3% | 607 MB | n/a | n/a |
| Q3: Group by status | 0% | 128 MB | 2% | 630 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 174 MB | 258% | 661 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 487 MB | 2% | 614 MB | n/a | n/a |
| Q6: Top 10 spenders | 33% | 687 MB | 129% | 735 MB | n/a | n/a |
| Q7: Regional analytics | 14% | 817 MB | 5% | 667 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 996 MB | 3% | 817 MB | n/a | n/a |

