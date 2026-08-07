# commerce-production-v1 — ci profile

Run: 2026-08-07T14:33:50.999Z → 2026-08-07T14:37:19.007Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 140.2 | 297.3 |
| q01-tenant-revenue | pintail | ok | 407.8 | 466.8 |
| q02-customer-history | mysql | ok | 8.7 | 28.3 |
| q02-customer-history | pintail | ok | 650.9 | 682.4 |
| q03-fulfillment-backlog | mysql | ok | 9.7 | 19.2 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 10.3 | 12.7 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 124.5 | 135.7 |
| q05-payment-failures | pintail | ok | 3.9 | 304.7 |
| q06-refund-rate | mysql | ok | 935.5 | 1068.4 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 915.4 | 944.2 |
| q07-product-performance | pintail | ok | 1073.9 | 1104.7 |
| q08-regional-cohorts | mysql | ok | 412.1 | 689.7 |
| q08-regional-cohorts | pintail | ok | 742.0 | 789.5 |
| q09-order-lifecycle | mysql | ok | 403.7 | 406.2 |
| q09-order-lifecycle | pintail | ok | 623.9 | 638.5 |
| q10-wide-operational-join | mysql | ok | 555.6 | 576.8 |
| q10-wide-operational-join | pintail | ok | 894.3 | 1314.2 |
