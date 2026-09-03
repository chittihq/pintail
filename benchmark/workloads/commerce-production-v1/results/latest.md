# commerce-production-v1 — ci profile

Run: 2026-09-03T08:33:03.697Z → 2026-09-03T08:37:22.279Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 201.9 | 392.8 |
| q01-tenant-revenue | pintail | ok | 512.8 | 590.9 |
| q02-customer-history | mysql | ok | 8.8 | 10.4 |
| q02-customer-history | pintail | ok | 584.6 | 695.0 |
| q03-fulfillment-backlog | mysql | ok | 11.2 | 27.8 |
| q03-fulfillment-backlog | pintail | ok | 359.8 | 398.6 |
| q04-inventory-risk | mysql | ok | 13.5 | 16.4 |
| q04-inventory-risk | pintail | ok | 389.1 | 398.7 |
| q05-payment-failures | mysql | ok | 175.5 | 293.1 |
| q05-payment-failures | pintail | ok | 3.7 | 302.3 |
| q06-refund-rate | mysql | ok | 1018.0 | 1018.4 |
| q06-refund-rate | pintail | ok | 1078.0 | 1163.1 |
| q07-product-performance | mysql | ok | 1007.0 | 1703.0 |
| q07-product-performance | pintail | ok | 1146.9 | 1161.1 |
| q08-regional-cohorts | mysql | ok | 490.4 | 506.6 |
| q08-regional-cohorts | pintail | ok | 836.0 | 838.4 |
| q09-order-lifecycle | mysql | ok | 274.0 | 274.3 |
| q09-order-lifecycle | pintail | ok | 659.0 | 704.1 |
| q10-wide-operational-join | mysql | ok | 401.3 | 483.7 |
| q10-wide-operational-join | pintail | ok | 1060.5 | 1265.6 |
| q11-dormant-customers | mysql | ok | 15.6 | 106.0 |
| q11-dormant-customers | pintail | ok | 910.2 | 956.2 |
| q12-per-customer-revenue | mysql | ok | 33.2 | 37.6 |
| q12-per-customer-revenue | pintail | ok | 414.7 | 462.5 |
