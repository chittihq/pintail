# Pintail analytical benchmark results

Measured 2026-08-04T08:35:12.435Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 179 ms | 13.0× | 162 ms | 184 ms | 1.03× | yes |
| Q2: Filtered count | 1,211 ms | 164 ms | 7.4× | 238 ms | 186 ms | 1.13× | yes |
| Q3: Group by status | 63,664 ms | 168 ms | 379.0× | 253 ms | 286 ms | 1.70× | yes |
| Q4: Region × status breakdown | 24,106 ms | 163 ms | 147.9× | 322 ms | 384 ms | 2.36× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 163 ms | 74.0× | 218 ms | 208 ms | 1.28× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 254 ms | 6325.1× | 285 ms | 313 ms | 1.23× | yes |
| Q7: Regional analytics | 117,114 ms | 618 ms | 189.5× | 292 ms | 305 ms | 0.49× | yes |
| Q8: Join users + orders | 1,638,533 ms | 163 ms | 10052.3× | 1,026 ms | 604 ms | 3.71× | yes |
| **Total** | **3,465,575 ms** | **1,872 ms** | **1851.3×** | **2,796 ms** | **2,470 ms** | **1.32×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 328 ms | 5.4× | 343 ms | 719 ms | 2.19× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 600 ms | 178.8× | 556 ms | 396 ms | 0.66× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 864 ms | 26.2× | 358 ms | 360 ms | 0.42× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,664 ms | 69.8× | 774 ms | 617 ms | 0.37× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 31 MB | 14% | 583 MB | n/a | n/a |
| Q2: Filtered count | 3% | 45 MB | 24% | 610 MB | n/a | n/a |
| Q3: Group by status | 1% | 134 MB | 274% | 680 MB | n/a | n/a |
| Q4: Region × status breakdown | 7% | 194 MB | 357% | 677 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 326% | 493 MB | 100% | 636 MB | n/a | n/a |
| Q6: Top 10 spenders | 33% | 696 MB | 236% | 762 MB | n/a | n/a |
| Q7: Regional analytics | 205% | 917 MB | 324% | 678 MB | n/a | n/a |
| Q8: Join users + orders | 577% | 1,028 MB | 228% | 776 MB | n/a | n/a |

