# commerce-production-v1 — ci profile

Run: 2026-08-19T17:19:07.005Z → 2026-08-19T17:20:54.743Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 61.2 | 323.0 |
| q01-tenant-revenue | pintail | ok | 393.9 | 476.3 |
| q02-customer-history | mysql | ok | 7.4 | 77.8 |
| q02-customer-history | pintail | ok | 586.0 | 688.8 |
| q03-fulfillment-backlog | mysql | ok | 11.5 | 13.5 |
| q03-fulfillment-backlog | pintail | ok | 347.2 | 358.8 |
| q04-inventory-risk | mysql | ok | 10.1 | 14.5 |
| q04-inventory-risk | pintail | ok | 391.8 | 393.5 |
| q05-payment-failures | mysql | ok | 104.6 | 170.5 |
| q05-payment-failures | pintail | ok | 4.6 | 304.3 |
| q06-refund-rate | mysql | ok | 1077.8 | 1088.4 |
| q06-refund-rate | pintail | ok | 1063.2 | 1101.3 |
| q07-product-performance | mysql | ok | 1014.0 | 1029.2 |
| q07-product-performance | pintail | ok | 1166.4 | 1240.6 |
| q08-regional-cohorts | mysql | ok | 483.0 | 507.2 |
| q08-regional-cohorts | pintail | ok | 765.6 | 796.2 |
| q09-order-lifecycle | mysql | ok | 280.4 | 354.4 |
| q09-order-lifecycle | pintail | ok | 657.1 | 706.9 |
| q10-wide-operational-join | mysql | ok | 528.7 | 640.8 |
| q10-wide-operational-join | pintail | ok | 863.5 | 1122.7 |
| q11-dormant-customers | mysql | ok | 15.0 | 32.7 |
| q11-dormant-customers | pintail | ok | 979.9 | 1076.1 |
| q12-per-customer-revenue | mysql | ok | 16.7 | 30.5 |
| q12-per-customer-revenue | pintail | ok | 333.8 | 344.2 |
