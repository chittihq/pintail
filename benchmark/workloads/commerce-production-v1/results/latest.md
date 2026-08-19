# commerce-production-v1 — ci profile

Run: 2026-08-19T07:28:41.939Z → 2026-08-19T07:31:59.791Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 65.2 | 380.3 |
| q01-tenant-revenue | pintail | ok | 455.3 | 543.0 |
| q02-customer-history | mysql | ok | 16.8 | 19.4 |
| q02-customer-history | pintail | ok | 630.7 | 723.5 |
| q03-fulfillment-backlog | mysql | ok | 18.5 | 85.9 |
| q03-fulfillment-backlog | pintail | ok | 378.1 | 442.1 |
| q04-inventory-risk | mysql | ok | 12.2 | 13.9 |
| q04-inventory-risk | pintail | ok | 390.2 | 459.3 |
| q05-payment-failures | mysql | ok | 123.3 | 167.9 |
| q05-payment-failures | pintail | ok | 4.1 | 327.9 |
| q06-refund-rate | mysql | ok | 1020.4 | 1026.4 |
| q06-refund-rate | pintail | ok | 1025.2 | 1268.2 |
| q07-product-performance | mysql | ok | 997.7 | 1032.8 |
| q07-product-performance | pintail | ok | 1155.3 | 1218.1 |
| q08-regional-cohorts | mysql | ok | 431.9 | 481.0 |
| q08-regional-cohorts | pintail | ok | 764.3 | 855.2 |
| q09-order-lifecycle | mysql | ok | 269.8 | 368.6 |
| q09-order-lifecycle | pintail | ok | 677.1 | 714.1 |
| q10-wide-operational-join | mysql | ok | 586.9 | 814.0 |
| q10-wide-operational-join | pintail | ok | 1190.7 | 1507.1 |
| q11-dormant-customers | mysql | ok | 20.5 | 41.5 |
| q11-dormant-customers | pintail | ok | 979.3 | 1121.9 |
| q12-per-customer-revenue | mysql | ok | 18.1 | 21.9 |
| q12-per-customer-revenue | pintail | ok | 328.7 | 347.7 |
