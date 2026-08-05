# commerce-production-v1 — ci profile

Run: 2026-08-04T21:02:19.515Z → 2026-08-04T22:04:05.349Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 353.6 | 906.6 |
| q01-tenant-revenue | pintail | ok | 394.6 | 460.4 |
| q02-customer-history | mysql | ok | 208.0 | 228.3 |
| q02-customer-history | pintail | ok | 602.6 | 742.2 |
| q03-fulfillment-backlog | mysql | ok | 161.2 | 188.0 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 246.3 | 618.0 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 433.4 | 463.5 |
| q05-payment-failures | pintail | ok | 3.2 | 278.5 |
| q06-refund-rate | mysql | ok | 2202.2 | 2235.0 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2513.7 | 2924.1 |
| q07-product-performance | pintail | ok | 985.2 | 1068.8 |
| q08-regional-cohorts | mysql | ok | 790.4 | 1018.3 |
| q08-regional-cohorts | pintail | ok | 561.5 | 600.0 |
| q09-order-lifecycle | mysql | ok | 613.7 | 616.7 |
| q09-order-lifecycle | pintail | ok | 605.3 | 610.7 |
| q10-wide-operational-join | mysql | ok | 1512.2 | 1815.9 |
| q10-wide-operational-join | pintail | ok | 899.2 | 1357.2 |
