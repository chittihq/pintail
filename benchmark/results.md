# Pintail analytical benchmark results

Measured 2026-08-10T12:48:21.568Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,504 ms | 12 ms | 125.3× | 13 ms | 15 ms | 1.25× | yes |
| Q2: Filtered count | 735 ms | 17 ms | 43.2× | 37 ms | 33 ms | 1.94× | yes |
| Q3: Group by status | 34,916 ms | 14 ms | 2494.0× | 75 ms | 70 ms | 5.00× | yes |
| Q4: Region × status breakdown | 13,263 ms | 20 ms | 663.1× | 178 ms | 224 ms | 11.20× | yes |
| Q5: Monthly revenue (2023) | 5,893 ms | 14 ms | 420.9× | 50 ms | 49 ms | 3.50× | yes |
| Q6: Top 10 spenders | 860,767 ms | 89 ms | 9671.5× | 176 ms | 188 ms | 2.11× | yes |
| Q7: Regional analytics | 53,293 ms | 15 ms | 3552.9× | 119 ms | 192 ms | 12.80× | yes |
| Q8: Join users + orders | 777,953 ms | 13 ms | 59842.5× | 179 ms | 169 ms | 13.00× | yes |
| **Total** | **1,748,324 ms** | **194 ms** | **9012.0×** | **827 ms** | **940 ms** | **4.85×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,088 ms | 588 ms | 1.9× | 57 ms | 58 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 12,954 ms | 1,098 ms | 11.8× | 99 ms | 93 ms | 0.08× | yes |
| N3: Monthly revenue, novel year | 8,229 ms | 519 ms | 15.9× | 38 ms | 62 ms | 0.12× | yes |
| N4: Regional analytics, novel range | 54,424 ms | 937 ms | 58.1× | 190 ms | 144 ms | 0.15× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 18 MB | 2% | 995 MB | 81% | 1,541 MB |
| Q2: Filtered count | 0% | 33 MB | 2% | 1,002 MB | 0% | 1,540 MB |
| Q3: Group by status | 0% | 121 MB | 2% | 1,037 MB | 84% | 1,541 MB |
| Q4: Region × status breakdown | 0% | 166 MB | 69% | 1,069 MB | 107% | 1,553 MB |
| Q5: Monthly revenue (2023) | 0% | 482 MB | 3% | 1,040 MB | 111% | 1,552 MB |
| Q6: Top 10 spenders | 25% | 653 MB | 32% | 1,185 MB | 13% | 1,553 MB |
| Q7: Regional analytics | 23% | 848 MB | 2% | 1,126 MB | 64% | 1,553 MB |
| Q8: Join users + orders | 3% | 873 MB | 182% | 1,320 MB | 15% | 1,706 MB |

