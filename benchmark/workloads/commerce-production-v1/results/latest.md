# commerce-production-v1 — ci profile

Run: 2026-08-18T04:38:37.165Z → 2026-08-18T04:41:33.500Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 60.6 | 319.1 |
| q01-tenant-revenue | pintail | ok | 497.8 | 588.9 |
| q02-customer-history | mysql | ok | 11.3 | 154.0 |
| q02-customer-history | pintail | ok | 682.8 | 913.2 |
| q03-fulfillment-backlog | mysql | ok | 8.1 | 11.5 |
| q03-fulfillment-backlog | pintail | ok | 391.4 | 411.5 |
| q04-inventory-risk | mysql | ok | 9.8 | 13.4 |
| q04-inventory-risk | pintail | ok | 455.1 | 466.2 |
| q05-payment-failures | mysql | ok | 159.6 | 195.8 |
| q05-payment-failures | pintail | ok | 4.9 | 312.9 |
| q06-refund-rate | mysql | ok | 1055.7 | 1132.8 |
| q06-refund-rate | pintail | ok | 1652.6 | 1808.4 |
| q07-product-performance | mysql | ok | 1057.3 | 1098.0 |
| q07-product-performance | pintail | ok | 1242.6 | 1257.3 |
| q08-regional-cohorts | mysql | ok | 473.3 | 601.3 |
| q08-regional-cohorts | pintail | ok | 855.8 | 876.3 |
| q09-order-lifecycle | mysql | ok | 332.2 | 676.6 |
| q09-order-lifecycle | pintail | ok | 862.1 | 953.6 |
| q10-wide-operational-join | mysql | ok | 754.6 | 797.7 |
| q10-wide-operational-join | pintail | ok | 1470.5 | 1656.5 |
| q11-dormant-customers | mysql | ok | 119.8 | 510.9 |
| q11-dormant-customers | pintail | ok | 1195.9 | 1410.4 |
| q12-per-customer-revenue | mysql | ok | 23.1 | 30.7 |
| q12-per-customer-revenue | pintail | ok | 430.4 | 439.6 |
