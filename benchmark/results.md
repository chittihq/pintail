# Pintail analytical benchmark results

Measured 2026-08-10T06:52:53.311Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured 2026-08-08T07:55:28.496Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,422 ms | 14 ms | 101.6× | 14 ms | 17 ms | 1.21× | yes |
| Q2: Filtered count | 607 ms | 15 ms | 40.5× | 34 ms | 35 ms | 2.33× | yes |
| Q3: Group by status | 35,559 ms | 13 ms | 2735.3× | 76 ms | 67 ms | 5.15× | yes |
| Q4: Region × status breakdown | 13,522 ms | 15 ms | 901.5× | 241 ms | 259 ms | 17.27× | yes |
| Q5: Monthly revenue (2023) | 5,847 ms | 13 ms | 449.8× | 46 ms | 49 ms | 3.77× | yes |
| Q6: Top 10 spenders | 837,988 ms | 78 ms | 10743.4× | 241 ms | 292 ms | 3.74× | yes |
| Q7: Regional analytics | 57,632 ms | 13 ms | 4433.2× | 146 ms | 189 ms | 14.54× | yes |
| Q8: Join users + orders | 849,244 ms | 12 ms | 70770.3× | 200 ms | 198 ms | 16.50× | yes |
| **Total** | **1,801,821 ms** | **173 ms** | **10415.2×** | **998 ms** | **1,106 ms** | **6.39×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (median of 5 memo-cold variants — raw engine speed)

Each row is the median of five distinct predicate variants, each run once
per engine with no warmup. Pintail therefore cannot replay an exact-result
memo entry. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,082 ms | 625 ms | 1.7× | 64 ms | 58 ms | 0.09× | yes |
| N2: Group by region (novel group column) | 14,113 ms | 1,192 ms | 11.8× | 100 ms | 88 ms | 0.07× | yes |
| N3: Monthly revenue, novel year | 9,220 ms | 523 ms | 17.6× | 42 ms | 54 ms | 0.10× | yes |
| N4: Regional analytics, novel range | 60,058 ms | 1,059 ms | 56.7× | 186 ms | 175 ms | 0.17× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 32 MB | 2% | 445 MB | n/a | n/a |
| Q2: Filtered count | 0% | 48 MB | 4% | 472 MB | n/a | n/a |
| Q3: Group by status | 0% | 136 MB | 2% | 490 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 185 MB | 413% | 538 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 494 MB | 2% | 493 MB | n/a | n/a |
| Q6: Top 10 spenders | 32% | 669 MB | 369% | 621 MB | n/a | n/a |
| Q7: Regional analytics | 82% | 870 MB | 78% | 541 MB | n/a | n/a |
| Q8: Join users + orders | 14% | 900 MB | 178% | 707 MB | n/a | n/a |

