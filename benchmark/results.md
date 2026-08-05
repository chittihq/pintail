# Pintail analytical benchmark results

Measured 2026-08-05T21:36:15.057Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 168 ms | 13.8× | 166 ms | 174 ms | 1.04× | yes |
| Q2: Filtered count | 1,211 ms | 168 ms | 7.2× | 191 ms | 194 ms | 1.15× | yes |
| Q3: Group by status | 63,664 ms | 167 ms | 381.2× | 254 ms | 237 ms | 1.42× | yes |
| Q4: Region × status breakdown | 24,106 ms | 211 ms | 114.2× | 314 ms | 304 ms | 1.44× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 162 ms | 74.5× | 204 ms | 212 ms | 1.31× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 253 ms | 6350.1× | 720 ms | 279 ms | 1.10× | yes |
| Q7: Regional analytics | 117,114 ms | 161 ms | 727.4× | 276 ms | 298 ms | 1.85× | yes |
| Q8: Join users + orders | 1,638,533 ms | 162 ms | 10114.4× | 1,021 ms | 584 ms | 3.60× | yes |
| **Total** | **3,465,575 ms** | **1,452 ms** | **2386.8×** | **3,146 ms** | **2,282 ms** | **1.57×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 326 ms | 5.4× | 344 ms | 335 ms | 1.03× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 561 ms | 191.2× | 628 ms | 377 ms | 0.67× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 781 ms | 29.0× | 852 ms | 2,255 ms | 2.89× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,578 ms | 73.6× | 930 ms | 495 ms | 0.31× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 28 MB | 6% | 513 MB | n/a | n/a |
| Q2: Filtered count | 2% | 44 MB | 40% | 543 MB | n/a | n/a |
| Q3: Group by status | 2% | 131 MB | 120% | 554 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 183 MB | 343% | 603 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 2% | 487 MB | 101% | 566 MB | n/a | n/a |
| Q6: Top 10 spenders | 151% | 694 MB | 267% | 697 MB | n/a | n/a |
| Q7: Regional analytics | 332% | 897 MB | 261% | 647 MB | n/a | n/a |
| Q8: Join users + orders | 300% | 1,016 MB | 213% | 733 MB | n/a | n/a |

