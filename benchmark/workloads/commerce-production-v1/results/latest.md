# commerce-production-v1 — ci profile

Run: 2026-08-04T15:06:59.790Z → 2026-08-04T15:10:38.991Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 343.9 | 946.6 |
| q01-tenant-revenue | pintail | ok | 412.8 | 456.9 |
| q02-customer-history | mysql | ok | 192.1 | 211.5 |
| q02-customer-history | pintail | ok | 645.7 | 767.0 |
| q03-fulfillment-backlog | mysql | ok | 212.4 | 242.9 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 162.7 | 197.5 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 321.9 | 407.6 |
| q05-payment-failures | pintail | ok | 3.4 | 286.1 |
| q06-refund-rate | mysql | ok | 2066.5 | 2161.4 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2426.1 | 2438.8 |
| q07-product-performance | pintail | ok | 1075.0 | 1131.3 |
| q08-regional-cohorts | mysql | ok | 841.7 | 1005.4 |
| q08-regional-cohorts | pintail | ok | 647.8 | 668.2 |
| q09-order-lifecycle | mysql | ok | 615.1 | 622.0 |
| q09-order-lifecycle | pintail | ok | 629.0 | 630.4 |
| q10-wide-operational-join | mysql | ok | 1466.5 | 1971.6 |
| q10-wide-operational-join | pintail | ok | 922.0 | 1505.1 |
