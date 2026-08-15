# commerce-production-v1 — ci profile

Run: 2026-08-15T23:45:11.109Z → 2026-08-15T23:48:11.467Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 231.0 | 355.7 |
| q01-tenant-revenue | pintail | ok | 450.3 | 564.3 |
| q02-customer-history | mysql | ok | 8.9 | 78.2 |
| q02-customer-history | pintail | ok | 634.9 | 767.1 |
| q03-fulfillment-backlog | mysql | ok | 8.3 | 85.0 |
| q03-fulfillment-backlog | pintail | ok | 366.1 | 380.4 |
| q04-inventory-risk | mysql | ok | 9.6 | 16.3 |
| q04-inventory-risk | pintail | ok | 397.7 | 400.0 |
| q05-payment-failures | mysql | ok | 104.1 | 396.0 |
| q05-payment-failures | pintail | ok | 4.5 | 312.4 |
| q06-refund-rate | mysql | ok | 1113.7 | 1175.2 |
| q06-refund-rate | pintail | ok | 1029.4 | 1213.4 |
| q07-product-performance | mysql | ok | 947.7 | 950.5 |
| q07-product-performance | pintail | ok | 1165.2 | 1197.7 |
| q08-regional-cohorts | mysql | ok | 527.7 | 666.4 |
| q08-regional-cohorts | pintail | ok | 774.5 | 816.2 |
| q09-order-lifecycle | mysql | ok | 270.1 | 283.2 |
| q09-order-lifecycle | pintail | ok | 685.2 | 694.4 |
| q10-wide-operational-join | mysql | ok | 503.6 | 674.6 |
| q10-wide-operational-join | pintail | ok | 1153.0 | 1382.3 |
| q11-dormant-customers | mysql | ok | 18.8 | 104.3 |
| q11-dormant-customers | pintail | ok | 992.0 | 1164.9 |
| q12-per-customer-revenue | mysql | ok | 37.0 | 60.0 |
| q12-per-customer-revenue | pintail | ok | 349.1 | 363.2 |
