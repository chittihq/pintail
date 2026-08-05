# Pintail analytical benchmark results

Measured 2026-08-05T15:38:22.254Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 158 ms | 14.7× | 158 ms | 162 ms | 1.03× | yes |
| Q2: Filtered count | 1,211 ms | 159 ms | 7.6× | 182 ms | 181 ms | 1.14× | yes |
| Q3: Group by status | 63,664 ms | 159 ms | 400.4× | 235 ms | 229 ms | 1.44× | yes |
| Q4: Region × status breakdown | 24,106 ms | 159 ms | 151.6× | 309 ms | 318 ms | 2.00× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 159 ms | 75.9× | 201 ms | 199 ms | 1.25× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 249 ms | 6452.1× | 270 ms | 275 ms | 1.10× | yes |
| Q7: Regional analytics | 117,114 ms | 159 ms | 736.6× | 287 ms | 289 ms | 1.82× | yes |
| Q8: Join users + orders | 1,638,533 ms | 161 ms | 10177.2× | 579 ms | 561 ms | 3.48× | yes |
| **Total** | **3,465,575 ms** | **1,363 ms** | **2542.6×** | **2,221 ms** | **2,214 ms** | **1.62×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 361 ms | 4.9× | 331 ms | 392 ms | 1.09× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 541 ms | 198.2× | 433 ms | 379 ms | 0.70× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 789 ms | 28.7× | 355 ms | 426 ms | 0.54× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,568 ms | 74.1× | 456 ms | 519 ms | 0.33× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 39 MB | 13% | 527 MB | n/a | n/a |
| Q2: Filtered count | 2% | 53 MB | 44% | 543 MB | n/a | n/a |
| Q3: Group by status | 2% | 140 MB | 188% | 566 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 193 MB | 354% | 628 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 15% | 500 MB | 65% | 573 MB | n/a | n/a |
| Q6: Top 10 spenders | 277% | 704 MB | 343% | 721 MB | n/a | n/a |
| Q7: Regional analytics | 300% | 833 MB | 304% | 641 MB | n/a | n/a |
| Q8: Join users + orders | 589% | 935 MB | 289% | 793 MB | n/a | n/a |

