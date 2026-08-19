# commerce-production-v1 — ci profile

Run: 2026-08-19T17:15:39.401Z → 2026-08-19T17:17:37.001Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 105.5 | 316.8 |
| q01-tenant-revenue | pintail | ok | 412.3 | 488.0 |
| q02-customer-history | mysql | ok | 11.0 | 19.7 |
| q02-customer-history | pintail | ok | 696.1 | 758.6 |
| q03-fulfillment-backlog | mysql | ok | 8.8 | 13.2 |
| q03-fulfillment-backlog | pintail | ok | 398.0 | 457.7 |
| q04-inventory-risk | mysql | ok | 9.2 | 66.9 |
| q04-inventory-risk | pintail | ok | 389.1 | 408.0 |
| q05-payment-failures | mysql | ok | 134.8 | 139.2 |
| q05-payment-failures | pintail | ok | 4.8 | 335.0 |
| q06-refund-rate | mysql | ok | 998.3 | 999.5 |
| q06-refund-rate | pintail | ok | 1073.3 | 1137.4 |
| q07-product-performance | mysql | ok | 956.3 | 971.4 |
| q07-product-performance | pintail | ok | 1223.6 | 1229.5 |
| q08-regional-cohorts | mysql | ok | 413.5 | 546.8 |
| q08-regional-cohorts | pintail | ok | 775.7 | 825.1 |
| q09-order-lifecycle | mysql | ok | 266.4 | 271.8 |
| q09-order-lifecycle | pintail | ok | 683.9 | 720.8 |
| q10-wide-operational-join | mysql | ok | 579.7 | 746.1 |
| q10-wide-operational-join | pintail | ok | 898.2 | 1292.0 |
| q11-dormant-customers | mysql | ok | 14.9 | 48.3 |
| q11-dormant-customers | pintail | ok | 960.7 | 1098.2 |
| q12-per-customer-revenue | mysql | ok | 16.7 | 95.1 |
| q12-per-customer-revenue | pintail | ok | 340.5 | 352.1 |
