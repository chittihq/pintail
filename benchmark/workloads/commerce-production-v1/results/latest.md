# commerce-production-v1 — ci profile

Run: 2026-09-09T13:39:23.012Z → 2026-09-09T13:40:16.909Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 49.1 | 282.9 |
| q01-tenant-revenue | pintail | ok | 3.2 | 93.8 |
| q02-customer-history | mysql | ok | 0.8 | 3.0 |
| q02-customer-history | pintail | ok | 2.3 | 315.4 |
| q03-fulfillment-backlog | mysql | ok | 1.0 | 2.7 |
| q03-fulfillment-backlog | pintail | ok | 2.7 | 58.7 |
| q04-inventory-risk | mysql | ok | 0.8 | 2.6 |
| q04-inventory-risk | pintail | ok | 3.9 | 130.6 |
| q05-payment-failures | mysql | ok | 90.3 | 123.9 |
| q05-payment-failures | pintail | ok | 3.7 | 190.1 |
| q06-refund-rate | mysql | ok | 938.1 | 941.1 |
| q06-refund-rate | pintail | ok | 3.2 | 700.5 |
| q07-product-performance | mysql | ok | 883.8 | 887.3 |
| q07-product-performance | pintail | ok | 50.2 | 736.1 |
| q08-regional-cohorts | mysql | ok | 410.5 | 466.3 |
| q08-regional-cohorts | pintail | ok | 573.3 | 595.0 |
| q09-order-lifecycle | mysql | ok | 263.9 | 265.6 |
| q09-order-lifecycle | pintail | ok | 353.8 | 422.0 |
| q10-wide-operational-join | mysql | ok | 420.5 | 532.2 |
| q10-wide-operational-join | pintail | ok | 7.0 | 709.5 |
| q11-dormant-customers | mysql | ok | 5.4 | 21.7 |
| q11-dormant-customers | pintail | ok | 473.2 | 475.2 |
| q12-per-customer-revenue | mysql | ok | 5.7 | 10.3 |
| q12-per-customer-revenue | pintail | ok | 2.8 | 54.2 |
