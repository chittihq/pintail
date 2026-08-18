# commerce-production-v1 — ci profile

Run: 2026-08-18T17:59:03.904Z → 2026-08-18T18:01:47.820Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 59.3 | 305.7 |
| q01-tenant-revenue | pintail | ok | 451.4 | 519.8 |
| q02-customer-history | mysql | ok | 12.5 | 19.2 |
| q02-customer-history | pintail | ok | 650.8 | 807.2 |
| q03-fulfillment-backlog | mysql | ok | 8.5 | 13.1 |
| q03-fulfillment-backlog | pintail | ok | 360.8 | 372.2 |
| q04-inventory-risk | mysql | ok | 8.8 | 10.6 |
| q04-inventory-risk | pintail | ok | 411.8 | 459.0 |
| q05-payment-failures | mysql | ok | 142.6 | 496.5 |
| q05-payment-failures | pintail | ok | 4.2 | 328.9 |
| q06-refund-rate | mysql | ok | 1109.4 | 1145.5 |
| q06-refund-rate | pintail | ok | 1133.0 | 1390.8 |
| q07-product-performance | mysql | ok | 1072.8 | 1206.7 |
| q07-product-performance | pintail | ok | 1183.9 | 1264.6 |
| q08-regional-cohorts | mysql | ok | 415.5 | 505.7 |
| q08-regional-cohorts | pintail | ok | 811.8 | 848.8 |
| q09-order-lifecycle | mysql | ok | 266.7 | 523.7 |
| q09-order-lifecycle | pintail | ok | 650.7 | 693.4 |
| q10-wide-operational-join | mysql | ok | 630.2 | 818.1 |
| q10-wide-operational-join | pintail | ok | 1145.2 | 1375.1 |
| q11-dormant-customers | mysql | ok | 23.9 | 38.8 |
| q11-dormant-customers | pintail | ok | 1009.1 | 1196.6 |
| q12-per-customer-revenue | mysql | ok | 41.0 | 97.0 |
| q12-per-customer-revenue | pintail | ok | 373.9 | 374.1 |
