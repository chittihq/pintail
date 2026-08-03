# Pintail analytical benchmark results

Measured 2026-08-03T16:16:50.247Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 167 ms | 13.9× | 161 ms | 164 ms | 0.98× | yes |
| Q2: Filtered count | 1,211 ms | 212 ms | 5.7× | 188 ms | 186 ms | 0.88× | yes |
| Q3: Group by status | 63,664 ms | 161 ms | 395.4× | 238 ms | 236 ms | 1.47× | yes |
| Q4: Region × status breakdown | 24,106 ms | 163 ms | 147.9× | 317 ms | 312 ms | 1.91× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 161 ms | 74.9× | 205 ms | 207 ms | 1.29× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 237 ms | 6778.7× | 297 ms | 286 ms | 1.21× | yes |
| Q7: Regional analytics | 117,114 ms | 164 ms | 714.1× | 286 ms | 320 ms | 1.95× | yes |
| Q8: Join users + orders | 1,638,533 ms | 163 ms | 10052.3× | 598 ms | 675 ms | 4.14× | yes |
| **Total** | **3,465,575 ms** | **1,428 ms** | **2426.9×** | **2,290 ms** | **2,386 ms** | **1.67×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 334 ms | 5.3× | 414 ms | 340 ms | 1.02× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 568 ms | 188.8× | 402 ms | 405 ms | 0.71× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 861 ms | 26.3× | 355 ms | 422 ms | 0.49× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,646 ms | 70.5× | 676 ms | 503 ms | 0.31× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 32 MB | 5% | 563 MB | n/a | n/a |
| Q2: Filtered count | 1% | 45 MB | 69% | 578 MB | n/a | n/a |
| Q3: Group by status | 2% | 130 MB | 172% | 606 MB | n/a | n/a |
| Q4: Region × status breakdown | 2% | 181 MB | 413% | 636 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 1% | 490 MB | 88% | 609 MB | n/a | n/a |
| Q6: Top 10 spenders | 209% | 699 MB | 336% | 763 MB | n/a | n/a |
| Q7: Regional analytics | 358% | 898 MB | 282% | 668 MB | n/a | n/a |
| Q8: Join users + orders | 564% | 1,029 MB | 224% | 799 MB | n/a | n/a |

