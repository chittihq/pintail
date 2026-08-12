# Pintail analytical benchmark results

Measured 2026-08-12T13:24:53.875Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,434 ms | 10 ms | 143.4× | 8 ms | 10 ms | 1.00× | yes |
| Q2: Filtered count | 593 ms | 8 ms | 74.1× | 29 ms | 30 ms | 3.75× | yes |
| Q3: Group by status | 35,774 ms | 10 ms | 3577.4× | 61 ms | 66 ms | 6.60× | yes |
| Q4: Region × status breakdown | 13,301 ms | 10 ms | 1330.1× | 247 ms | 233 ms | 23.30× | yes |
| Q5: Monthly revenue (2023) | 5,638 ms | 10 ms | 563.8× | 41 ms | 47 ms | 4.70× | yes |
| Q6: Top 10 spenders | 814,794 ms | 69 ms | 11808.6× | 270 ms | 238 ms | 3.45× | yes |
| Q7: Regional analytics | 56,956 ms | 10 ms | 5695.6× | 140 ms | 165 ms | 16.50× | yes |
| Q8: Join users + orders | 826,572 ms | 9 ms | 91841.3× | 188 ms | 190 ms | 21.11× | yes |
| **Total** | **1,755,062 ms** | **136 ms** | **12904.9×** | **984 ms** | **979 ms** | **7.20×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,081 ms | 615 ms | 1.8× | 53 ms | 49 ms | 0.08× | yes |
| N2: Group by region (novel group column) | 13,995 ms | 1,165 ms | 12.0× | 87 ms | 84 ms | 0.07× | yes |
| N3: Monthly revenue, novel year | 9,160 ms | 481 ms | 19.0× | 37 ms | 46 ms | 0.10× | yes |
| N4: Regional analytics, novel range | 58,019 ms | 1,019 ms | 56.9× | 156 ms | 172 ms | 0.17× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 38 MB | 2% | 483 MB | 100% | 1,541 MB |
| Q2: Filtered count | 0% | 52 MB | 3% | 505 MB | 0% | 1,540 MB |
| Q3: Group by status | 0% | 142 MB | 7% | 543 MB | 83% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 199 MB | 336% | 597 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 500 MB | 2% | 556 MB | 110% | 1,552 MB |
| Q6: Top 10 spenders | 39% | 706 MB | 429% | 760 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 45% | 869 MB | 3% | 659 MB | 60% | 1,553 MB |
| Q8: Join users + orders | 16% | 982 MB | 139% | 849 MB | 14% | 1,706 MB |

