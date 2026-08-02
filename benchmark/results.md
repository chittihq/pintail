# Pintail analytical benchmark results

Measured 2026-08-02T08:41:08.865Z with 20,000,000 orders.

All engines run on the docker host under identical limits (8 CPUs, 8 GB).
Pintail/ClickHouse: median of 5 warm runs. MySQL: single cold run (baseline).
CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing
pintail's always-correct merge-on-read duty; the apples-to-apples reference.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| Q1: Full table count | 2,321 ms | 163 ms | 14.2× | 172 ms | 167 ms | 1.02× | yes |
| Q2: Filtered count | 1,211 ms | 166 ms | 7.3× | 185 ms | 184 ms | 1.11× | yes |
| Q3: Group by status | 63,664 ms | 162 ms | 393.0× | 239 ms | 292 ms | 1.80× | yes |
| Q4: Region × status breakdown | 24,106 ms | 162 ms | 148.8× | 315 ms | 307 ms | 1.90× | yes |
| Q5: Monthly revenue (2023) | 12,063 ms | 164 ms | 73.6× | 201 ms | 207 ms | 1.26× | yes |
| Q6: Top 10 spenders | 1,606,563 ms | 210 ms | 7650.3× | 710 ms | 284 ms | 1.35× | yes |
| Q7: Regional analytics | 117,114 ms | 162 ms | 722.9× | 294 ms | 312 ms | 1.93× | yes |
| Q8: Join users + orders | 1,638,533 ms | 174 ms | 9416.9× | 1,318 ms | 736 ms | 4.23× | yes |
| **Total** | **3,465,575 ms** | **1,363 ms** | **2542.6×** | **3,434 ms** | **2,489 ms** | **1.83×** | |

Release gate: PASS (required ≥50× and exact results).

## Novel queries (cold, single run — raw engine speed)

These queries run exactly once per engine with no warmup, so the
settled aggregate memo cannot serve them: this is what a never-seen
ad-hoc query pays. Excluded from the release-gate totals.

| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |
|---|---:|---:|---:|---:|---:|---:|:--|
| N1: Filtered count, novel constant | 1,764 ms | 357 ms | 4.9× | 385 ms | 413 ms | 1.16× | yes |
| N2: Group by region (novel group column) | 107,253 ms | 3,144 ms | 34.1× | 828 ms | 389 ms | 0.12× | yes |
| N3: Monthly revenue, novel year | 22,665 ms | 835 ms | 27.1× | 507 ms | 363 ms | 0.43× | yes |
| N4: Regional analytics, novel range | 116,111 ms | 2,157 ms | 53.8× | 597 ms | 458 ms | 0.21× | yes |

## Resources during measured runs

Peak container CPU (cumulative across 8 cores, so up to 800%) and peak
memory, sampled via `docker stats` every 250 ms while each engine ran.
MySQL shows n/a when its cold baseline came from the cache.

| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |
|---|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 0% | 34 MB | 8% | 478 MB | 102% | 1,540 MB |
| Q2: Filtered count | 1% | 48 MB | 49% | 502 MB | 40% | 1,540 MB |
| Q3: Group by status | 3% | 203 MB | 237% | 562 MB | 85% | 1,540 MB |
| Q4: Region × status breakdown | 59% | 255 MB | 368% | 602 MB | 103% | 1,552 MB |
| Q5: Monthly revenue (2023) | 2% | 510 MB | 95% | 564 MB | 107% | 1,552 MB |
| Q6: Top 10 spenders | 238% | 661 MB | 111% | 730 MB | 24% | 1,553 MB |
| Q7: Regional analytics | 260% | 865 MB | 250% | 667 MB | 59% | 1,552 MB |
| Q8: Join users + orders | 583% | 1,033 MB | 222% | 795 MB | 25% | 1,708 MB |

