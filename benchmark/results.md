# Pintail analytical benchmark results

Measured 2026-08-04T06:14:20.702Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 193 ms | 12.0× | 173 ms | 190 ms | 0.98× | yes |
| Q2: Filtered count | 1,211 ms | 169 ms | 7.2× | 187 ms | 200 ms | 1.18× | yes |
| Q3: Group by status | 63,664 ms | 162 ms | 393.0× | 240 ms | 236 ms | 1.46× | yes |
| Q4: Region × status breakdown | 24,106 ms | 162 ms | 148.8× | 327 ms | 340 ms | 2.10× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 168 ms | 71.8× | 203 ms | 211 ms | 1.26× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 265 ms | 6062.5× | 290 ms | 299 ms | 1.13× | yes |
| Q7: Regional analytics | 117,114 ms | 185 ms | 633.0× | 308 ms | 313 ms | 1.69× | yes |
| Q8: Join users + orders | 1,638,533 ms | 165 ms | 9930.5× | 640 ms | 595 ms | 3.61× | yes |
| **Total** | **3,465,575 ms** | **1,469 ms** | **2359.1×** | **2,368 ms** | **2,384 ms** | **1.62×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 361 ms | 4.9× | 344 ms | 354 ms | 0.98× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 597 ms | 179.7× | 396 ms | 399 ms | 0.67× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 915 ms | 24.8× | 388 ms | 365 ms | 0.40× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,870 ms | 62.1× | 466 ms | 485 ms | 0.26× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 2% | 38 MB | 9% | 559 MB | n/a | n/a |
| Q2: Filtered count | 2% | 52 MB | 45% | 580 MB | n/a | n/a |
| Q3: Group by status | 2% | 140 MB | 177% | 592 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 191 MB | 346% | 674 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 51% | 496 MB | 102% | 611 MB | n/a | n/a |
| Q6: Top 10 spenders | 140% | 693 MB | 301% | 754 MB | n/a | n/a |
| Q7: Regional analytics | 290% | 900 MB | 281% | 644 MB | n/a | n/a |
| Q8: Join users + orders | 587% | 1,053 MB | 296% | 811 MB | n/a | n/a |

