# commerce-production-v1 — ci profile

Run: 2026-08-13T23:00:31.639Z → 2026-08-13T23:06:29.014Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 57.8 | 309.5 |
| q01-tenant-revenue | pintail | ok | 430.6 | 566.3 |
| q02-customer-history | mysql | ok | 9.2 | 56.4 |
| q02-customer-history | pintail | ok | 931.9 | 951.4 |
| q03-fulfillment-backlog | mysql | ok | 11.5 | 14.3 |
| q03-fulfillment-backlog | pintail | ok | 478.3 | 489.5 |
| q04-inventory-risk | mysql | ok | 34.6 | 35.2 |
| q04-inventory-risk | pintail | ok | 459.1 | 490.1 |
| q05-payment-failures | mysql | ok | 101.0 | 157.8 |
| q05-payment-failures | pintail | ok | 5.4 | 330.3 |
| q06-refund-rate | mysql | ok | 1039.2 | 1048.9 |
| q06-refund-rate | pintail | ok | 1423.5 | 1734.3 |
| q07-product-performance | mysql | ok | 920.7 | 1003.8 |
| q07-product-performance | pintail | ok | 1265.1 | 1342.3 |
| q08-regional-cohorts | mysql | ok | 412.7 | 459.4 |
| q08-regional-cohorts | pintail | ok | 795.3 | 974.5 |
| q09-order-lifecycle | mysql | ok | 263.4 | 271.7 |
| q09-order-lifecycle | pintail | ok | 672.8 | 704.4 |
| q10-wide-operational-join | mysql | ok | 515.2 | 648.7 |
| q10-wide-operational-join | pintail | ok | 1992.9 | 2013.9 |
| q11-dormant-customers | mysql | ok | 15.6 | 156.6 |
| q11-dormant-customers | pintail | ok | 1114.1 | 1220.3 |
| q12-per-customer-revenue | mysql | ok | 17.2 | 87.4 |
| q12-per-customer-revenue | pintail | ok | 366.2 | 372.8 |
