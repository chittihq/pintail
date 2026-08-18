# commerce-production-v1 — ci profile

Run: 2026-08-18T04:42:06.440Z → 2026-08-18T04:45:02.513Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 61.6 | 322.3 |
| q01-tenant-revenue | pintail | ok | 526.1 | 611.1 |
| q02-customer-history | mysql | ok | 40.9 | 503.1 |
| q02-customer-history | pintail | ok | 1113.7 | 2725.6 |
| q03-fulfillment-backlog | mysql | ok | 9.9 | 27.5 |
| q03-fulfillment-backlog | pintail | ok | 394.9 | 510.5 |
| q04-inventory-risk | mysql | ok | 7.7 | 26.9 |
| q04-inventory-risk | pintail | ok | 422.7 | 447.1 |
| q05-payment-failures | mysql | ok | 175.3 | 176.4 |
| q05-payment-failures | pintail | ok | 4.9 | 331.4 |
| q06-refund-rate | mysql | ok | 1038.8 | 1060.1 |
| q06-refund-rate | pintail | ok | 1132.2 | 1352.5 |
| q07-product-performance | mysql | ok | 1060.1 | 1263.8 |
| q07-product-performance | pintail | ok | 1214.9 | 1262.7 |
| q08-regional-cohorts | mysql | ok | 467.3 | 542.2 |
| q08-regional-cohorts | pintail | ok | 834.4 | 887.1 |
| q09-order-lifecycle | mysql | ok | 274.7 | 298.3 |
| q09-order-lifecycle | pintail | ok | 705.8 | 720.7 |
| q10-wide-operational-join | mysql | ok | 594.4 | 776.7 |
| q10-wide-operational-join | pintail | ok | 1460.7 | 1478.4 |
| q11-dormant-customers | mysql | ok | 34.8 | 55.9 |
| q11-dormant-customers | pintail | ok | 1072.4 | 1352.8 |
| q12-per-customer-revenue | mysql | ok | 25.2 | 42.6 |
| q12-per-customer-revenue | pintail | ok | 360.8 | 388.5 |
