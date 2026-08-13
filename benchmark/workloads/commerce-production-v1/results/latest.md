# commerce-production-v1 — ci profile

Run: 2026-08-13T06:34:30.707Z → 2026-08-13T06:37:12.237Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 57.2 | 438.7 |
| q01-tenant-revenue | pintail | ok | 423.3 | 501.1 |
| q02-customer-history | mysql | ok | 9.4 | 11.2 |
| q02-customer-history | pintail | ok | 611.1 | 786.4 |
| q03-fulfillment-backlog | mysql | ok | 7.9 | 137.2 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 7.6 | 9.0 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 100.2 | 137.8 |
| q05-payment-failures | pintail | ok | 4.6 | 293.6 |
| q06-refund-rate | mysql | ok | 1048.4 | 1053.3 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1178.1 | 1215.1 |
| q07-product-performance | pintail | ok | 1182.2 | 1299.1 |
| q08-regional-cohorts | mysql | ok | 401.5 | 555.5 |
| q08-regional-cohorts | pintail | ok | 901.4 | 1016.7 |
| q09-order-lifecycle | mysql | ok | 264.0 | 264.1 |
| q09-order-lifecycle | pintail | ok | 633.5 | 668.9 |
| q10-wide-operational-join | mysql | ok | 512.8 | 777.5 |
| q10-wide-operational-join | pintail | ok | 1350.7 | 1575.8 |
| q11-dormant-customers | mysql | ok | 20.4 | 33.6 |
| q11-dormant-customers | pintail | ok | 986.4 | 1141.5 |
| q12-per-customer-revenue | mysql | ok | 15.8 | 30.1 |
| q12-per-customer-revenue | pintail | ok | 386.1 | 413.4 |
