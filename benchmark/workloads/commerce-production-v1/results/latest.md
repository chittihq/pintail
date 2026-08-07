# commerce-production-v1 — ci profile

Run: 2026-08-07T05:43:57.135Z → 2026-08-07T05:48:31.195Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 63.2 | 427.5 |
| q01-tenant-revenue | pintail | ok | 409.1 | 490.3 |
| q02-customer-history | mysql | ok | 7.4 | 144.2 |
| q02-customer-history | pintail | ok | 609.6 | 671.2 |
| q03-fulfillment-backlog | mysql | ok | 10.9 | 165.9 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.1 | 11.1 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 128.4 | 254.4 |
| q05-payment-failures | pintail | ok | 4.2 | 305.4 |
| q06-refund-rate | mysql | ok | 1065.5 | 1083.2 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1062.6 | 1071.0 |
| q07-product-performance | pintail | ok | 1060.4 | 1117.5 |
| q08-regional-cohorts | mysql | ok | 516.1 | 565.0 |
| q08-regional-cohorts | pintail | ok | 716.3 | 737.8 |
| q09-order-lifecycle | mysql | ok | 269.0 | 304.2 |
| q09-order-lifecycle | pintail | ok | 622.1 | 631.3 |
| q10-wide-operational-join | mysql | ok | 578.1 | 757.6 |
| q10-wide-operational-join | pintail | ok | 882.8 | 1360.7 |
