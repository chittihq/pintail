# Pintail analytical benchmark results

Measured 2026-08-07T05:43:53.749Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 13 ms | 113.2× | 10 ms | 12 ms | 0.92× | yes |
| Q2: Filtered count | 597 ms | 13 ms | 45.9× | 33 ms | 28 ms | 2.15× | yes |
| Q3: Group by status | 34,657 ms | 15 ms | 2310.5× | 59 ms | 64 ms | 4.27× | yes |
| Q4: Region × status breakdown | 13,449 ms | 14 ms | 960.6× | 179 ms | 176 ms | 12.57× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 12 ms | 478.8× | 43 ms | 45 ms | 3.75× | yes |
| Q6: Top 10 spenders | 874,366 ms | 72 ms | 12144.0× | 197 ms | 171 ms | 2.38× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 113 ms | 132 ms | 10.15× | yes |
| Q8: Join users + orders | 890,711 ms | 13 ms | 68516.2× | 163 ms | 173 ms | 13.31× | yes |
| **Total** | **1,874,417 ms** | **165 ms** | **11360.1×** | **797 ms** | **801 ms** | **4.85×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 259 ms | 3.4× | 154 ms | 183 ms | 0.71× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 338 ms | 142.9× | 72 ms | 64 ms | 0.19× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 508 ms | 16.0× | 100 ms | 52 ms | 0.10× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 1,360 ms | 40.2× | 120 ms | 286 ms | 0.21× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 33 MB | 2% | 506 MB | n/a | n/a |
| Q2: Filtered count | 0% | 47 MB | 3% | 538 MB | n/a | n/a |
| Q3: Group by status | 0% | 134 MB | 7% | 549 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 192 MB | 241% | 582 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 495 MB | 2% | 590 MB | n/a | n/a |
| Q6: Top 10 spenders | 18% | 708 MB | 340% | 690 MB | n/a | n/a |
| Q7: Regional analytics | 19% | 862 MB | 3% | 595 MB | n/a | n/a |
| Q8: Join users + orders | 2% | 879 MB | 3% | 750 MB | n/a | n/a |

