# commerce-production-v1 — ci profile

Run: 2026-09-10T07:30:22.029Z → 2026-09-10T07:31:14.970Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 49.0 | 296.3 |
| q01-tenant-revenue | pintail | ok | 2.4 | 102.4 |
| q02-customer-history | mysql | ok | 0.7 | 7.1 |
| q02-customer-history | pintail | ok | 3.0 | 313.4 |
| q03-fulfillment-backlog | mysql | ok | 1.0 | 27.1 |
| q03-fulfillment-backlog | pintail | ok | 2.4 | 64.2 |
| q04-inventory-risk | mysql | ok | 0.9 | 2.7 |
| q04-inventory-risk | pintail | ok | 4.4 | 145.9 |
| q05-payment-failures | mysql | ok | 88.1 | 120.7 |
| q05-payment-failures | pintail | ok | 3.0 | 215.6 |
| q06-refund-rate | mysql | ok | 975.9 | 977.0 |
| q06-refund-rate | pintail | ok | 4.5 | 760.3 |
| q07-product-performance | mysql | ok | 921.3 | 932.2 |
| q07-product-performance | pintail | ok | 51.4 | 742.4 |
| q08-regional-cohorts | mysql | ok | 410.6 | 462.0 |
| q08-regional-cohorts | pintail | ok | 558.0 | 595.4 |
| q09-order-lifecycle | mysql | ok | 265.9 | 266.6 |
| q09-order-lifecycle | pintail | ok | 353.3 | 356.4 |
| q10-wide-operational-join | mysql | ok | 437.6 | 567.8 |
| q10-wide-operational-join | pintail | ok | 10.0 | 709.6 |
| q11-dormant-customers | mysql | ok | 5.3 | 21.7 |
| q11-dormant-customers | pintail | ok | 483.3 | 485.1 |
| q12-per-customer-revenue | mysql | ok | 5.7 | 10.1 |
| q12-per-customer-revenue | pintail | ok | 2.8 | 53.9 |
