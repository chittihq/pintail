# Pintail analytical benchmark results

Measured 2026-08-06T23:55:09.438Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: cold baseline measured 2026-08-06T23:39:14.299Z.
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 1,471 ms | 71 ms | 20.7× | 43 ms | 13 ms | 0.18× | yes |
| Q2: Filtered count | 597 ms | 191 ms | 3.1× | 28 ms | 31 ms | 0.16× | yes |
| Q3: Group by status | 34,657 ms | 12 ms | 2888.1× | 74 ms | 64 ms | 5.33× | yes |
| Q4: Region × status breakdown | 13,449 ms | 12 ms | 1120.8× | 185 ms | 173 ms | 14.42× | yes |
| Q5: Monthly revenue (2023) | 5,746 ms | 12 ms | 478.8× | 39 ms | 41 ms | 3.42× | yes |
| Q6: Top 10 spenders | 874,366 ms | 76 ms | 11504.8× | 186 ms | 308 ms | 4.05× | yes |
| Q7: Regional analytics | 53,420 ms | 13 ms | 4109.2× | 115 ms | 146 ms | 11.23× | yes |
| Q8: Join users + orders | 890,711 ms | 12 ms | 74225.9× | 160 ms | 159 ms | 13.25× | yes |
| **Total** | **1,874,417 ms** | **399 ms** | **4697.8×** | **830 ms** | **935 ms** | **2.34×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 874 ms | 184 ms | 4.8× | 33 ms | 79 ms | 0.43× | yes |
| N2: Group by region (novel group column) | 48,299 ms | 340 ms | 142.1× | 65 ms | 149 ms | 0.44× | yes |
| N3: Monthly revenue, novel year | 8,104 ms | 521 ms | 15.6× | 46 ms | 178 ms | 0.34× | yes |
| N4: Regional analytics, novel range | 54,702 ms | 1,033 ms | 53.0× | 120 ms | 189 ms | 0.18× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 26 MB | 2% | 425 MB | n/a | n/a |
| Q2: Filtered count | 0% | 38 MB | 7% | 447 MB | n/a | n/a |
| Q3: Group by status | 0% | 127 MB | 7% | 466 MB | n/a | n/a |
| Q4: Region × status breakdown | 0% | 180 MB | 170% | 513 MB | n/a | n/a |
| Q5: Monthly revenue (2023) | 0% | 485 MB | 2% | 458 MB | n/a | n/a |
| Q6: Top 10 spenders | 30% | 699 MB | 200% | 629 MB | n/a | n/a |
| Q7: Regional analytics | 12% | 814 MB | 7% | 526 MB | n/a | n/a |
| Q8: Join users + orders | 0% | 976 MB | 5% | 672 MB | n/a | n/a |

