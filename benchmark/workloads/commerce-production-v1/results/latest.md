# commerce-production-v1 — ci profile

Run: 2026-08-10T16:26:42.631Z → 2026-08-10T16:33:55.952Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 288.1 | 299.0 |
| q01-tenant-revenue | pintail | ok | 429.5 | 537.7 |
| q02-customer-history | mysql | ok | 8.9 | 11.2 |
| q02-customer-history | pintail | ok | 662.3 | 753.1 |
| q03-fulfillment-backlog | mysql | ok | 11.5 | 178.6 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 13.7 | 148.9 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 101.4 | 132.1 |
| q05-payment-failures | pintail | ok | 5.6 | 302.2 |
| q06-refund-rate | mysql | ok | 949.7 | 972.0 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 920.3 | 928.0 |
| q07-product-performance | pintail | ok | 1221.7 | 1251.5 |
| q08-regional-cohorts | mysql | ok | 439.6 | 449.8 |
| q08-regional-cohorts | pintail | ok | 830.0 | 840.9 |
| q09-order-lifecycle | mysql | ok | 267.4 | 267.6 |
| q09-order-lifecycle | pintail | ok | 659.4 | 666.0 |
| q10-wide-operational-join | mysql | ok | 646.8 | 662.9 |
| q10-wide-operational-join | pintail | ok | 1102.7 | 1362.6 |
