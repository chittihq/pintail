# commerce-production-v1 — ci profile

Run: 2026-08-17T18:47:05.941Z → 2026-08-17T18:51:11.579Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 58.4 | 291.3 |
| q01-tenant-revenue | pintail | ok | 421.0 | 536.5 |
| q02-customer-history | mysql | ok | 8.9 | 35.6 |
| q02-customer-history | pintail | ok | 673.8 | 739.8 |
| q03-fulfillment-backlog | mysql | ok | 139.6 | 144.6 |
| q03-fulfillment-backlog | pintail | ok | 402.7 | 474.2 |
| q04-inventory-risk | mysql | ok | 8.9 | 71.3 |
| q04-inventory-risk | pintail | ok | 413.0 | 427.6 |
| q05-payment-failures | mysql | ok | 99.6 | 340.3 |
| q05-payment-failures | pintail | ok | 4.8 | 309.7 |
| q06-refund-rate | mysql | ok | 1020.6 | 1102.0 |
| q06-refund-rate | pintail | ok | 1153.9 | 1362.9 |
| q07-product-performance | mysql | ok | 934.1 | 952.8 |
| q07-product-performance | pintail | ok | 1237.3 | 1303.6 |
| q08-regional-cohorts | mysql | ok | 466.9 | 555.5 |
| q08-regional-cohorts | pintail | ok | 897.5 | 909.2 |
| q09-order-lifecycle | mysql | ok | 282.9 | 345.0 |
| q09-order-lifecycle | pintail | ok | 829.4 | 902.6 |
| q10-wide-operational-join | mysql | ok | 521.1 | 652.2 |
| q10-wide-operational-join | pintail | ok | 1219.8 | 1958.2 |
| q11-dormant-customers | mysql | ok | 24.0 | 41.3 |
| q11-dormant-customers | pintail | ok | 1333.2 | 1418.4 |
| q12-per-customer-revenue | mysql | ok | 15.7 | 601.4 |
| q12-per-customer-revenue | pintail | ok | 408.0 | 453.9 |
