# commerce-production-v1 — ci profile

Run: 2026-08-18T15:24:14.968Z → 2026-08-18T15:28:42.724Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 59.9 | 332.2 |
| q01-tenant-revenue | pintail | ok | 446.1 | 531.3 |
| q02-customer-history | mysql | ok | 9.8 | 12.2 |
| q02-customer-history | pintail | ok | 781.8 | 787.3 |
| q03-fulfillment-backlog | mysql | ok | 9.7 | 14.7 |
| q03-fulfillment-backlog | pintail | ok | 433.4 | 449.6 |
| q04-inventory-risk | mysql | ok | 11.2 | 141.7 |
| q04-inventory-risk | pintail | ok | 476.7 | 618.1 |
| q05-payment-failures | mysql | ok | 105.7 | 145.6 |
| q05-payment-failures | pintail | ok | 5.3 | 338.8 |
| q06-refund-rate | mysql | ok | 1117.0 | 1131.2 |
| q06-refund-rate | pintail | ok | 1294.2 | 1410.0 |
| q07-product-performance | mysql | ok | 967.9 | 990.5 |
| q07-product-performance | pintail | ok | 1284.5 | 1293.2 |
| q08-regional-cohorts | mysql | ok | 427.9 | 468.7 |
| q08-regional-cohorts | pintail | ok | 828.0 | 863.5 |
| q09-order-lifecycle | mysql | ok | 308.6 | 334.0 |
| q09-order-lifecycle | pintail | ok | 712.4 | 717.3 |
| q10-wide-operational-join | mysql | ok | 509.8 | 652.9 |
| q10-wide-operational-join | pintail | ok | 1247.4 | 1423.9 |
| q11-dormant-customers | mysql | ok | 34.5 | 98.6 |
| q11-dormant-customers | pintail | ok | 1060.9 | 1238.3 |
| q12-per-customer-revenue | mysql | ok | 17.2 | 38.4 |
| q12-per-customer-revenue | pintail | ok | 370.9 | 376.6 |
