# Pintail analytical benchmark results

Measured 2026-08-07T17:30:34.497Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 14 ms | 105.1× | 12 ms | 12 ms | 0.86× | yes |
| Q2: Filtered count | 597 ms | 13 ms | 45.9× | 31 ms | 31 ms | 2.38× | yes |
| Q3: Group by status | 34,657 ms | 11 ms | 3150.6× | 63 ms | 62 ms | 5.64× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 173 ms | 175 ms | 14.58× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 13 ms | 442.0× | 41 ms | 46 ms | 3.54× | yes |
| Q6: Top 10 spenders | 874,366 ms | 77 ms | 11355.4× | 172 ms | 171 ms | 2.22× | yes |
| Q7: Regional analytics | 53,420 ms | 12 ms | 4451.7× | 117 ms | 185 ms | 15.42× | yes |
| Q8: Join users + orders | 890,711 ms | 13 ms | 68516.2× | 179 ms | 178 ms | 13.69× | yes |
| **Total** | **1,874,417 ms** | **165 ms** | **11360.1×** | **788 ms** | **860 ms** | **5.21×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 126 ms | 6.9× | 36 ms | 40 ms | 0.32× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 347 ms | 139.2× | 71 ms | 74 ms | 0.21× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 521 ms | 15.6× | 47 ms | 119 ms | 0.23× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 936 ms | 58.4× | 125 ms | 144 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 18 MB | 3% | 434 MB | n/a | n/a |
| Q2: Filtered count | 0% | 33 MB | 2% | 460 MB | n/a | n/a |
| Q3: Group by status | 0% | 123 MB | 2% | 476 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 165 MB | 55% | 523 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 481 MB | 7% | 491 MB | n/a | n/a |
| Q6: Top 10 spenders | 28% | 706 MB | 66% | 624 MB | n/a | n/a |
| Q7: Regional analytics | 38% | 895 MB | 3% | 537 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 1,001 MB | 96% | 706 MB | n/a | n/a |

