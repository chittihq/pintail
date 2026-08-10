# commerce-production-v1 — ci profile

Run: 2026-08-10T06:52:59.689Z → 2026-08-10T06:56:57.244Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 159.5 | 403.4 |
| q01-tenant-revenue | pintail | ok | 409.5 | 505.4 |
| q02-customer-history | mysql | ok | 29.2 | 249.5 |
| q02-customer-history | pintail | ok | 633.4 | 847.6 |
| q03-fulfillment-backlog | mysql | ok | 7.5 | 167.5 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 18.9 | 191.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 264.6 | 760.6 |
| q05-payment-failures | pintail | ok | 4.3 | 387.6 |
| q06-refund-rate | mysql | ok | 1010.8 | 1024.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 979.3 | 1091.3 |
| q07-product-performance | pintail | ok | 1162.6 | 1277.6 |
| q08-regional-cohorts | mysql | ok | 404.7 | 454.5 |
| q08-regional-cohorts | pintail | ok | 825.8 | 872.3 |
| q09-order-lifecycle | mysql | ok | 411.4 | 727.7 |
| q09-order-lifecycle | pintail | ok | 685.9 | 704.0 |
| q10-wide-operational-join | mysql | ok | 551.1 | 753.1 |
| q10-wide-operational-join | pintail | ok | 1020.4 | 1529.7 |
