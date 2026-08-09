# commerce-production-v1 — ci profile

Run: 2026-08-09T17:40:02.631Z → 2026-08-09T17:42:24.245Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 58.8 | 387.8 |
| q01-tenant-revenue | pintail | ok | 430.4 | 490.1 |
| q02-customer-history | mysql | ok | 31.7 | 154.4 |
| q02-customer-history | pintail | ok | 689.8 | 774.3 |
| q03-fulfillment-backlog | mysql | ok | 8.2 | 11.5 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 20.1 | 44.8 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 134.2 | 170.2 |
| q05-payment-failures | pintail | ok | 5.5 | 312.1 |
| q06-refund-rate | mysql | ok | 1053.9 | 1140.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1049.3 | 1050.9 |
| q07-product-performance | pintail | ok | 1287.2 | 1292.4 |
| q08-regional-cohorts | mysql | ok | 456.6 | 606.4 |
| q08-regional-cohorts | pintail | ok | 807.7 | 855.5 |
| q09-order-lifecycle | mysql | ok | 265.7 | 290.2 |
| q09-order-lifecycle | pintail | ok | 648.6 | 665.2 |
| q10-wide-operational-join | mysql | ok | 601.4 | 769.6 |
| q10-wide-operational-join | pintail | ok | 971.1 | 1500.6 |
