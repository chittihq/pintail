# commerce-production-v1 — ci profile

Run: 2026-08-05T14:39:54.858Z → 2026-08-05T14:59:59.147Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 401.5 | 1014.3 |
| q01-tenant-revenue | pintail | ok | 395.1 | 481.1 |
| q02-customer-history | mysql | ok | 159.3 | 192.6 |
| q02-customer-history | pintail | ok | 670.7 | 673.5 |
| q03-fulfillment-backlog | mysql | ok | 176.5 | 182.9 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 163.0 | 528.5 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 523.5 | 738.5 |
| q05-payment-failures | pintail | ok | 3.2 | 293.7 |
| q06-refund-rate | mysql | ok | 1906.3 | 1987.5 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2474.0 | 2621.1 |
| q07-product-performance | pintail | ok | 982.9 | 1101.3 |
| q08-regional-cohorts | mysql | ok | 803.0 | 951.4 |
| q08-regional-cohorts | pintail | ok | 556.4 | 630.4 |
| q09-order-lifecycle | mysql | ok | 689.8 | 990.9 |
| q09-order-lifecycle | pintail | ok | 576.1 | 634.0 |
| q10-wide-operational-join | mysql | ok | 941.1 | 954.6 |
| q10-wide-operational-join | pintail | ok | 995.2 | 1376.6 |
