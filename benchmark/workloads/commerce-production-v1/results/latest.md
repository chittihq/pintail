# commerce-production-v1 — ci profile

Run: 2026-08-03T14:08:43.256Z → 2026-08-03T14:27:46.472Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 264.8 | 880.4 |
| q01-tenant-revenue | pintail | ok | 391.3 | 464.4 |
| q02-customer-history | mysql | ok | 196.1 | 237.2 |
| q02-customer-history | pintail | ok | 663.1 | 734.7 |
| q03-fulfillment-backlog | mysql | ok | 163.8 | 165.8 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 186.1 | 188.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 341.9 | 420.5 |
| q05-payment-failures | pintail | ok | 3.2 | 289.6 |
| q06-refund-rate | mysql | ok | 2250.2 | 2269.1 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2441.4 | 2509.3 |
| q07-product-performance | pintail | ok | 1008.3 | 1076.9 |
| q08-regional-cohorts | mysql | ok | 786.9 | 1111.5 |
| q08-regional-cohorts | pintail | ok | 589.9 | 645.5 |
| q09-order-lifecycle | mysql | ok | 628.6 | 645.0 |
| q09-order-lifecycle | pintail | ok | 600.8 | 631.0 |
| q10-wide-operational-join | mysql | ok | 1505.4 | 1888.7 |
| q10-wide-operational-join | pintail | ok | 973.2 | 1370.9 |
