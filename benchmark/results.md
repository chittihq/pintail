# Pintail analytical benchmark results

Measured 2026-08-02T18:56:08.694Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 152 ms | 15.3× | 151 ms | 155 ms | 1.02× | yes |
| Q2: Filtered count | 1,211 ms | 153 ms | 7.9× | 178 ms | 198 ms | 1.29× | yes |
| Q3: Group by status | 63,664 ms | 169 ms | 376.7× | 244 ms | 258 ms | 1.53× | yes |
| Q4: Region × status breakdown | 24,106 ms | 167 ms | 144.3× | 355 ms | 307 ms | 1.84× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 168 ms | 71.8× | 210 ms | 212 ms | 1.26× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 220 ms | 7302.6× | 285 ms | 284 ms | 1.29× | yes |
| Q7: Regional analytics | 117,114 ms | 170 ms | 688.9× | 294 ms | 319 ms | 1.88× | yes |
| Q8: Join users + orders | 1,638,533 ms | 173 ms | 9471.3× | 631 ms | 642 ms | 3.71× | yes |
| **Total** | **3,465,575 ms** | **1,372 ms** | **2525.9×** | **2,348 ms** | **2,375 ms** | **1.73×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 343 ms | 5.1× | 1,208 ms | 395 ms | 1.15× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 688 ms | 155.9× | 483 ms | 416 ms | 0.60× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 814 ms | 27.8× | 397 ms | 374 ms | 0.46× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,714 ms | 67.7× | 445 ms | 1,006 ms | 0.59× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 29 MB | 6% | 555 MB | n/a | n/a |
| Q2: Filtered count | 1% | 43 MB | 45% | 580 MB | n/a | n/a |
| Q3: Group by status | 1% | 127 MB | 121% | 604 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 181 MB | 277% | 702 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 488 MB | 155% | 620 MB | n/a | n/a |
| Q6: Top 10 spenders | 16% | 650 MB | 94% | 741 MB | n/a | n/a |
| Q7: Regional analytics | 242% | 837 MB | 319% | 659 MB | n/a | n/a |
| Q8: Join users + orders | 181% | 949 MB | 233% | 823 MB | n/a | n/a |

