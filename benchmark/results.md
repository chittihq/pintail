# Pintail analytical benchmark results

Measured 2026-08-05T06:15:18.079Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 153 ms | 15.2× | 229 ms | 156 ms | 1.02× | yes |
| Q2: Filtered count | 1,211 ms | 151 ms | 8.0× | 174 ms | 179 ms | 1.19× | yes |
| Q3: Group by status | 63,664 ms | 149 ms | 427.3× | 224 ms | 236 ms | 1.58× | yes |
| Q4: Region × status breakdown | 24,106 ms | 153 ms | 157.6× | 306 ms | 300 ms | 1.96× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 159 ms | 75.9× | 193 ms | 198 ms | 1.25× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 285 ms | 5637.1× | 268 ms | 256 ms | 0.90× | yes |
| Q7: Regional analytics | 117,114 ms | 164 ms | 714.1× | 291 ms | 290 ms | 1.77× | yes |
| Q8: Join users + orders | 1,638,533 ms | 152 ms | 10779.8× | 579 ms | 539 ms | 3.55× | yes |
| **Total** | **3,465,575 ms** | **1,366 ms** | **2537.0×** | **2,264 ms** | **2,154 ms** | **1.58×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 304 ms | 5.8× | 535 ms | 350 ms | 1.15× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 668 ms | 160.6× | 385 ms | 794 ms | 1.19× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 759 ms | 29.9× | 446 ms | 341 ms | 0.45× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,564 ms | 74.2× | 404 ms | 445 ms | 0.28× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 37 MB | 7% | 573 MB | n/a | n/a |
| Q2: Filtered count | 2% | 54 MB | 83% | 602 MB | n/a | n/a |
| Q3: Group by status | 1% | 142 MB | 173% | 615 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 191 MB | 440% | 665 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 500 MB | 5% | 668 MB | n/a | n/a |
| Q6: Top 10 spenders | 38% | 693 MB | 158% | 755 MB | n/a | n/a |
| Q7: Regional analytics | 202% | 870 MB | 206% | 678 MB | n/a | n/a |
| Q8: Join users + orders | 510% | 979 MB | 330% | 896 MB | n/a | n/a |

