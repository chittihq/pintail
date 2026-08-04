# Pintail analytical benchmark results

Measured 2026-08-04T10:52:11.470Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 154 ms | 15.1× | 157 ms | 159 ms | 1.03× | yes |
| Q2: Filtered count | 1,211 ms | 153 ms | 7.9× | 198 ms | 195 ms | 1.27× | yes |
| Q3: Group by status | 63,664 ms | 171 ms | 372.3× | 243 ms | 239 ms | 1.40× | yes |
| Q4: Region × status breakdown | 24,106 ms | 172 ms | 140.2× | 307 ms | 308 ms | 1.79× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 157 ms | 76.8× | 201 ms | 205 ms | 1.31× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 260 ms | 6179.1× | 299 ms | 282 ms | 1.08× | yes |
| Q7: Regional analytics | 117,114 ms | 153 ms | 765.5× | 292 ms | 299 ms | 1.95× | yes |
| Q8: Join users + orders | 1,638,533 ms | 155 ms | 10571.2× | 607 ms | 609 ms | 3.93× | yes |
| **Total** | **3,465,575 ms** | **1,375 ms** | **2520.4×** | **2,304 ms** | **2,296 ms** | **1.67×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 349 ms | 5.1× | 322 ms | 343 ms | 0.98× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 606 ms | 177.0× | 362 ms | 392 ms | 0.65× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 816 ms | 27.8× | 346 ms | 441 ms | 0.54× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,644 ms | 70.6× | 432 ms | 467 ms | 0.28× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 24 MB | 7% | 571 MB | n/a | n/a |
| Q2: Filtered count | 1% | 38 MB | 45% | 606 MB | n/a | n/a |
| Q3: Group by status | 2% | 123 MB | 188% | 657 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 173 MB | 353% | 670 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 3% | 485 MB | 68% | 629 MB | n/a | n/a |
| Q6: Top 10 spenders | 211% | 669 MB | 263% | 763 MB | n/a | n/a |
| Q7: Regional analytics | 349% | 870 MB | 326% | 698 MB | n/a | n/a |
| Q8: Join users + orders | 571% | 996 MB | 276% | 810 MB | n/a | n/a |

