# commerce-production-v1 — ci profile

Run: 2026-08-07T22:22:45.297Z → 2026-08-07T22:25:29.695Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 198.7 | 308.5 |
| q01-tenant-revenue | pintail | ok | 409.8 | 468.1 |
| q02-customer-history | mysql | ok | 8.7 | 9.9 |
| q02-customer-history | pintail | ok | 639.7 | 812.3 |
| q03-fulfillment-backlog | mysql | ok | 8.4 | 11.9 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.5 | 12.9 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 100.3 | 258.9 |
| q05-payment-failures | pintail | ok | 4.2 | 315.2 |
| q06-refund-rate | mysql | ok | 1076.4 | 1078.3 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1070.6 | 1199.8 |
| q07-product-performance | pintail | ok | 1145.2 | 1190.3 |
| q08-regional-cohorts | mysql | ok | 476.2 | 528.3 |
| q08-regional-cohorts | pintail | ok | 723.0 | 751.9 |
| q09-order-lifecycle | mysql | ok | 266.9 | 396.9 |
| q09-order-lifecycle | pintail | ok | 626.0 | 648.1 |
| q10-wide-operational-join | mysql | ok | 527.0 | 731.6 |
| q10-wide-operational-join | pintail | ok | 944.8 | 1312.4 |
