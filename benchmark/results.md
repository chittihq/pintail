# Pintail analytical benchmark results

Measured 2026-08-03T16:59:11.419Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 163 ms | 14.2× | 163 ms | 183 ms | 1.12× | yes |
| Q2: Filtered count | 1,211 ms | 185 ms | 6.5× | 190 ms | 187 ms | 1.01× | yes |
| Q3: Group by status | 63,664 ms | 164 ms | 388.2× | 257 ms | 237 ms | 1.45× | yes |
| Q4: Region × status breakdown | 24,106 ms | 165 ms | 146.1× | 314 ms | 318 ms | 1.93× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 172 ms | 70.1× | 207 ms | 212 ms | 1.23× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 246 ms | 6530.7× | 292 ms | 282 ms | 1.15× | yes |
| Q7: Regional analytics | 117,114 ms | 167 ms | 701.3× | 307 ms | 310 ms | 1.86× | yes |
| Q8: Join users + orders | 1,638,533 ms | 220 ms | 7447.9× | 617 ms | 646 ms | 2.94× | yes |
| **Total** | **3,465,575 ms** | **1,482 ms** | **2338.4×** | **2,347 ms** | **2,375 ms** | **1.60×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 337 ms | 5.2× | 381 ms | 386 ms | 1.15× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 629 ms | 170.5× | 478 ms | 686 ms | 1.09× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 1,336 ms | 17.0× | 869 ms | 468 ms | 0.35× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,635 ms | 71.0× | 445 ms | 534 ms | 0.33× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 29 MB | 5% | 547 MB | n/a | n/a |
| Q2: Filtered count | 2% | 44 MB | 37% | 578 MB | n/a | n/a |
| Q3: Group by status | 2% | 130 MB | 177% | 598 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 181 MB | 396% | 648 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 4% | 487 MB | 72% | 605 MB | n/a | n/a |
| Q6: Top 10 spenders | 167% | 687 MB | 367% | 764 MB | n/a | n/a |
| Q7: Regional analytics | 348% | 873 MB | 299% | 671 MB | n/a | n/a |
| Q8: Join users + orders | 207% | 945 MB | 335% | 812 MB | n/a | n/a |

