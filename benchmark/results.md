# Pintail analytical benchmark results

Measured 2026-08-07T14:33:46.058Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 17 ms | 86.5× | 15 ms | 13 ms | 0.76× | yes |
| Q2: Filtered count | 597 ms | 14 ms | 42.6× | 36 ms | 31 ms | 2.21× | yes |
| Q3: Group by status | 34,657 ms | 13 ms | 2665.9× | 233 ms | 64 ms | 4.92× | yes |
| Q4: Region × status breakdown | 13,449 ms | 15 ms | 896.6× | 204 ms | 181 ms | 12.07× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 13 ms | 442.0× | 43 ms | 43 ms | 3.31× | yes |
| Q6: Top 10 spenders | 874,366 ms | 71 ms | 12315.0× | 172 ms | 174 ms | 2.45× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 154 ms | 133 ms | 10.23× | yes |
| Q8: Join users + orders | 890,711 ms | 19 ms | 46879.5× | 176 ms | 165 ms | 8.68× | yes |
| **Total** | **1,874,417 ms** | **175 ms** | **10711.0×** | **1,033 ms** | **804 ms** | **4.59×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 130 ms | 6.7× | 156 ms | 157 ms | 1.21× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 335 ms | 144.2× | 76 ms | 75 ms | 0.22× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 510 ms | 15.9× | 49 ms | 134 ms | 0.26× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 969 ms | 56.5× | 144 ms | 147 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 35 MB | 2% | 442 MB | n/a | n/a |
| Q2: Filtered count | 0% | 49 MB | 2% | 465 MB | n/a | n/a |
| Q3: Group by status | 0% | 137 MB | 7% | 470 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 194 MB | 245% | 524 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 496 MB | 2% | 481 MB | n/a | n/a |
| Q6: Top 10 spenders | 35% | 706 MB | 58% | 603 MB | n/a | n/a |
| Q7: Regional analytics | 135% | 881 MB | 2% | 568 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 962 MB | 171% | 696 MB | n/a | n/a |

