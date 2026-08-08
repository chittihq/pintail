# Pintail analytical benchmark results

Measured 2026-08-08T00:57:41.283Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-07T22:21:31.450Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 12 ms | 122.6× | 10 ms | 12 ms | 1.00× | yes |
| Q2: Filtered count | 597 ms | 13 ms | 45.9× | 30 ms | 32 ms | 2.46× | yes |
| Q3: Group by status | 34,657 ms | 13 ms | 2665.9× | 72 ms | 67 ms | 5.15× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 171 ms | 173 ms | 14.42× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 13 ms | 442.0× | 39 ms | 45 ms | 3.46× | yes |
| Q6: Top 10 spenders | 874,366 ms | 74 ms | 11815.8× | 171 ms | 175 ms | 2.36× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 124 ms | 136 ms | 10.46× | yes |
| Q8: Join users + orders | 890,711 ms | 13 ms | 68516.2× | 176 ms | 165 ms | 12.69× | yes |
| **Total** | **1,874,417 ms** | **163 ms** | **11499.5×** | **793 ms** | **805 ms** | **4.94×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,086 ms | 535 ms | 2.0× | 66 ms | 53 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 10,732 ms | 1,005 ms | 10.7× | 90 ms | 91 ms | 0.09× | yes |
| N3: Monthly revenue, novel year | 5,893 ms | 516 ms | 11.4× | 41 ms | 45 ms | 0.09× | yes |
| N4: Regional analytics, novel range | 52,533 ms | 986 ms | 53.3× | 127 ms | 191 ms | 0.19× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 38 MB | 3% | 501 MB | n/a | n/a |
| Q2: Filtered count | 0% | 51 MB | 2% | 522 MB | n/a | n/a |
| Q3: Group by status | 0% | 141 MB | 2% | 531 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 190 MB | 51% | 583 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 498 MB | 3% | 533 MB | n/a | n/a |
| Q6: Top 10 spenders | 20% | 677 MB | 42% | 656 MB | n/a | n/a |
| Q7: Regional analytics | 2% | 844 MB | 73% | 617 MB | n/a | n/a |
| Q8: Join users + orders | 1% | 913 MB | 49% | 756 MB | n/a | n/a |

