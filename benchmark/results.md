# Pintail analytical benchmark results

Measured 2026-08-07T22:21:41.480Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 14 ms | 105.1× | 11 ms | 12 ms | 0.86× | yes |
| Q2: Filtered count | 597 ms | 12 ms | 49.8× | 31 ms | 33 ms | 2.75× | yes |
| Q3: Group by status | 34,657 ms | 13 ms | 2665.9× | 70 ms | 70 ms | 5.38× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 173 ms | 171 ms | 14.25× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 15 ms | 383.1× | 43 ms | 43 ms | 2.87× | yes |
| Q6: Top 10 spenders | 874,366 ms | 77 ms | 11355.4× | 172 ms | 175 ms | 2.27× | yes |
| Q7: Regional analytics | 53,420 ms | 12 ms | 4451.7× | 119 ms | 215 ms | 17.92× | yes |
| Q8: Join users + orders | 890,711 ms | 12 ms | 74225.9× | 163 ms | 161 ms | 13.42× | yes |
| **Total** | **1,874,417 ms** | **167 ms** | **11224.1×** | **782 ms** | **880 ms** | **5.27×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,086 ms | 525 ms | 2.1× | 75 ms | 51 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 10,732 ms | 1,031 ms | 10.4× | 90 ms | 91 ms | 0.09× | yes |
| N3: Monthly revenue, novel year | 5,893 ms | 426 ms | 13.8× | 41 ms | 44 ms | 0.10× | yes |
| N4: Regional analytics, novel range | 52,533 ms | 1,017 ms | 51.7× | 137 ms | 146 ms | 0.14× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 29 MB | 3% | 525 MB | n/a | n/a |
| Q2: Filtered count | 0% | 45 MB | 2% | 557 MB | n/a | n/a |
| Q3: Group by status | 0% | 129 MB | 4% | 562 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 181 MB | 53% | 601 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 490 MB | 7% | 572 MB | n/a | n/a |
| Q6: Top 10 spenders | 25% | 715 MB | 44% | 691 MB | n/a | n/a |
| Q7: Regional analytics | 0% | 871 MB | 2% | 624 MB | n/a | n/a |
| Q8: Join users + orders | 10% | 913 MB | 59% | 777 MB | n/a | n/a |

