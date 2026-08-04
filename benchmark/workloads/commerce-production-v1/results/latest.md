# commerce-production-v1 — ci profile

Run: 2026-08-04T08:35:22.398Z → 2026-08-04T08:54:35.083Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 351.0 | 910.8 |
| q01-tenant-revenue | pintail | ok | 379.4 | 484.2 |
| q02-customer-history | mysql | ok | 186.7 | 311.1 |
| q02-customer-history | pintail | ok | 620.4 | 779.8 |
| q03-fulfillment-backlog | mysql | ok | 194.0 | 880.7 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 181.3 | 255.5 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 334.3 | 407.8 |
| q05-payment-failures | pintail | ok | 3.4 | 319.0 |
| q06-refund-rate | mysql | ok | 2063.7 | 2143.9 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2441.0 | 2456.2 |
| q07-product-performance | pintail | ok | 1066.3 | 1109.0 |
| q08-regional-cohorts | mysql | ok | 814.4 | 1003.9 |
| q08-regional-cohorts | pintail | ok | 578.7 | 643.1 |
| q09-order-lifecycle | mysql | ok | 628.2 | 632.8 |
| q09-order-lifecycle | pintail | ok | 610.8 | 626.1 |
| q10-wide-operational-join | mysql | ok | 1405.6 | 1971.6 |
| q10-wide-operational-join | pintail | ok | 1102.0 | 1491.4 |
