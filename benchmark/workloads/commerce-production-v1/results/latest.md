# commerce-production-v1 — ci profile

Run: 2026-08-13T13:21:46.719Z → 2026-08-13T13:26:46.434Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 101.7 | 387.4 |
| q01-tenant-revenue | pintail | ok | 546.0 | 749.0 |
| q02-customer-history | mysql | ok | 7.2 | 8.8 |
| q02-customer-history | pintail | ok | 841.9 | 908.7 |
| q03-fulfillment-backlog | mysql | ok | 5.7 | 16.0 |
| q03-fulfillment-backlog | pintail | ok | 504.1 | 568.3 |
| q04-inventory-risk | mysql | ok | 5.7 | 7.4 |
| q04-inventory-risk | pintail | ok | 633.0 | 653.0 |
| q05-payment-failures | mysql | ok | 97.0 | 138.3 |
| q05-payment-failures | pintail | ok | 5.6 | 326.6 |
| q06-refund-rate | mysql | ok | 908.4 | 934.9 |
| q06-refund-rate | pintail | ok | 1128.3 | 1512.9 |
| q07-product-performance | mysql | ok | 893.8 | 1023.1 |
| q07-product-performance | pintail | ok | 1312.5 | 1815.5 |
| q08-regional-cohorts | mysql | ok | 409.5 | 464.7 |
| q08-regional-cohorts | pintail | ok | 855.3 | 1181.1 |
| q09-order-lifecycle | mysql | ok | 266.7 | 273.9 |
| q09-order-lifecycle | pintail | ok | 863.0 | 951.7 |
| q10-wide-operational-join | mysql | ok | 587.0 | 786.0 |
| q10-wide-operational-join | pintail | ok | 1727.6 | 2032.3 |
| q11-dormant-customers | mysql | ok | 14.2 | 43.1 |
| q11-dormant-customers | pintail | ok | 1135.4 | 1426.4 |
| q12-per-customer-revenue | mysql | ok | 15.2 | 31.1 |
| q12-per-customer-revenue | pintail | ok | 412.5 | 457.5 |
