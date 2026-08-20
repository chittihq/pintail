# commerce-production-v1 — ci profile

Run: 2026-08-20T02:59:12.957Z → 2026-08-20T03:02:37.671Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 115.2 | 305.0 |
| q01-tenant-revenue | pintail | ok | 439.3 | 559.8 |
| q02-customer-history | mysql | ok | 8.2 | 79.3 |
| q02-customer-history | pintail | ok | 679.9 | 764.7 |
| q03-fulfillment-backlog | mysql | ok | 14.7 | 17.7 |
| q03-fulfillment-backlog | pintail | ok | 416.3 | 420.2 |
| q04-inventory-risk | mysql | ok | 8.9 | 10.2 |
| q04-inventory-risk | pintail | ok | 433.7 | 437.6 |
| q05-payment-failures | mysql | ok | 99.7 | 126.4 |
| q05-payment-failures | pintail | ok | 5.5 | 326.4 |
| q06-refund-rate | mysql | ok | 993.1 | 1020.3 |
| q06-refund-rate | pintail | ok | 1141.7 | 1194.0 |
| q07-product-performance | mysql | ok | 977.7 | 2098.5 |
| q07-product-performance | pintail | ok | 1263.7 | 1295.2 |
| q08-regional-cohorts | mysql | ok | 420.1 | 466.9 |
| q08-regional-cohorts | pintail | ok | 867.9 | 928.9 |
| q09-order-lifecycle | mysql | ok | 271.6 | 531.4 |
| q09-order-lifecycle | pintail | ok | 758.3 | 761.5 |
| q10-wide-operational-join | mysql | ok | 609.7 | 807.3 |
| q10-wide-operational-join | pintail | ok | 1158.2 | 1451.3 |
| q11-dormant-customers | mysql | ok | 14.9 | 32.2 |
| q11-dormant-customers | pintail | ok | 1089.8 | 1252.7 |
| q12-per-customer-revenue | mysql | ok | 17.0 | 72.1 |
| q12-per-customer-revenue | pintail | ok | 386.9 | 408.7 |
