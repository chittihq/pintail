# Pintail analytical benchmark results

Measured 2026-08-05T07:42:23.530Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 188 ms | 12.3× | 149 ms | 190 ms | 1.01× | yes |
| Q2: Filtered count | 1,211 ms | 188 ms | 6.4× | 183 ms | 176 ms | 0.94× | yes |
| Q3: Group by status | 63,664 ms | 149 ms | 427.3× | 228 ms | 226 ms | 1.52× | yes |
| Q4: Region × status breakdown | 24,106 ms | 150 ms | 160.7× | 298 ms | 305 ms | 2.03× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 150 ms | 80.4× | 190 ms | 197 ms | 1.31× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 253 ms | 6350.1× | 256 ms | 357 ms | 1.41× | yes |
| Q7: Regional analytics | 117,114 ms | 169 ms | 693.0× | 274 ms | 286 ms | 1.69× | yes |
| Q8: Join users + orders | 1,638,533 ms | 152 ms | 10779.8× | 640 ms | 404 ms | 2.66× | yes |
| **Total** | **3,465,575 ms** | **1,399 ms** | **2477.2×** | **2,218 ms** | **2,141 ms** | **1.53×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 303 ms | 5.8× | 322 ms | 320 ms | 1.06× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 602 ms | 178.2× | 419 ms | 412 ms | 0.68× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 775 ms | 29.2× | 342 ms | 407 ms | 0.53× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,695 ms | 68.5× | 507 ms | 509 ms | 0.30× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 3% | 26 MB | 10% | 597 MB | n/a | n/a |
| Q2: Filtered count | 2% | 40 MB | 65% | 626 MB | n/a | n/a |
| Q3: Group by status | 3% | 128 MB | 123% | 678 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 178 MB | 351% | 703 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 485 MB | 73% | 642 MB | n/a | n/a |
| Q6: Top 10 spenders | 36% | 677 MB | 292% | 777 MB | n/a | n/a |
| Q7: Regional analytics | 184% | 896 MB | 295% | 685 MB | n/a | n/a |
| Q8: Join users + orders | 564% | 1,068 MB | 342% | 844 MB | n/a | n/a |

