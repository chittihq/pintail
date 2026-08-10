# commerce-production-v1 — ci profile

Run: 2026-08-10T12:50:13.909Z → 2026-08-10T12:54:33.655Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 145.2 | 383.7 |
| q01-tenant-revenue | pintail | ok | 450.7 | 549.2 |
| q02-customer-history | mysql | ok | 16.3 | 16.3 |
| q02-customer-history | pintail | ok | 721.1 | 818.1 |
| q03-fulfillment-backlog | mysql | ok | 16.1 | 25.0 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 115.8 | 170.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 154.0 | 185.8 |
| q05-payment-failures | pintail | ok | 4.8 | 320.5 |
| q06-refund-rate | mysql | ok | 988.4 | 1003.3 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1071.5 | 1533.6 |
| q07-product-performance | pintail | ok | 1152.7 | 1299.0 |
| q08-regional-cohorts | mysql | ok | 452.2 | 512.6 |
| q08-regional-cohorts | pintail | ok | 854.9 | 919.6 |
| q09-order-lifecycle | mysql | ok | 265.2 | 278.4 |
| q09-order-lifecycle | pintail | ok | 627.3 | 656.8 |
| q10-wide-operational-join | mysql | ok | 533.4 | 685.4 |
| q10-wide-operational-join | pintail | ok | 966.7 | 1472.2 |
