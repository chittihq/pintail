# commerce-production-v1 — ci profile

Run: 2026-08-16T18:07:01.425Z → 2026-08-16T18:09:39.003Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 200.1 | 308.2 |
| q01-tenant-revenue | pintail | ok | 424.9 | 548.8 |
| q02-customer-history | mysql | ok | 9.2 | 9.4 |
| q02-customer-history | pintail | ok | 734.2 | 823.8 |
| q03-fulfillment-backlog | mysql | ok | 10.8 | 13.7 |
| q03-fulfillment-backlog | pintail | ok | 350.5 | 432.0 |
| q04-inventory-risk | mysql | ok | 12.1 | 52.4 |
| q04-inventory-risk | pintail | ok | 370.1 | 390.6 |
| q05-payment-failures | mysql | ok | 145.5 | 171.1 |
| q05-payment-failures | pintail | ok | 4.2 | 336.4 |
| q06-refund-rate | mysql | ok | 1111.6 | 1162.0 |
| q06-refund-rate | pintail | ok | 1029.8 | 1220.3 |
| q07-product-performance | mysql | ok | 1063.4 | 1409.1 |
| q07-product-performance | pintail | ok | 1162.0 | 1196.8 |
| q08-regional-cohorts | mysql | ok | 414.6 | 526.5 |
| q08-regional-cohorts | pintail | ok | 796.6 | 820.9 |
| q09-order-lifecycle | mysql | ok | 266.3 | 311.3 |
| q09-order-lifecycle | pintail | ok | 688.0 | 710.5 |
| q10-wide-operational-join | mysql | ok | 711.3 | 808.3 |
| q10-wide-operational-join | pintail | ok | 1152.6 | 1452.4 |
| q11-dormant-customers | mysql | ok | 13.6 | 37.4 |
| q11-dormant-customers | pintail | ok | 1042.9 | 1196.0 |
| q12-per-customer-revenue | mysql | ok | 27.1 | 33.2 |
| q12-per-customer-revenue | pintail | ok | 360.4 | 421.6 |
