# commerce-production-v1 — ci profile

Run: 2026-08-07T13:36:29.341Z → 2026-08-07T13:39:05.258Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 138.3 | 427.2 |
| q01-tenant-revenue | pintail | ok | 412.8 | 503.4 |
| q02-customer-history | mysql | ok | 9.9 | 15.3 |
| q02-customer-history | pintail | ok | 644.9 | 682.3 |
| q03-fulfillment-backlog | mysql | ok | 12.0 | 20.0 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 10.7 | 136.8 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 105.8 | 186.0 |
| q05-payment-failures | pintail | ok | 3.9 | 306.1 |
| q06-refund-rate | mysql | ok | 940.0 | 1033.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 919.6 | 1011.8 |
| q07-product-performance | pintail | ok | 1050.3 | 1121.7 |
| q08-regional-cohorts | mysql | ok | 402.1 | 468.0 |
| q08-regional-cohorts | pintail | ok | 703.3 | 734.6 |
| q09-order-lifecycle | mysql | ok | 267.5 | 999.5 |
| q09-order-lifecycle | pintail | ok | 610.0 | 635.1 |
| q10-wide-operational-join | mysql | ok | 501.6 | 751.9 |
| q10-wide-operational-join | pintail | ok | 977.4 | 1336.3 |
