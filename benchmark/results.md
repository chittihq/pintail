# Pintail analytical benchmark results

Measured 2026-08-05T14:39:40.373Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 172 ms | 13.5× | 343 ms | 373 ms | 2.17× | yes |
| Q2: Filtered count | 1,211 ms | 170 ms | 7.1× | 356 ms | 342 ms | 2.01× | yes |
| Q3: Group by status | 63,664 ms | 172 ms | 370.1× | 391 ms | 388 ms | 2.26× | yes |
| Q4: Region × status breakdown | 24,106 ms | 174 ms | 138.5× | 523 ms | 527 ms | 3.03× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 158 ms | 76.3× | 351 ms | 354 ms | 2.24× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 251 ms | 6400.6× | 525 ms | 437 ms | 1.74× | yes |
| Q7: Regional analytics | 117,114 ms | 175 ms | 669.2× | 524 ms | 524 ms | 2.99× | yes |
| Q8: Join users + orders | 1,638,533 ms | 179 ms | 9153.8× | 619 ms | 653 ms | 3.65× | yes |
| **Total** | **3,465,575 ms** | **1,451 ms** | **2388.4×** | **3,632 ms** | **3,598 ms** | **2.48×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 324 ms | 5.4× | 345 ms | 441 ms | 1.36× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 582 ms | 184.3× | 370 ms | 381 ms | 0.65× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 773 ms | 29.3× | 354 ms | 436 ms | 0.56× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 2,757 ms | 42.1× | 514 ms | 472 ms | 0.17× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 39 MB | 13% | 533 MB | n/a | n/a |
| Q2: Filtered count | 2% | 52 MB | 26% | 553 MB | n/a | n/a |
| Q3: Group by status | 2% | 137 MB | 183% | 630 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 194 MB | 249% | 623 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 498 MB | 99% | 585 MB | n/a | n/a |
| Q6: Top 10 spenders | 307% | 701 MB | 109% | 681 MB | n/a | n/a |
| Q7: Regional analytics | 345% | 893 MB | 190% | 654 MB | n/a | n/a |
| Q8: Join users + orders | 577% | 1,049 MB | 274% | 783 MB | n/a | n/a |

