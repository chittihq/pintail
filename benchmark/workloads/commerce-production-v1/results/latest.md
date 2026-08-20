# commerce-production-v1 — ci profile

Run: 2026-08-20T14:16:56.061Z → 2026-08-20T14:18:59.521Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 54.7 | 305.3 |
| q01-tenant-revenue | pintail | ok | 456.6 | 576.7 |
| q02-customer-history | mysql | ok | 4.9 | 6.4 |
| q02-customer-history | pintail | ok | 666.3 | 874.5 |
| q03-fulfillment-backlog | mysql | ok | 6.0 | 10.1 |
| q03-fulfillment-backlog | pintail | ok | 374.2 | 375.2 |
| q04-inventory-risk | mysql | ok | 5.2 | 6.8 |
| q04-inventory-risk | pintail | ok | 421.8 | 428.6 |
| q05-payment-failures | mysql | ok | 95.2 | 146.6 |
| q05-payment-failures | pintail | ok | 5.0 | 360.5 |
| q06-refund-rate | mysql | ok | 985.5 | 985.7 |
| q06-refund-rate | pintail | ok | 1131.8 | 1231.6 |
| q07-product-performance | mysql | ok | 953.7 | 967.7 |
| q07-product-performance | pintail | ok | 1220.1 | 1278.1 |
| q08-regional-cohorts | mysql | ok | 460.3 | 512.7 |
| q08-regional-cohorts | pintail | ok | 937.7 | 947.6 |
| q09-order-lifecycle | mysql | ok | 272.9 | 275.9 |
| q09-order-lifecycle | pintail | ok | 728.3 | 764.8 |
| q10-wide-operational-join | mysql | ok | 500.4 | 630.2 |
| q10-wide-operational-join | pintail | ok | 1286.9 | 1639.8 |
| q11-dormant-customers | mysql | ok | 11.3 | 32.4 |
| q11-dormant-customers | pintail | ok | 1352.9 | 1368.4 |
| q12-per-customer-revenue | mysql | ok | 14.4 | 24.3 |
| q12-per-customer-revenue | pintail | ok | 387.9 | 482.3 |
