# commerce-production-v1 — ci profile

Run: 2026-08-18T14:41:18.339Z → 2026-08-18T14:48:02.455Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 192.8 | 313.5 |
| q01-tenant-revenue | pintail | ok | 465.9 | 560.5 |
| q02-customer-history | mysql | ok | 10.5 | 20.4 |
| q02-customer-history | pintail | ok | 726.8 | 913.0 |
| q03-fulfillment-backlog | mysql | ok | 9.4 | 10.2 |
| q03-fulfillment-backlog | pintail | ok | 418.2 | 448.0 |
| q04-inventory-risk | mysql | ok | 17.5 | 22.0 |
| q04-inventory-risk | pintail | ok | 430.4 | 434.3 |
| q05-payment-failures | mysql | ok | 179.2 | 204.0 |
| q05-payment-failures | pintail | ok | 5.4 | 392.3 |
| q06-refund-rate | mysql | ok | 1049.3 | 1128.7 |
| q06-refund-rate | pintail | ok | 1481.7 | 1582.2 |
| q07-product-performance | mysql | ok | 927.1 | 928.0 |
| q07-product-performance | pintail | ok | 1310.4 | 1439.8 |
| q08-regional-cohorts | mysql | ok | 426.2 | 463.6 |
| q08-regional-cohorts | pintail | ok | 946.2 | 968.1 |
| q09-order-lifecycle | mysql | ok | 277.8 | 348.3 |
| q09-order-lifecycle | pintail | ok | 790.4 | 886.2 |
| q10-wide-operational-join | mysql | ok | 545.8 | 691.8 |
| q10-wide-operational-join | pintail | ok | 1804.6 | 2216.6 |
| q11-dormant-customers | mysql | ok | 31.8 | 34.9 |
| q11-dormant-customers | pintail | ok | 1221.0 | 1478.8 |
| q12-per-customer-revenue | mysql | ok | 54.9 | 311.9 |
| q12-per-customer-revenue | pintail | ok | 416.3 | 459.7 |
