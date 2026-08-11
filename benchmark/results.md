# Pintail analytical benchmark results

Measured 2026-08-11T21:29:00.927Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,612 ms | 28 ms | 57.6× | 11 ms | 16 ms | 0.57× | yes |
| Q2: Filtered count | 592 ms | 12 ms | 49.3× | 34 ms | 35 ms | 2.92× | yes |
| Q3: Group by status | 34,510 ms | 28 ms | 1232.5× | 69 ms | 65 ms | 2.32× | yes |
| Q4: Region × status breakdown | 13,245 ms | 13 ms | 1018.8× | 208 ms | 181 ms | 13.92× | yes |
| Q5: Monthly revenue (2023) | 5,627 ms | 46 ms | 122.3× | 44 ms | 56 ms | 1.22× | yes |
| Q6: Top 10 spenders | 877,405 ms | 81 ms | 10832.2× | 258 ms | 181 ms | 2.23× | yes |
| Q7: Regional analytics | 53,044 ms | 13 ms | 4080.3× | 131 ms | 139 ms | 10.69× | yes |
| Q8: Join users + orders | 783,831 ms | 16 ms | 48989.4× | 173 ms | 176 ms | 11.00× | yes |
| **Total** | **1,769,866 ms** | **237 ms** | **7467.8×** | **928 ms** | **849 ms** | **3.58×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,089 ms | 576 ms | 1.9× | 59 ms | 67 ms | 0.12× | yes |
| N2: Group by region (novel group column) | 13,026 ms | 1,148 ms | 11.3× | 97 ms | 89 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 8,156 ms | 416 ms | 19.6× | 63 ms | 95 ms | 0.23× | yes |
| N4: Regional analytics, novel range | 54,146 ms | 969 ms | 55.9× | 136 ms | 140 ms | 0.14× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 21 MB | 8% | 500 MB | 101% | 1,541 MB |
| Q2: Filtered count | 0% | 36 MB | 10% | 516 MB | 1% | 1,540 MB |
| Q3: Group by status | 0% | 124 MB | 8% | 565 MB | 84% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 184 MB | 345% | 609 MB | 106% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 484 MB | 9% | 559 MB | 110% | 1,552 MB |
| Q6: Top 10 spenders | 37% | 688 MB | 336% | 718 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 0% | 842 MB | 106% | 656 MB | 63% | 1,552 MB |
| Q8: Join users + orders | 5% | 965 MB | 63% | 836 MB | 16% | 1,706 MB |

