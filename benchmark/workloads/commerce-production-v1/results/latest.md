# commerce-production-v1 — ci profile

Run: 2026-08-18T15:17:49.289Z → 2026-08-18T15:23:06.623Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 203.0 | 502.6 |
| q01-tenant-revenue | pintail | ok | 422.2 | 510.7 |
| q02-customer-history | mysql | ok | 8.4 | 9.9 |
| q02-customer-history | pintail | ok | 747.4 | 963.0 |
| q03-fulfillment-backlog | mysql | ok | 13.4 | 15.2 |
| q03-fulfillment-backlog | pintail | ok | 386.4 | 477.1 |
| q04-inventory-risk | mysql | ok | 11.5 | 27.9 |
| q04-inventory-risk | pintail | ok | 413.9 | 417.7 |
| q05-payment-failures | mysql | ok | 104.9 | 202.9 |
| q05-payment-failures | pintail | ok | 5.0 | 335.1 |
| q06-refund-rate | mysql | ok | 1030.6 | 1395.4 |
| q06-refund-rate | pintail | ok | 1133.4 | 1387.4 |
| q07-product-performance | mysql | ok | 1295.1 | 1302.9 |
| q07-product-performance | pintail | ok | 1273.5 | 1292.3 |
| q08-regional-cohorts | mysql | ok | 440.2 | 461.0 |
| q08-regional-cohorts | pintail | ok | 830.2 | 878.3 |
| q09-order-lifecycle | mysql | ok | 282.5 | 357.6 |
| q09-order-lifecycle | pintail | ok | 700.4 | 756.6 |
| q10-wide-operational-join | mysql | ok | 806.7 | 940.6 |
| q10-wide-operational-join | pintail | ok | 1691.1 | 1716.7 |
| q11-dormant-customers | mysql | ok | 16.9 | 34.0 |
| q11-dormant-customers | pintail | ok | 1133.2 | 1394.2 |
| q12-per-customer-revenue | mysql | ok | 305.0 | 401.7 |
| q12-per-customer-revenue | pintail | ok | 425.2 | 448.1 |
