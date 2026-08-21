# commerce-production-v1 — ci profile

Run: 2026-08-21T19:16:33.201Z → 2026-08-21T19:19:36.917Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 61.6 | 346.4 |
| q01-tenant-revenue | pintail | ok | 623.9 | 639.2 |
| q02-customer-history | mysql | ok | 8.4 | 9.8 |
| q02-customer-history | pintail | ok | 1039.3 | 1187.2 |
| q03-fulfillment-backlog | mysql | ok | 21.4 | 30.9 |
| q03-fulfillment-backlog | pintail | ok | 389.0 | 546.5 |
| q04-inventory-risk | mysql | ok | 9.9 | 10.4 |
| q04-inventory-risk | pintail | ok | 472.3 | 512.5 |
| q05-payment-failures | mysql | ok | 99.7 | 150.6 |
| q05-payment-failures | pintail | ok | 5.5 | 332.1 |
| q06-refund-rate | mysql | ok | 993.2 | 1043.2 |
| q06-refund-rate | pintail | ok | 1423.7 | 1581.5 |
| q07-product-performance | mysql | ok | 1070.3 | 1102.7 |
| q07-product-performance | pintail | ok | 1367.9 | 1634.9 |
| q08-regional-cohorts | mysql | ok | 500.0 | 522.9 |
| q08-regional-cohorts | pintail | ok | 1109.8 | 1159.4 |
| q09-order-lifecycle | mysql | ok | 287.6 | 426.1 |
| q09-order-lifecycle | pintail | ok | 783.3 | 873.9 |
| q10-wide-operational-join | mysql | ok | 499.4 | 795.1 |
| q10-wide-operational-join | pintail | ok | 1942.9 | 2092.1 |
| q11-dormant-customers | mysql | ok | 14.0 | 47.3 |
| q11-dormant-customers | pintail | ok | 1268.6 | 1414.9 |
| q12-per-customer-revenue | mysql | ok | 17.0 | 105.6 |
| q12-per-customer-revenue | pintail | ok | 409.9 | 449.8 |
