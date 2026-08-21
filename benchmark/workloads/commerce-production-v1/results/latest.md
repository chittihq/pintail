# commerce-production-v1 — ci profile

Run: 2026-08-21T04:37:33.813Z → 2026-08-21T04:46:38.657Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 141.4 | 345.5 |
| q01-tenant-revenue | pintail | ok | 700.6 | 770.3 |
| q02-customer-history | mysql | ok | 13.7 | 15.6 |
| q02-customer-history | pintail | ok | 1890.1 | 1929.3 |
| q03-fulfillment-backlog | mysql | ok | 10.2 | 33.2 |
| q03-fulfillment-backlog | pintail | ok | 1094.7 | 1166.8 |
| q04-inventory-risk | mysql | ok | 15.0 | 155.0 |
| q04-inventory-risk | pintail | ok | 3190.5 | 3455.4 |
| q05-payment-failures | mysql | ok | 122.5 | 238.2 |
| q05-payment-failures | pintail | ok | 20.4 | 1613.0 |
| q06-refund-rate | mysql | ok | 906.5 | 975.5 |
| q06-refund-rate | pintail | ok | 5957.3 | 8746.8 |
| q07-product-performance | mysql | ok | 974.9 | 1037.4 |
| q07-product-performance | pintail | ok | 11481.1 | 17523.8 |
| q08-regional-cohorts | mysql | ok | 462.1 | 496.7 |
| q08-regional-cohorts | pintail | ok | 3266.0 | 3575.2 |
| q09-order-lifecycle | mysql | ok | 275.0 | 348.9 |
| q09-order-lifecycle | pintail | ok | 1225.1 | 1299.1 |
| q10-wide-operational-join | mysql | ok | 650.1 | 767.9 |
| q10-wide-operational-join | pintail | ok | 2258.2 | 3190.7 |
| q11-dormant-customers | mysql | ok | 15.7 | 39.2 |
| q11-dormant-customers | pintail | ok | 1542.3 | 3527.8 |
| q12-per-customer-revenue | mysql | ok | 123.1 | 184.1 |
| q12-per-customer-revenue | pintail | ok | 565.5 | 568.3 |
