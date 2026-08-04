# Pintail analytical benchmark results

Measured 2026-08-04T14:11:53.164Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 173 ms | 13.4× | 172 ms | 174 ms | 1.01× | yes |
| Q2: Filtered count | 1,211 ms | 173 ms | 7.0× | 203 ms | 196 ms | 1.13× | yes |
| Q3: Group by status | 63,664 ms | 179 ms | 355.7× | 254 ms | 267 ms | 1.49× | yes |
| Q4: Region × status breakdown | 24,106 ms | 176 ms | 137.0× | 342 ms | 391 ms | 2.22× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 177 ms | 68.2× | 231 ms | 228 ms | 1.29× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 278 ms | 5779.0× | 289 ms | 316 ms | 1.14× | yes |
| Q7: Regional analytics | 117,114 ms | 203 ms | 576.9× | 300 ms | 315 ms | 1.55× | yes |
| Q8: Join users + orders | 1,638,533 ms | 211 ms | 7765.6× | 633 ms | 653 ms | 3.09× | yes |
| **Total** | **3,465,575 ms** | **1,570 ms** | **2207.4×** | **2,424 ms** | **2,540 ms** | **1.62×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 392 ms | 4.5× | 367 ms | 369 ms | 0.94× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 643 ms | 166.8× | 413 ms | 467 ms | 0.73× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 880 ms | 25.8× | 426 ms | 422 ms | 0.48× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,716 ms | 67.7× | 445 ms | 499 ms | 0.29× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 33 MB | 14% | 557 MB | n/a | n/a |
| Q2: Filtered count | 2% | 47 MB | 64% | 580 MB | n/a | n/a |
| Q3: Group by status | 3% | 132 MB | 201% | 600 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 184 MB | 360% | 650 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 65% | 493 MB | 97% | 597 MB | n/a | n/a |
| Q6: Top 10 spenders | 212% | 697 MB | 266% | 735 MB | n/a | n/a |
| Q7: Regional analytics | 345% | 906 MB | 361% | 677 MB | n/a | n/a |
| Q8: Join users + orders | 582% | 979 MB | 336% | 819 MB | n/a | n/a |

