# commerce-production-v1 — ci profile

Run: 2026-08-06T23:55:15.404Z → 2026-08-06T23:57:43.596Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 139.5 | 293.6 |
| q01-tenant-revenue | pintail | ok | 395.6 | 464.7 |
| q02-customer-history | mysql | ok | 15.0 | 218.7 |
| q02-customer-history | pintail | ok | 593.2 | 753.3 |
| q03-fulfillment-backlog | mysql | ok | 10.6 | 18.3 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.0 | 9.4 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 97.8 | 137.4 |
| q05-payment-failures | pintail | ok | 5.0 | 271.0 |
| q06-refund-rate | mysql | ok | 960.3 | 961.7 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 937.8 | 1096.5 |
| q07-product-performance | pintail | ok | 999.5 | 1084.4 |
| q08-regional-cohorts | mysql | ok | 432.0 | 617.4 |
| q08-regional-cohorts | pintail | ok | 566.2 | 585.8 |
| q09-order-lifecycle | mysql | ok | 255.2 | 256.2 |
| q09-order-lifecycle | pintail | ok | 591.5 | 603.8 |
| q10-wide-operational-join | mysql | ok | 414.7 | 594.9 |
| q10-wide-operational-join | pintail | ok | 918.5 | 1329.6 |
