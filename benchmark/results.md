# Pintail analytical benchmark results

Measured 2026-08-02T13:24:49.563Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 160 ms | 14.5× | 159 ms | 164 ms | 1.02× | yes |
| Q2: Filtered count | 1,211 ms | 163 ms | 7.4× | 189 ms | 189 ms | 1.16× | yes |
| Q3: Group by status | 63,664 ms | 605 ms | 105.2× | 243 ms | 241 ms | 0.40× | yes |
| Q4: Region × status breakdown | 24,106 ms | 161 ms | 149.7× | 460 ms | 313 ms | 1.94× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 161 ms | 74.9× | 284 ms | 209 ms | 1.30× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 210 ms | 7650.3× | 290 ms | 298 ms | 1.42× | yes |
| Q7: Regional analytics | 117,114 ms | 165 ms | 709.8× | 289 ms | 305 ms | 1.85× | yes |
| Q8: Join users + orders | 1,638,533 ms | 162 ms | 10114.4× | 606 ms | 630 ms | 3.89× | yes |
| **Total** | **3,465,575 ms** | **1,787 ms** | **1939.3×** | **2,520 ms** | **2,349 ms** | **1.31×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 345 ms | 5.1× | 403 ms | 399 ms | 1.16× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 816 ms | 131.4× | 435 ms | 384 ms | 0.47× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 784 ms | 28.9× | 357 ms | 415 ms | 0.53× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,530 ms | 75.9× | 616 ms | 458 ms | 0.30× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 16 MB | 14% | 546 MB | n/a | n/a |
| Q2: Filtered count | 0% | 31 MB | 67% | 574 MB | n/a | n/a |
| Q3: Group by status | 2% | 188 MB | 169% | 651 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 236 MB | 280% | 625 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 2% | 478 MB | 65% | 598 MB | n/a | n/a |
| Q6: Top 10 spenders | 11% | 621 MB | 192% | 691 MB | n/a | n/a |
| Q7: Regional analytics | 3% | 795 MB | 281% | 660 MB | n/a | n/a |
| Q8: Join users + orders | 566% | 951 MB | 357% | 819 MB | n/a | n/a |

