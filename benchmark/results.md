# Pintail analytical benchmark results

Measured 2026-08-06T23:39:20.249Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 10 ms | 147.1× | 10 ms | 12 ms | 1.20× | yes |
| Q2: Filtered count | 597 ms | 17 ms | 35.1× | 165 ms | 31 ms | 1.82× | yes |
| Q3: Group by status | 34,657 ms | 11 ms | 3150.6× | 67 ms | 65 ms | 5.91× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 181 ms | 179 ms | 14.92× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 12 ms | 478.8× | 39 ms | 43 ms | 3.58× | yes |
| Q6: Top 10 spenders | 874,366 ms | 77 ms | 11355.4× | 174 ms | 174 ms | 2.26× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 166 ms | 152 ms | 11.69× | yes |
| Q8: Join users + orders | 890,711 ms | 12 ms | 74225.9× | 163 ms | 217 ms | 18.08× | yes |
| **Total** | **1,874,417 ms** | **164 ms** | **11429.4×** | **965 ms** | **873 ms** | **5.32×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 113 ms | 7.7× | 163 ms | 35 ms | 0.31× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 344 ms | 140.4× | 82 ms | 132 ms | 0.38× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 512 ms | 15.8× | 398 ms | 48 ms | 0.09× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 983 ms | 55.6× | 135 ms | 146 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 29 MB | 3% | 423 MB | 99% | 1,541 MB |
| Q2: Filtered count | 0% | 43 MB | 7% | 461 MB | 0% | 1,540 MB |
| Q3: Group by status | 0% | 131 MB | 3% | 486 MB | 85% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 189 MB | 116% | 542 MB | 107% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 492 MB | 2% | 489 MB | 110% | 1,552 MB |
| Q6: Top 10 spenders | 20% | 719 MB | 58% | 707 MB | 14% | 1,553 MB |
| Q7: Regional analytics | 2% | 775 MB | 55% | 662 MB | 64% | 1,553 MB |
| Q8: Join users + orders | 2% | 938 MB | 118% | 835 MB | 14% | 1,705 MB |

