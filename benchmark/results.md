# Pintail analytical benchmark results

Measured 2026-08-07T04:48:43.448Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 12 ms | 122.6× | 10 ms | 12 ms | 1.00× | yes |
| Q2: Filtered count | 597 ms | 12 ms | 49.8× | 30 ms | 39 ms | 3.25× | yes |
| Q3: Group by status | 34,657 ms | 12 ms | 2888.1× | 119 ms | 63 ms | 5.25× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 170 ms | 180 ms | 15.00× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 14 ms | 410.4× | 45 ms | 42 ms | 3.00× | yes |
| Q6: Top 10 spenders | 874,366 ms | 203 ms | 4307.2× | 176 ms | 179 ms | 0.88× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 120 ms | 145 ms | 11.15× | yes |
| Q8: Join users + orders | 890,711 ms | 99 ms | 8997.1× | 156 ms | 168 ms | 1.70× | yes |
| **Total** | **1,874,417 ms** | **377 ms** | **4971.9×** | **826 ms** | **828 ms** | **2.20×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 130 ms | 6.7× | 31 ms | 35 ms | 0.27× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 376 ms | 128.5× | 69 ms | 87 ms | 0.23× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 461 ms | 17.6× | 69 ms | 52 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 1,007 ms | 54.3× | 160 ms | 147 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 21 MB | 2% | 498 MB | n/a | n/a |
| Q2: Filtered count | 0% | 35 MB | 2% | 539 MB | n/a | n/a |
| Q3: Group by status | 0% | 122 MB | 85% | 535 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 174 MB | 66% | 587 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 482 MB | 2% | 531 MB | n/a | n/a |
| Q6: Top 10 spenders | 19% | 676 MB | 50% | 698 MB | n/a | n/a |
| Q7: Regional analytics | 137% | 831 MB | 4% | 616 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 942 MB | 3% | 728 MB | n/a | n/a |

