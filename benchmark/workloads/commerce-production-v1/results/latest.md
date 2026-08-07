# commerce-production-v1 — ci profile

Run: 2026-08-07T04:48:47.858Z → 2026-08-07T04:51:35.550Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 265.2 | 410.5 |
| q01-tenant-revenue | pintail | ok | 387.7 | 471.4 |
| q02-customer-history | mysql | ok | 9.4 | 11.5 |
| q02-customer-history | pintail | ok | 629.1 | 690.0 |
| q03-fulfillment-backlog | mysql | ok | 8.8 | 10.7 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.6 | 10.1 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 130.2 | 250.8 |
| q05-payment-failures | pintail | ok | 5.4 | 323.4 |
| q06-refund-rate | mysql | ok | 932.4 | 932.6 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1088.9 | 1094.5 |
| q07-product-performance | pintail | ok | 1067.2 | 1118.3 |
| q08-regional-cohorts | mysql | ok | 417.4 | 452.3 |
| q08-regional-cohorts | pintail | ok | 710.9 | 770.4 |
| q09-order-lifecycle | mysql | ok | 283.3 | 293.1 |
| q09-order-lifecycle | pintail | ok | 609.0 | 618.8 |
| q10-wide-operational-join | mysql | ok | 505.7 | 524.6 |
| q10-wide-operational-join | pintail | ok | 929.8 | 1267.3 |
