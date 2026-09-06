# commerce-production-v1 — ci profile

Run: 2026-09-06T17:35:50.988Z → 2026-09-06T17:39:41.898Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 60.8 | 287.5 |
| q01-tenant-revenue | pintail | ok | 3.7 | 162.3 |
| q02-customer-history | mysql | ok | 11.9 | 74.4 |
| q02-customer-history | pintail | ok | 6.1 | 349.9 |
| q03-fulfillment-backlog | mysql | ok | 8.6 | 164.5 |
| q03-fulfillment-backlog | pintail | ok | 3.3 | 72.2 |
| q04-inventory-risk | mysql | ok | 10.6 | 12.5 |
| q04-inventory-risk | pintail | ok | 6.4 | 168.8 |
| q05-payment-failures | mysql | ok | 100.0 | 187.4 |
| q05-payment-failures | pintail | ok | 15.3 | 416.2 |
| q06-refund-rate | mysql | ok | 1003.4 | 1051.0 |
| q06-refund-rate | pintail | ok | 26.5 | 4955.6 |
| q07-product-performance | mysql | ok | 961.5 | 985.2 |
| q07-product-performance | pintail | ok | 154.5 | 2070.6 |
| q08-regional-cohorts | mysql | ok | 456.0 | 477.7 |
| q08-regional-cohorts | pintail | ok | 1644.1 | 2143.9 |
| q09-order-lifecycle | mysql | ok | 282.5 | 306.8 |
| q09-order-lifecycle | pintail | ok | 912.6 | 1297.2 |
| q10-wide-operational-join | mysql | ok | 504.1 | 660.3 |
| q10-wide-operational-join | pintail | ok | 5.1 | 298.1 |
| q11-dormant-customers | mysql | ok | 19.0 | 35.3 |
| q11-dormant-customers | pintail | ok | 623.3 | 687.9 |
| q12-per-customer-revenue | mysql | ok | 14.6 | 29.6 |
| q12-per-customer-revenue | pintail | ok | 5.7 | 84.0 |
