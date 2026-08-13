# commerce-production-v1 — ci profile

Run: 2026-08-13T06:29:42.036Z → 2026-08-13T06:32:47.543Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 63.9 | 483.5 |
| q01-tenant-revenue | pintail | ok | 438.1 | 543.3 |
| q02-customer-history | mysql | ok | 10.8 | 143.6 |
| q02-customer-history | pintail | ok | 828.9 | 877.8 |
| q03-fulfillment-backlog | mysql | ok | 17.5 | 58.3 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 20.9 | 36.1 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 120.3 | 172.2 |
| q05-payment-failures | pintail | ok | 5.3 | 322.0 |
| q06-refund-rate | mysql | ok | 1028.4 | 1035.4 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 997.1 | 1034.4 |
| q07-product-performance | pintail | ok | 1232.0 | 1289.4 |
| q08-regional-cohorts | mysql | ok | 425.3 | 456.4 |
| q08-regional-cohorts | pintail | ok | 778.7 | 806.2 |
| q09-order-lifecycle | mysql | ok | 265.2 | 267.3 |
| q09-order-lifecycle | pintail | ok | 683.2 | 704.6 |
| q10-wide-operational-join | mysql | ok | 553.4 | 861.3 |
| q10-wide-operational-join | pintail | ok | 1356.2 | 1568.8 |
| q11-dormant-customers | mysql | ok | 15.7 | 34.3 |
| q11-dormant-customers | pintail | ok | 980.2 | 1155.9 |
| q12-per-customer-revenue | mysql | ok | 105.1 | 660.8 |
| q12-per-customer-revenue | pintail | ok | 344.3 | 366.2 |
