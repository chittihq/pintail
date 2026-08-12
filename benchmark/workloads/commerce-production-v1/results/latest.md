# commerce-production-v1 — ci profile

Run: 2026-08-12T13:25:28.619Z → 2026-08-12T13:27:33.512Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 58.6 | 328.2 |
| q01-tenant-revenue | pintail | ok | 417.2 | 521.5 |
| q02-customer-history | mysql | ok | 9.9 | 10.9 |
| q02-customer-history | pintail | ok | 734.6 | 767.1 |
| q03-fulfillment-backlog | mysql | ok | 7.3 | 19.5 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 18.1 | 31.5 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 133.9 | 178.3 |
| q05-payment-failures | pintail | ok | 7.4 | 330.3 |
| q06-refund-rate | mysql | ok | 991.1 | 1339.9 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1027.0 | 1035.8 |
| q07-product-performance | pintail | ok | 1152.3 | 1218.8 |
| q08-regional-cohorts | mysql | ok | 391.1 | 485.5 |
| q08-regional-cohorts | pintail | ok | 763.0 | 804.9 |
| q09-order-lifecycle | mysql | ok | 272.5 | 287.9 |
| q09-order-lifecycle | pintail | ok | 635.6 | 674.8 |
| q10-wide-operational-join | mysql | ok | 525.8 | 662.6 |
| q10-wide-operational-join | pintail | ok | 1302.2 | 1469.2 |
