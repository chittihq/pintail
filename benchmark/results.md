# Pintail analytical benchmark results

Measured 2026-08-04T05:08:40.835Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 164 ms | 14.2× | 164 ms | 165 ms | 1.01× | yes |
| Q2: Filtered count | 1,211 ms | 162 ms | 7.5× | 188 ms | 187 ms | 1.15× | yes |
| Q3: Group by status | 63,664 ms | 180 ms | 353.7× | 235 ms | 254 ms | 1.41× | yes |
| Q4: Region × status breakdown | 24,106 ms | 162 ms | 148.8× | 304 ms | 305 ms | 1.88× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 174 ms | 69.3× | 205 ms | 207 ms | 1.19× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 276 ms | 5820.9× | 284 ms | 282 ms | 1.02× | yes |
| Q7: Regional analytics | 117,114 ms | 161 ms | 727.4× | 304 ms | 363 ms | 2.25× | yes |
| Q8: Join users + orders | 1,638,533 ms | 163 ms | 10052.3× | 610 ms | 618 ms | 3.79× | yes |
| **Total** | **3,465,575 ms** | **1,442 ms** | **2403.3×** | **2,294 ms** | **2,381 ms** | **1.65×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 333 ms | 5.3× | 340 ms | 424 ms | 1.27× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 582 ms | 184.3× | 457 ms | 389 ms | 0.67× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 899 ms | 25.2× | 355 ms | 446 ms | 0.50× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,692 ms | 68.6× | 908 ms | 609 ms | 0.36× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 19 MB | 6% | 548 MB | n/a | n/a |
| Q2: Filtered count | 0% | 33 MB | 103% | 590 MB | n/a | n/a |
| Q3: Group by status | 3% | 121 MB | 178% | 599 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 177 MB | 334% | 636 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 72% | 478 MB | 65% | 601 MB | n/a | n/a |
| Q6: Top 10 spenders | 201% | 681 MB | 92% | 718 MB | n/a | n/a |
| Q7: Regional analytics | 135% | 870 MB | 193% | 673 MB | n/a | n/a |
| Q8: Join users + orders | 149% | 981 MB | 504% | 816 MB | n/a | n/a |

