# commerce-production-v1 — ci profile

Run: 2026-08-07T23:34:53.401Z → 2026-08-07T23:38:19.305Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 195.7 | 289.2 |
| q01-tenant-revenue | pintail | ok | 404.9 | 486.1 |
| q02-customer-history | mysql | ok | 7.9 | 7.9 |
| q02-customer-history | pintail | ok | 613.6 | 738.7 |
| q03-fulfillment-backlog | mysql | ok | 13.0 | 17.9 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 9.9 | 20.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 204.6 | 231.5 |
| q05-payment-failures | pintail | ok | 4.5 | 298.2 |
| q06-refund-rate | mysql | ok | 935.2 | 937.2 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 938.9 | 1200.7 |
| q07-product-performance | pintail | ok | 1149.4 | 1195.7 |
| q08-regional-cohorts | mysql | ok | 402.5 | 489.5 |
| q08-regional-cohorts | pintail | ok | 715.3 | 764.5 |
| q09-order-lifecycle | mysql | ok | 341.3 | 415.2 |
| q09-order-lifecycle | pintail | ok | 627.9 | 650.0 |
| q10-wide-operational-join | mysql | ok | 474.9 | 727.3 |
| q10-wide-operational-join | pintail | ok | 940.5 | 1315.7 |
