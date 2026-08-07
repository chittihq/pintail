# commerce-production-v1 — ci profile

Run: 2026-08-07T17:30:38.415Z → 2026-08-07T17:32:03.084Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 131.7 | 356.0 |
| q01-tenant-revenue | pintail | ok | 420.6 | 466.7 |
| q02-customer-history | mysql | ok | 9.0 | 95.0 |
| q02-customer-history | pintail | ok | 626.4 | 742.8 |
| q03-fulfillment-backlog | mysql | ok | 15.1 | 96.1 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 10.1 | 11.9 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 99.2 | 148.3 |
| q05-payment-failures | pintail | ok | 4.9 | 353.5 |
| q06-refund-rate | mysql | ok | 1025.4 | 1034.7 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 982.0 | 1031.0 |
| q07-product-performance | pintail | ok | 1197.9 | 1213.4 |
| q08-regional-cohorts | mysql | ok | 437.4 | 469.6 |
| q08-regional-cohorts | pintail | ok | 854.6 | 857.6 |
| q09-order-lifecycle | mysql | ok | 263.7 | 265.1 |
| q09-order-lifecycle | pintail | ok | 621.1 | 670.3 |
| q10-wide-operational-join | mysql | ok | 529.2 | 637.2 |
| q10-wide-operational-join | pintail | ok | 1137.1 | 1372.2 |
