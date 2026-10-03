# commerce-production-v1 — ci profile

Run: 2026-10-03T05:51:01.232Z → 2026-10-03T05:51:54.245Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 33.1 | 104.8 |
| q01-tenant-revenue | pintail | ok | 1.5 | 18.4 |
| q02-customer-history | mysql | ok | 0.4 | 1.4 |
| q02-customer-history | pintail | ok | 2.2 | 6.6 |
| q03-fulfillment-backlog | mysql | ok | 0.7 | 1.4 |
| q03-fulfillment-backlog | pintail | ok | 0.9 | 3.0 |
| q04-inventory-risk | mysql | ok | 0.3 | 1.1 |
| q04-inventory-risk | pintail | ok | 1.5 | 1.8 |
| q05-payment-failures | mysql | ok | 49.2 | 73.6 |
| q05-payment-failures | pintail | ok | 1.6 | 150.4 |
| q06-refund-rate | mysql | ok | 530.8 | 571.6 |
| q06-refund-rate | pintail | ok | 41.1 | 44.5 |
| q07-product-performance | mysql | ok | 510.8 | 511.2 |
| q07-product-performance | pintail | ok | 164.6 | 165.8 |
| q08-regional-cohorts | mysql | ok | 212.0 | 236.5 |
| q08-regional-cohorts | pintail | ok | 372.9 | 376.5 |
| q09-order-lifecycle | mysql | ok | 168.2 | 179.2 |
| q09-order-lifecycle | pintail | ok | 235.7 | 243.5 |
| q10-wide-operational-join | mysql | ok | 165.1 | 192.3 |
| q10-wide-operational-join | pintail | ok | 19.9 | 22.2 |
| q11-dormant-customers | mysql | ok | 3.9 | 11.1 |
| q11-dormant-customers | pintail | ok | 2.7 | 3.0 |
| q12-per-customer-revenue | mysql | ok | 3.4 | 6.6 |
| q12-per-customer-revenue | pintail | ok | 1.6 | 3.4 |
