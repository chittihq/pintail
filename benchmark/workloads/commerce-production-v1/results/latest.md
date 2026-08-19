# commerce-production-v1 — ci profile

Run: 2026-08-19T13:34:31.058Z → 2026-08-19T13:36:26.417Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 55.9 | 318.3 |
| q01-tenant-revenue | pintail | ok | 421.5 | 486.6 |
| q02-customer-history | mysql | ok | 6.4 | 6.4 |
| q02-customer-history | pintail | ok | 588.1 | 665.9 |
| q03-fulfillment-backlog | mysql | ok | 4.8 | 7.3 |
| q03-fulfillment-backlog | pintail | ok | 355.0 | 368.5 |
| q04-inventory-risk | mysql | ok | 6.2 | 7.1 |
| q04-inventory-risk | pintail | ok | 395.8 | 462.5 |
| q05-payment-failures | mysql | ok | 94.9 | 142.3 |
| q05-payment-failures | pintail | ok | 4.6 | 315.4 |
| q06-refund-rate | mysql | ok | 968.2 | 977.0 |
| q06-refund-rate | pintail | ok | 1040.5 | 1078.3 |
| q07-product-performance | mysql | ok | 929.8 | 930.4 |
| q07-product-performance | pintail | ok | 1274.9 | 1275.7 |
| q08-regional-cohorts | mysql | ok | 417.9 | 458.3 |
| q08-regional-cohorts | pintail | ok | 828.9 | 836.3 |
| q09-order-lifecycle | mysql | ok | 273.9 | 275.8 |
| q09-order-lifecycle | pintail | ok | 677.7 | 736.4 |
| q10-wide-operational-join | mysql | ok | 473.5 | 589.4 |
| q10-wide-operational-join | pintail | ok | 926.4 | 1216.4 |
| q11-dormant-customers | mysql | ok | 11.4 | 29.6 |
| q11-dormant-customers | pintail | ok | 1018.9 | 1173.0 |
| q12-per-customer-revenue | mysql | ok | 12.2 | 22.9 |
| q12-per-customer-revenue | pintail | ok | 347.5 | 368.6 |
