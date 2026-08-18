# commerce-production-v1 — ci profile

Run: 2026-08-18T08:32:50.666Z → 2026-08-18T08:36:04.160Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 57.8 | 508.1 |
| q01-tenant-revenue | pintail | ok | 692.8 | 724.5 |
| q02-customer-history | mysql | ok | 12.6 | 69.9 |
| q02-customer-history | pintail | ok | 1097.3 | 1191.6 |
| q03-fulfillment-backlog | mysql | ok | 13.2 | 78.2 |
| q03-fulfillment-backlog | pintail | ok | 425.5 | 591.5 |
| q04-inventory-risk | mysql | ok | 16.7 | 94.0 |
| q04-inventory-risk | pintail | ok | 469.6 | 478.7 |
| q05-payment-failures | mysql | ok | 210.4 | 332.7 |
| q05-payment-failures | pintail | ok | 5.9 | 336.3 |
| q06-refund-rate | mysql | ok | 1002.4 | 1144.5 |
| q06-refund-rate | pintail | ok | 1966.5 | 2016.7 |
| q07-product-performance | mysql | ok | 1011.8 | 1021.7 |
| q07-product-performance | pintail | ok | 1571.8 | 1795.1 |
| q08-regional-cohorts | mysql | ok | 513.2 | 581.3 |
| q08-regional-cohorts | pintail | ok | 1145.1 | 1182.7 |
| q09-order-lifecycle | mysql | ok | 265.2 | 275.4 |
| q09-order-lifecycle | pintail | ok | 889.2 | 970.6 |
| q10-wide-operational-join | mysql | ok | 652.5 | 768.8 |
| q10-wide-operational-join | pintail | ok | 1987.5 | 2219.4 |
| q11-dormant-customers | mysql | ok | 35.5 | 99.9 |
| q11-dormant-customers | pintail | ok | 2372.0 | 4011.4 |
| q12-per-customer-revenue | mysql | ok | 24.9 | 93.4 |
| q12-per-customer-revenue | pintail | ok | 436.0 | 794.9 |
