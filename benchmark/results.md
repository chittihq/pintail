# Pintail analytical benchmark results

Measured 2026-08-05T20:11:57.095Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 170 ms | 13.7× | 167 ms | 171 ms | 1.01× | yes |
| Q2: Filtered count | 1,211 ms | 169 ms | 7.2× | 225 ms | 192 ms | 1.14× | yes |
| Q3: Group by status | 63,664 ms | 170 ms | 374.5× | 248 ms | 244 ms | 1.44× | yes |
| Q4: Region × status breakdown | 24,106 ms | 171 ms | 141.0× | 324 ms | 329 ms | 1.92× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 170 ms | 71.0× | 214 ms | 212 ms | 1.25× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 261 ms | 6155.4× | 294 ms | 289 ms | 1.11× | yes |
| Q7: Regional analytics | 117,114 ms | 171 ms | 684.9× | 294 ms | 317 ms | 1.85× | yes |
| Q8: Join users + orders | 1,638,533 ms | 188 ms | 8715.6× | 650 ms | 605 ms | 3.22× | yes |
| **Total** | **3,465,575 ms** | **1,470 ms** | **2357.5×** | **2,416 ms** | **2,359 ms** | **1.60×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 812 ms | 2.2× | 1,304 ms | 363 ms | 0.45× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 597 ms | 179.7× | 402 ms | 604 ms | 1.01× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 865 ms | 26.2× | 419 ms | 389 ms | 0.45× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,600 ms | 72.6× | 470 ms | 502 ms | 0.31× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 29 MB | 14% | 522 MB | n/a | n/a |
| Q2: Filtered count | 0% | 42 MB | 13% | 547 MB | n/a | n/a |
| Q3: Group by status | 2% | 129 MB | 236% | 571 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 182 MB | 351% | 601 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 2% | 489 MB | 97% | 572 MB | n/a | n/a |
| Q6: Top 10 spenders | 43% | 692 MB | 201% | 694 MB | n/a | n/a |
| Q7: Regional analytics | 179% | 889 MB | 280% | 635 MB | n/a | n/a |
| Q8: Join users + orders | 560% | 1,021 MB | 244% | 787 MB | n/a | n/a |

