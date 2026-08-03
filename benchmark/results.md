# Pintail analytical benchmark results

Measured 2026-08-03T19:45:15.798Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 164 ms | 14.2× | 163 ms | 176 ms | 1.07× | yes |
| Q2: Filtered count | 1,211 ms | 164 ms | 7.4× | 190 ms | 190 ms | 1.16× | yes |
| Q3: Group by status | 63,664 ms | 164 ms | 388.2× | 241 ms | 707 ms | 4.31× | yes |
| Q4: Region × status breakdown | 24,106 ms | 168 ms | 143.5× | 326 ms | 322 ms | 1.92× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 164 ms | 73.6× | 208 ms | 210 ms | 1.28× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 233 ms | 6895.1× | 294 ms | 293 ms | 1.26× | yes |
| Q7: Regional analytics | 117,114 ms | 614 ms | 190.7× | 367 ms | 319 ms | 0.52× | yes |
| Q8: Join users + orders | 1,638,533 ms | 162 ms | 10114.4× | 592 ms | 620 ms | 3.83× | yes |
| **Total** | **3,465,575 ms** | **1,833 ms** | **1890.7×** | **2,381 ms** | **2,837 ms** | **1.55×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 347 ms | 5.1× | 448 ms | 345 ms | 0.99× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 594 ms | 180.6× | 379 ms | 409 ms | 0.69× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 855 ms | 26.5× | 370 ms | 374 ms | 0.44× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 2,287 ms | 50.8× | 508 ms | 517 ms | 0.23× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 46 MB | 5% | 546 MB | n/a | n/a |
| Q2: Filtered count | 1% | 60 MB | 72% | 577 MB | n/a | n/a |
| Q3: Group by status | 2% | 145 MB | 16% | 628 MB | n/a | n/a |
| Q4: Region × status breakdown | 24% | 200 MB | 251% | 637 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 288% | 505 MB | 93% | 597 MB | n/a | n/a |
| Q6: Top 10 spenders | 24% | 704 MB | 380% | 735 MB | n/a | n/a |
| Q7: Regional analytics | 192% | 899 MB | 179% | 649 MB | n/a | n/a |
| Q8: Join users + orders | 595% | 971 MB | 267% | 848 MB | n/a | n/a |

