# commerce-production-v1 — ci profile

Run: 2026-08-10T07:54:12.531Z → 2026-08-10T07:58:15.383Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 205.7 | 349.8 |
| q01-tenant-revenue | pintail | ok | 405.6 | 515.3 |
| q02-customer-history | mysql | ok | 11.3 | 140.6 |
| q02-customer-history | pintail | ok | 584.5 | 752.0 |
| q03-fulfillment-backlog | mysql | ok | 9.2 | 29.6 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 22.9 | 32.9 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 143.7 | 181.9 |
| q05-payment-failures | pintail | ok | 7.2 | 345.4 |
| q06-refund-rate | mysql | ok | 986.7 | 991.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 981.1 | 994.6 |
| q07-product-performance | pintail | ok | 1117.7 | 1226.2 |
| q08-regional-cohorts | mysql | ok | 405.2 | 454.0 |
| q08-regional-cohorts | pintail | ok | 771.7 | 795.0 |
| q09-order-lifecycle | mysql | ok | 266.2 | 283.8 |
| q09-order-lifecycle | pintail | ok | 611.7 | 651.6 |
| q10-wide-operational-join | mysql | ok | 669.7 | 752.3 |
| q10-wide-operational-join | pintail | ok | 965.5 | 1353.8 |
