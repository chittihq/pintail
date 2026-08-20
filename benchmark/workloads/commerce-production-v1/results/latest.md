# commerce-production-v1 — ci profile

Run: 2026-08-20T10:34:55.978Z → 2026-08-20T10:38:03.298Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 138.1 | 399.6 |
| q01-tenant-revenue | pintail | ok | 424.1 | 517.7 |
| q02-customer-history | mysql | ok | 5.0 | 9.8 |
| q02-customer-history | pintail | ok | 625.4 | 722.4 |
| q03-fulfillment-backlog | mysql | ok | 6.8 | 9.9 |
| q03-fulfillment-backlog | pintail | ok | 377.2 | 413.1 |
| q04-inventory-risk | mysql | ok | 4.4 | 10.9 |
| q04-inventory-risk | pintail | ok | 413.0 | 421.0 |
| q05-payment-failures | mysql | ok | 155.5 | 345.1 |
| q05-payment-failures | pintail | ok | 4.5 | 332.0 |
| q06-refund-rate | mysql | ok | 1023.5 | 1028.0 |
| q06-refund-rate | pintail | ok | 1211.7 | 1400.3 |
| q07-product-performance | mysql | ok | 998.9 | 1009.0 |
| q07-product-performance | pintail | ok | 1230.2 | 1300.8 |
| q08-regional-cohorts | mysql | ok | 425.1 | 468.8 |
| q08-regional-cohorts | pintail | ok | 783.0 | 871.7 |
| q09-order-lifecycle | mysql | ok | 276.3 | 355.0 |
| q09-order-lifecycle | pintail | ok | 668.5 | 703.4 |
| q10-wide-operational-join | mysql | ok | 625.3 | 841.0 |
| q10-wide-operational-join | pintail | ok | 1275.2 | 1493.1 |
| q11-dormant-customers | mysql | ok | 12.5 | 42.2 |
| q11-dormant-customers | pintail | ok | 1021.8 | 1278.1 |
| q12-per-customer-revenue | mysql | ok | 14.0 | 24.1 |
| q12-per-customer-revenue | pintail | ok | 344.3 | 374.9 |
