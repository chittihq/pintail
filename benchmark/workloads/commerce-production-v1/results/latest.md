# commerce-production-v1 — ci profile

Run: 2026-09-25T18:17:50.881Z → 2026-09-25T18:18:41.843Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 50.3 | 305.0 |
| q01-tenant-revenue | pintail | ok | 2.5 | 49.0 |
| q02-customer-history | mysql | ok | 0.8 | 7.3 |
| q02-customer-history | pintail | ok | 109.6 | 117.5 |
| q03-fulfillment-backlog | mysql | ok | 0.8 | 2.3 |
| q03-fulfillment-backlog | pintail | ok | 2.0 | 11.2 |
| q04-inventory-risk | mysql | ok | 0.7 | 2.4 |
| q04-inventory-risk | pintail | ok | 65.2 | 66.5 |
| q05-payment-failures | mysql | ok | 89.5 | 128.5 |
| q05-payment-failures | pintail | ok | 2.3 | 152.6 |
| q06-refund-rate | mysql | ok | 972.4 | 976.7 |
| q06-refund-rate | pintail | ok | 207.9 | 210.0 |
| q07-product-performance | mysql | ok | 910.3 | 918.6 |
| q07-product-performance | pintail | ok | 220.7 | 221.0 |
| q08-regional-cohorts | mysql | ok | 411.5 | 473.1 |
| q08-regional-cohorts | pintail | ok | 281.7 | 284.6 |
| q09-order-lifecycle | mysql | ok | 258.3 | 259.3 |
| q09-order-lifecycle | pintail | ok | 304.5 | 308.5 |
| q10-wide-operational-join | mysql | ok | 456.7 | 600.9 |
| q10-wide-operational-join | pintail | ok | 241.7 | 262.2 |
| q11-dormant-customers | mysql | ok | 5.6 | 23.1 |
| q11-dormant-customers | pintail | ok | 7.8 | 8.2 |
| q12-per-customer-revenue | mysql | ok | 5.5 | 9.8 |
| q12-per-customer-revenue | pintail | ok | 2.5 | 9.8 |
