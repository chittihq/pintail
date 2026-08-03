# Pintail analytical benchmark results

Measured 2026-08-03T14:08:33.501Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-02T08:41:02.244Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 166 ms | 14.0× | 169 ms | 204 ms | 1.23× | yes |
| Q2: Filtered count | 1,211 ms | 162 ms | 7.5× | 189 ms | 192 ms | 1.19× | yes |
| Q3: Group by status | 63,664 ms | 162 ms | 393.0× | 252 ms | 247 ms | 1.52× | yes |
| Q4: Region × status breakdown | 24,106 ms | 165 ms | 146.1× | 316 ms | 307 ms | 1.86× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 164 ms | 73.6× | 207 ms | 209 ms | 1.27× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 263 ms | 6108.6× | 304 ms | 290 ms | 1.10× | yes |
| Q7: Regional analytics | 117,114 ms | 169 ms | 693.0× | 288 ms | 528 ms | 3.12× | yes |
| Q8: Join users + orders | 1,638,533 ms | 163 ms | 10052.3× | 633 ms | 678 ms | 4.16× | yes |
| **Total** | **3,465,575 ms** | **1,414 ms** | **2450.9×** | **2,358 ms** | **2,655 ms** | **1.88×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 343 ms | 5.1× | 347 ms | 575 ms | 1.68× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 628 ms | 170.8× | 926 ms | 404 ms | 0.64× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 855 ms | 26.5× | 809 ms | 367 ms | 0.43× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 1,620 ms | 71.7× | 516 ms | 578 ms | 0.36× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1% | 29 MB | 6% | 517 MB | n/a | n/a |
| Q2: Filtered count | 1% | 42 MB | 44% | 553 MB | n/a | n/a |
| Q3: Group by status | 3% | 130 MB | 192% | 561 MB | n/a | n/a |
| Q4: Region × status breakdown | 3% | 185 MB | 357% | 617 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 1% | 489 MB | 38% | 566 MB | n/a | n/a |
| Q6: Top 10 spenders | 189% | 675 MB | 217% | 679 MB | n/a | n/a |
| Q7: Regional analytics | 261% | 888 MB | 246% | 643 MB | n/a | n/a |
| Q8: Join users + orders | 223% | 1,035 MB | 252% | 788 MB | n/a | n/a |

