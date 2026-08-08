# Pintail analytical benchmark results

Measured 2026-08-08T07:55:39.079Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 25 ms | 56.9× | 23 ms | 25 ms | 1.00× | yes |
| Q2: Filtered count | 607 ms | 27 ms | 22.5× | 71 ms | 52 ms | 1.93× | yes |
| Q3: Group by status | 35,559 ms | 24 ms | 1481.6× | 81 ms | 78 ms | 3.25× | yes |
| Q4: Region × status breakdown | 13,522 ms | 25 ms | 540.9× | 281 ms | 246 ms | 9.84× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 24 ms | 243.6× | 58 ms | 62 ms | 2.58× | yes |
| Q6: Top 10 spenders | 837,988 ms | 90 ms | 9311.0× | 265 ms | 268 ms | 2.98× | yes |
| Q7: Regional analytics | 57,632 ms | 24 ms | 2401.3× | 186 ms | 199 ms | 8.29× | yes |
| Q8: Join users + orders | 849,244 ms | 24 ms | 35385.2× | 205 ms | 238 ms | 9.92× | yes |
| **Total** | **1,801,821 ms** | **263 ms** | **6851.0×** | **1,170 ms** | **1,168 ms** | **4.44×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 631 ms | 1.7× | 72 ms | 65 ms | 0.10× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,078 ms | 13.1× | 119 ms | 134 ms | 0.12× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 521 ms | 17.7× | 56 ms | 66 ms | 0.13× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,091 ms | 55.0× | 218 ms | 210 ms | 0.19× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 27 MB | 7% | 604 MB | 77% | 1,540 MB |
| Q2: Filtered count | 0% | 42 MB | 8% | 632 MB | 0% | 1,540 MB |
| Q3: Group by status | 0% | 130 MB | 3% | 644 MB | 83% | 1,540 MB |
| Q4: Region × status breakdown | 0% | 192 MB | 504% | 704 MB | 106% | 1,552 MB |
| Q5: Monthly revenue (2023) | 0% | 489 MB | 3% | 630 MB | 109% | 1,552 MB |
| Q6: Top 10 spenders | 97% | 701 MB | 538% | 764 MB | 12% | 1,552 MB |
| Q7: Regional analytics | 33% | 904 MB | 143% | 663 MB | 61% | 1,552 MB |
| Q8: Join users + orders | 78% | 1,028 MB | 264% | 819 MB | 13% | 1,707 MB |

