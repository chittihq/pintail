# Pintail analytical benchmark results

Measured 2026-08-03T08:19:56.986Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 217 ms | 10.7× | 186 ms | 170 ms | 0.78× | yes |
| Q2: Filtered count | 1,211 ms | 172 ms | 7.0× | 193 ms | 197 ms | 1.15× | yes |
| Q3: Group by status | 63,664 ms | 167 ms | 381.2× | 247 ms | 248 ms | 1.49× | yes |
| Q4: Region × status breakdown | 24,106 ms | 169 ms | 142.6× | 320 ms | 313 ms | 1.85× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 186 ms | 64.9× | 211 ms | 197 ms | 1.06× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 248 ms | 6478.1× | 288 ms | 305 ms | 1.23× | yes |
| Q7: Regional analytics | 117,114 ms | 168 ms | 697.1× | 312 ms | 351 ms | 2.09× | yes |
| Q8: Join users + orders | 1,638,533 ms | 184 ms | 8905.1× | 638 ms | 611 ms | 3.32× | yes |
| **Total** | **3,465,575 ms** | **1,511 ms** | **2293.6×** | **2,395 ms** | **2,392 ms** | **1.58×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 348 ms | 5.1× | 379 ms | 351 ms | 1.01× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 554 ms | 193.6× | 509 ms | 408 ms | 0.74× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 913 ms | 24.8× | 368 ms | 391 ms | 0.43× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,719 ms | 67.5× | 451 ms | 477 ms | 0.28× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 30 MB | 5% | 543 MB | n/a | n/a |
| Q2: Filtered count | 3% | 44 MB | 52% | 569 MB | n/a | n/a |
| Q3: Group by status | 2% | 131 MB | 169% | 592 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 185 MB | 338% | 662 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 490 MB | 100% | 604 MB | n/a | n/a |
| Q6: Top 10 spenders | 143% | 693 MB | 333% | 722 MB | n/a | n/a |
| Q7: Regional analytics | 347% | 892 MB | 365% | 654 MB | n/a | n/a |
| Q8: Join users + orders | 566% | 951 MB | 312% | 865 MB | n/a | n/a |

