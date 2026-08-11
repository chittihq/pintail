# commerce-production-v1 — ci profile

Run: 2026-08-11T09:37:44.765Z → 2026-08-11T09:39:30.193Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 133.0 | 316.8 |
| q01-tenant-revenue | pintail | ok | 437.7 | 572.5 |
| q02-customer-history | mysql | ok | 7.8 | 87.4 |
| q02-customer-history | pintail | ok | 771.7 | 870.4 |
| q03-fulfillment-backlog | mysql | ok | 12.2 | 24.0 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.5 | 20.1 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 123.5 | 352.4 |
| q05-payment-failures | pintail | ok | 5.2 | 410.5 |
| q06-refund-rate | mysql | ok | 1122.8 | 1123.0 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1159.8 | 1169.5 |
| q07-product-performance | pintail | ok | 1281.3 | 1322.8 |
| q08-regional-cohorts | mysql | ok | 404.4 | 503.9 |
| q08-regional-cohorts | pintail | ok | 803.6 | 875.4 |
| q09-order-lifecycle | mysql | ok | 301.6 | 332.4 |
| q09-order-lifecycle | pintail | ok | 691.0 | 693.1 |
| q10-wide-operational-join | mysql | ok | 658.0 | 742.8 |
| q10-wide-operational-join | pintail | ok | 1220.3 | 1520.5 |
