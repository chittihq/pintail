# Pintail analytical benchmark results

Measured 2026-08-04T21:02:10.768Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 604 ms | 3.8× | 154 ms | 154 ms | 0.25× | yes |
| Q2: Filtered count | 1,211 ms | 153 ms | 7.9× | 174 ms | 176 ms | 1.15× | yes |
| Q3: Group by status | 63,664 ms | 153 ms | 416.1× | 367 ms | 236 ms | 1.54× | yes |
| Q4: Region × status breakdown | 24,106 ms | 152 ms | 158.6× | 300 ms | 303 ms | 1.99× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 152 ms | 79.4× | 195 ms | 195 ms | 1.28× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 246 ms | 6530.7× | 270 ms | 279 ms | 1.13× | yes |
| Q7: Regional analytics | 117,114 ms | 152 ms | 770.5× | 272 ms | 285 ms | 1.88× | yes |
| Q8: Join users + orders | 1,638,533 ms | 153 ms | 10709.4× | 580 ms | 599 ms | 3.92× | yes |
| **Total** | **3,465,575 ms** | **1,765 ms** | **1963.5×** | **2,312 ms** | **2,227 ms** | **1.26×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 297 ms | 5.9× | 321 ms | 361 ms | 1.22× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 565 ms | 189.8× | 357 ms | 416 ms | 0.74× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 765 ms | 29.6× | 336 ms | 510 ms | 0.67× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,477 ms | 78.6× | 411 ms | 435 ms | 0.29× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 36 MB | 6% | 559 MB | n/a | n/a |
| Q2: Filtered count | 2% | 53 MB | 24% | 592 MB | n/a | n/a |
| Q3: Group by status | 2% | 137 MB | 64% | 615 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 191 MB | 229% | 638 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 496 MB | 75% | 626 MB | n/a | n/a |
| Q6: Top 10 spenders | 130% | 690 MB | 362% | 780 MB | n/a | n/a |
| Q7: Regional analytics | 279% | 895 MB | 274% | 671 MB | n/a | n/a |
| Q8: Join users + orders | 289% | 1,007 MB | 352% | 849 MB | n/a | n/a |

