# commerce-production-v1 — ci profile

Run: 2026-08-17T18:43:15.756Z → 2026-08-17T18:46:16.262Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 139.4 | 335.0 |
| q01-tenant-revenue | pintail | ok | 465.6 | 585.5 |
| q02-customer-history | mysql | ok | 7.9 | 79.8 |
| q02-customer-history | pintail | ok | 692.4 | 817.7 |
| q03-fulfillment-backlog | mysql | ok | 7.8 | 159.5 |
| q03-fulfillment-backlog | pintail | ok | 502.8 | 540.2 |
| q04-inventory-risk | mysql | ok | 9.9 | 20.2 |
| q04-inventory-risk | pintail | ok | 433.6 | 501.3 |
| q05-payment-failures | mysql | ok | 155.2 | 243.2 |
| q05-payment-failures | pintail | ok | 6.8 | 336.3 |
| q06-refund-rate | mysql | ok | 992.1 | 1006.4 |
| q06-refund-rate | pintail | ok | 1056.8 | 1444.4 |
| q07-product-performance | mysql | ok | 1215.2 | 1221.2 |
| q07-product-performance | pintail | ok | 1216.9 | 1334.2 |
| q08-regional-cohorts | mysql | ok | 461.8 | 536.4 |
| q08-regional-cohorts | pintail | ok | 982.4 | 1066.3 |
| q09-order-lifecycle | mysql | ok | 331.5 | 622.4 |
| q09-order-lifecycle | pintail | ok | 821.3 | 886.4 |
| q10-wide-operational-join | mysql | ok | 569.7 | 756.4 |
| q10-wide-operational-join | pintail | ok | 1923.0 | 1966.4 |
| q11-dormant-customers | mysql | ok | 25.1 | 154.7 |
| q11-dormant-customers | pintail | ok | 1324.7 | 1465.7 |
| q12-per-customer-revenue | mysql | ok | 25.9 | 64.9 |
| q12-per-customer-revenue | pintail | ok | 541.5 | 668.7 |
