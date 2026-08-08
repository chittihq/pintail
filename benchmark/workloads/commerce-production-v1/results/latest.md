# commerce-production-v1 — ci profile

Run: 2026-08-08T00:57:46.593Z → 2026-08-08T00:59:54.306Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 56.8 | 296.8 |
| q01-tenant-revenue | pintail | ok | 400.5 | 474.4 |
| q02-customer-history | mysql | ok | 13.6 | 93.0 |
| q02-customer-history | pintail | ok | 611.1 | 816.1 |
| q03-fulfillment-backlog | mysql | ok | 10.4 | 90.8 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 7.7 | 8.8 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 96.6 | 139.8 |
| q05-payment-failures | pintail | ok | 4.2 | 290.8 |
| q06-refund-rate | mysql | ok | 1081.5 | 1088.1 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1180.3 | 1202.0 |
| q07-product-performance | pintail | ok | 1116.2 | 1182.1 |
| q08-regional-cohorts | mysql | ok | 421.1 | 584.7 |
| q08-regional-cohorts | pintail | ok | 725.1 | 754.9 |
| q09-order-lifecycle | mysql | ok | 272.1 | 317.6 |
| q09-order-lifecycle | pintail | ok | 606.3 | 612.5 |
| q10-wide-operational-join | mysql | ok | 480.5 | 896.8 |
| q10-wide-operational-join | pintail | ok | 923.9 | 1265.1 |
