# Pintail analytical benchmark results

Measured 2026-08-03T07:33:02.820Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 170 ms | 13.7× | 167 ms | 200 ms | 1.18× | yes |
| Q2: Filtered count | 1,211 ms | 167 ms | 7.3× | 203 ms | 194 ms | 1.16× | yes |
| Q3: Group by status | 63,664 ms | 169 ms | 376.7× | 251 ms | 265 ms | 1.57× | yes |
| Q4: Region × status breakdown | 24,106 ms | 175 ms | 137.7× | 369 ms | 435 ms | 2.49× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 167 ms | 72.2× | 200 ms | 205 ms | 1.23× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 231 ms | 6954.8× | 265 ms | 280 ms | 1.21× | yes |
| Q7: Regional analytics | 117,114 ms | 152 ms | 770.5× | 280 ms | 318 ms | 2.09× | yes |
| Q8: Join users + orders | 1,638,533 ms | 153 ms | 10709.4× | 584 ms | 593 ms | 3.88× | yes |
| **Total** | **3,465,575 ms** | **1,384 ms** | **2504.0×** | **2,319 ms** | **2,490 ms** | **1.80×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 352 ms | 5.0× | 323 ms | 314 ms | 0.89× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 2,373 ms | 45.2× | 367 ms | 357 ms | 0.15× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 847 ms | 26.8× | 372 ms | 395 ms | 0.47× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 4,183 ms | 27.8× | 490 ms | 491 ms | 0.12× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 31 MB | 6% | 519 MB | n/a | n/a |
| Q2: Filtered count | 1% | 44 MB | 44% | 550 MB | n/a | n/a |
| Q3: Group by status | 386% | 75 MB | 190% | 567 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 158 MB | 341% | 628 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 490 MB | 78% | 581 MB | n/a | n/a |
| Q6: Top 10 spenders | 145% | 669 MB | 353% | 724 MB | n/a | n/a |
| Q7: Regional analytics | 289% | 671 MB | 265% | 652 MB | n/a | n/a |
| Q8: Join users + orders | 585% | 833 MB | 325% | 822 MB | n/a | n/a |

