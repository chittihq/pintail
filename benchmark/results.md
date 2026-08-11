# Pintail analytical benchmark results

Measured 2026-08-11T09:37:14.478Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,416 ms | 8 ms | 177.0× | 9 ms | 12 ms | 1.50× | yes |
| Q2: Filtered count | 593 ms | 8 ms | 74.1× | 26 ms | 29 ms | 3.63× | yes |
| Q3: Group by status | 35,779 ms | 9 ms | 3975.4× | 67 ms | 63 ms | 7.00× | yes |
| Q4: Region × status breakdown | 13,333 ms | 10 ms | 1333.3× | 227 ms | 224 ms | 22.40× | yes |
| Q5: Monthly revenue (2023) | 5,629 ms | 9 ms | 625.4× | 39 ms | 41 ms | 4.56× | yes |
| Q6: Top 10 spenders | 835,734 ms | 68 ms | 12290.2× | 233 ms | 232 ms | 3.41× | yes |
| Q7: Regional analytics | 57,434 ms | 9 ms | 6381.6× | 138 ms | 162 ms | 18.00× | yes |
| Q8: Join users + orders | 839,764 ms | 10 ms | 83976.4× | 190 ms | 190 ms | 19.00× | yes |
| **Total** | **1,789,682 ms** | **131 ms** | **13661.7×** | **929 ms** | **953 ms** | **7.27×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,085 ms | 615 ms | 1.8× | 57 ms | 51 ms | 0.08× | yes |
| N2: Group by region (novel group column) | 14,139 ms | 1,207 ms | 11.7× | 90 ms | 90 ms | 0.07× | yes |
| N3: Monthly revenue, novel year | 9,227 ms | 523 ms | 17.6× | 41 ms | 39 ms | 0.07× | yes |
| N4: Regional analytics, novel range | 58,246 ms | 1,038 ms | 56.1× | 180 ms | 195 ms | 0.19× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 39 MB | 3% | 436 MB | 100% | 1,540 MB |
| Q2: Filtered count | 0% | 53 MB | 8% | 467 MB | 0% | 1,540 MB |
| Q3: Group by status | 0% | 143 MB | 3% | 499 MB | 83% | 1,540 MB |
| Q4: Region × status breakdown | 0% | 192 MB | 344% | 597 MB | 107% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 502 MB | 3% | 509 MB | 109% | 1,552 MB |
| Q6: Top 10 spenders | 34% | 706 MB | 324% | 740 MB | 13% | 1,552 MB |
| Q7: Regional analytics | 23% | 843 MB | 3% | 637 MB | 59% | 1,552 MB |
| Q8: Join users + orders | 58% | 1,006 MB | 198% | 827 MB | 13% | 1,705 MB |

