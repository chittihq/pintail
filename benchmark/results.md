# Pintail analytical benchmark results

Measured 2026-08-07T13:36:24.074Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 12 ms | 122.6× | 11 ms | 12 ms | 1.00× | yes |
| Q2: Filtered count | 597 ms | 11 ms | 54.3× | 29 ms | 29 ms | 2.64× | yes |
| Q3: Group by status | 34,657 ms | 13 ms | 2665.9× | 78 ms | 62 ms | 4.77× | yes |
| Q4: Region × status breakdown | 13,449 ms | 13 ms | 1034.5× | 191 ms | 172 ms | 13.23× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 11 ms | 522.4× | 44 ms | 43 ms | 3.91× | yes |
| Q6: Top 10 spenders | 874,366 ms | 74 ms | 11815.8× | 174 ms | 173 ms | 2.34× | yes |
| Q7: Regional analytics | 53,420 ms | 14 ms | 3815.7× | 114 ms | 130 ms | 9.29× | yes |
| Q8: Join users + orders | 890,711 ms | 13 ms | 68516.2× | 171 ms | 165 ms | 12.69× | yes |
| **Total** | **1,874,417 ms** | **161 ms** | **11642.3×** | **812 ms** | **786 ms** | **4.88×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 131 ms | 6.7× | 35 ms | 39 ms | 0.30× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 335 ms | 144.2× | 250 ms | 71 ms | 0.21× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 474 ms | 17.1× | 59 ms | 101 ms | 0.21× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 1,226 ms | 44.6× | 125 ms | 141 ms | 0.12× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 17 MB | 2% | 449 MB | n/a | n/a |
| Q2: Filtered count | 0% | 31 MB | 2% | 462 MB | n/a | n/a |
| Q3: Group by status | 0% | 118 MB | 5% | 484 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 181 MB | 24% | 535 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 483 MB | 2% | 492 MB | n/a | n/a |
| Q6: Top 10 spenders | 24% | 689 MB | 58% | 649 MB | n/a | n/a |
| Q7: Regional analytics | 47% | 861 MB | 7% | 566 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 959 MB | 55% | 758 MB | n/a | n/a |

