# Pintail analytical benchmark results

Measured 2026-08-10T16:26:37.474Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-10T12:48:11.332Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,504 ms | 18 ms | 83.6× | 12 ms | 15 ms | 0.83× | yes |
| Q2: Filtered count | 735 ms | 12 ms | 61.3× | 33 ms | 40 ms | 3.33× | yes |
| Q3: Group by status | 34,916 ms | 12 ms | 2909.7× | 67 ms | 68 ms | 5.67× | yes |
| Q4: Region × status breakdown | 13,263 ms | 15 ms | 884.2× | 176 ms | 176 ms | 11.73× | yes |
| Q5: Monthly revenue (2023) | 5,893 ms | 16 ms | 368.3× | 47 ms | 59 ms | 3.69× | yes |
| Q6: Top 10 spenders | 860,767 ms | 74 ms | 11632.0× | 179 ms | 195 ms | 2.64× | yes |
| Q7: Regional analytics | 53,293 ms | 12 ms | 4441.1× | 139 ms | 144 ms | 12.00× | yes |
| Q8: Join users + orders | 777,953 ms | 16 ms | 48622.1× | 242 ms | 174 ms | 10.88× | yes |
| **Total** | **1,748,324 ms** | **175 ms** | **9990.4×** | **895 ms** | **871 ms** | **4.98×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,088 ms | 576 ms | 1.9× | 68 ms | 63 ms | 0.11× | yes |
| N2: Group by region (novel group column) | 12,954 ms | 1,241 ms | 10.4× | 102 ms | 102 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 8,229 ms | 441 ms | 18.7× | 55 ms | 49 ms | 0.11× | yes |
| N4: Regional analytics, novel range | 54,424 ms | 936 ms | 58.1× | 147 ms | 150 ms | 0.16× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 28 MB | 2% | 436 MB | n/a | n/a |
| Q2: Filtered count | 0% | 42 MB | 2% | 462 MB | n/a | n/a |
| Q3: Group by status | 0% | 131 MB | 2% | 485 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 181 MB | 2% | 535 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 490 MB | 84% | 481 MB | n/a | n/a |
| Q6: Top 10 spenders | 17% | 700 MB | 195% | 625 MB | n/a | n/a |
| Q7: Regional analytics | 61% | 890 MB | 3% | 543 MB | n/a | n/a |
| Q8: Join users + orders | 1% | 1,008 MB | 224% | 698 MB | n/a | n/a |

