# Pintail analytical benchmark results

Measured 2026-08-02T18:12:18.041Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 173 ms | 13.4× | 172 ms | 198 ms | 1.14× | yes |
| Q2: Filtered count | 1,211 ms | 244 ms | 5.0× | 199 ms | 206 ms | 0.84× | yes |
| Q3: Group by status | 63,664 ms | 321 ms | 198.3× | 249 ms | 241 ms | 0.75× | yes |
| Q4: Region × status breakdown | 24,106 ms | 168 ms | 143.5× | 334 ms | 333 ms | 1.98× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 180 ms | 67.0× | 210 ms | 211 ms | 1.17× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 212 ms | 7578.1× | 298 ms | 315 ms | 1.49× | yes |
| Q7: Regional analytics | 117,114 ms | 172 ms | 680.9× | 296 ms | 313 ms | 1.82× | yes |
| Q8: Join users + orders | 1,638,533 ms | 186 ms | 8809.3× | 611 ms | 617 ms | 3.32× | yes |
| **Total** | **3,465,575 ms** | **1,656 ms** | **2092.7×** | **2,369 ms** | **2,434 ms** | **1.47×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 337 ms | 5.2× | 422 ms | 390 ms | 1.16× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 648 ms | 165.5× | 420 ms | 412 ms | 0.64× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 862 ms | 26.3× | 511 ms | 363 ms | 0.42× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,757 ms | 66.1× | 481 ms | 508 ms | 0.29× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 29 MB | 11% | 529 MB | n/a | n/a |
| Q2: Filtered count | 3% | 43 MB | 62% | 560 MB | n/a | n/a |
| Q3: Group by status | 2% | 129 MB | 247% | 573 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 179 MB | 371% | 627 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 26% | 488 MB | 102% | 574 MB | n/a | n/a |
| Q6: Top 10 spenders | 151% | 664 MB | 362% | 742 MB | n/a | n/a |
| Q7: Regional analytics | 329% | 875 MB | 120% | 636 MB | n/a | n/a |
| Q8: Join users + orders | 560% | 993 MB | 237% | 789 MB | n/a | n/a |

