# commerce-production-v1 — ci profile

Run: 2026-08-11T21:29:12.533Z → 2026-08-11T21:37:18.064Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 90.8 | 326.7 |
| q01-tenant-revenue | pintail | ok | 403.6 | 541.1 |
| q02-customer-history | mysql | ok | 9.6 | 148.9 |
| q02-customer-history | pintail | ok | 598.4 | 746.8 |
| q03-fulfillment-backlog | mysql | ok | 10.1 | 46.5 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.9 | 10.5 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 171.0 | 186.2 |
| q05-payment-failures | pintail | ok | 4.8 | 286.6 |
| q06-refund-rate | mysql | ok | 1033.1 | 1038.0 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1077.6 | 1338.4 |
| q07-product-performance | pintail | ok | 1123.1 | 1185.0 |
| q08-regional-cohorts | mysql | ok | 405.1 | 475.8 |
| q08-regional-cohorts | pintail | ok | 733.9 | 756.9 |
| q09-order-lifecycle | mysql | ok | 267.8 | 431.7 |
| q09-order-lifecycle | pintail | ok | 610.9 | 631.1 |
| q10-wide-operational-join | mysql | ok | 533.0 | 736.9 |
| q10-wide-operational-join | pintail | ok | 973.3 | 1453.4 |
