# commerce-production-v1 — ci profile

Run: 2026-08-03T06:05:19.766Z → 2026-08-03T06:12:37.908Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 327.3 | 895.7 |
| q01-tenant-revenue | pintail | ok | 421.6 | 536.0 |
| q02-customer-history | mysql | ok | 148.3 | 154.9 |
| q02-customer-history | pintail | ok | 731.2 | 894.8 |
| q03-fulfillment-backlog | mysql | ok | 153.1 | 261.2 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 198.0 | 211.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 350.3 | 416.9 |
| q05-payment-failures | pintail | ok | 3.7 | 354.0 |
| q06-refund-rate | mysql | ok | 2194.3 | 2250.4 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2588.2 | 2602.3 |
| q07-product-performance | pintail | ok | 1072.8 | 1204.7 |
| q08-regional-cohorts | mysql | ok | 810.6 | 1085.6 |
| q08-regional-cohorts | pintail | ok | 659.5 | 672.6 |
| q09-order-lifecycle | mysql | ok | 611.6 | 641.6 |
| q09-order-lifecycle | pintail | ok | 651.1 | 670.3 |
| q10-wide-operational-join | mysql | ok | 1446.5 | 1872.4 |
| q10-wide-operational-join | pintail | ok | 988.7 | 1636.8 |
