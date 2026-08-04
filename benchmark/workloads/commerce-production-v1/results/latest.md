# commerce-production-v1 — ci profile

Run: 2026-08-04T10:52:18.335Z → 2026-08-04T10:58:56.234Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 328.7 | 863.0 |
| q01-tenant-revenue | pintail | ok | 378.7 | 484.8 |
| q02-customer-history | mysql | ok | 159.9 | 164.7 |
| q02-customer-history | pintail | ok | 590.1 | 736.1 |
| q03-fulfillment-backlog | mysql | ok | 152.6 | 176.0 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 155.6 | 158.7 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 294.1 | 414.8 |
| q05-payment-failures | pintail | ok | 3.3 | 286.7 |
| q06-refund-rate | mysql | ok | 2187.0 | 2268.4 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2490.6 | 2595.5 |
| q07-product-performance | pintail | ok | 989.2 | 1042.8 |
| q08-regional-cohorts | mysql | ok | 877.0 | 986.5 |
| q08-regional-cohorts | pintail | ok | 577.2 | 663.3 |
| q09-order-lifecycle | mysql | ok | 604.0 | 611.6 |
| q09-order-lifecycle | pintail | ok | 589.2 | 615.2 |
| q10-wide-operational-join | mysql | ok | 1484.1 | 1936.9 |
| q10-wide-operational-join | pintail | ok | 927.4 | 1232.5 |
