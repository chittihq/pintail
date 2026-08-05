# commerce-production-v1 — ci profile

Run: 2026-08-05T07:42:31.959Z → 2026-08-05T09:01:36.559Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 260.7 | 993.3 |
| q01-tenant-revenue | pintail | ok | 416.2 | 511.4 |
| q02-customer-history | mysql | ok | 159.3 | 235.2 |
| q02-customer-history | pintail | ok | 656.8 | 785.9 |
| q03-fulfillment-backlog | mysql | ok | 174.3 | 252.6 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 158.7 | 180.4 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 334.4 | 436.4 |
| q05-payment-failures | pintail | ok | 3.5 | 274.9 |
| q06-refund-rate | mysql | ok | 2111.5 | 2162.5 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 2515.4 | 2581.0 |
| q07-product-performance | pintail | ok | 1079.1 | 1175.0 |
| q08-regional-cohorts | mysql | ok | 815.8 | 1620.9 |
| q08-regional-cohorts | pintail | ok | 618.7 | 657.3 |
| q09-order-lifecycle | mysql | ok | 634.3 | 674.5 |
| q09-order-lifecycle | pintail | ok | 621.0 | 629.7 |
| q10-wide-operational-join | mysql | ok | 1465.7 | 1853.4 |
| q10-wide-operational-join | pintail | ok | 944.3 | 1459.1 |
