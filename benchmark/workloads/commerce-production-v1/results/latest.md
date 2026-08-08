# commerce-production-v1 — ci profile

Run: 2026-08-08T10:34:01.845Z → 2026-08-08T10:39:30.133Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 129.4 | 398.2 |
| q01-tenant-revenue | pintail | ok | 563.9 | 691.4 |
| q02-customer-history | mysql | ok | 20.6 | 21.8 |
| q02-customer-history | pintail | ok | 882.3 | 984.4 |
| q03-fulfillment-backlog | mysql | ok | 30.8 | 105.6 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 43.3 | 214.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 172.9 | 238.5 |
| q05-payment-failures | pintail | ok | 5.3 | 453.3 |
| q06-refund-rate | mysql | ok | 1024.3 | 1027.0 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1055.5 | 1058.6 |
| q07-product-performance | pintail | ok | 1578.5 | 1622.5 |
| q08-regional-cohorts | mysql | ok | 459.8 | 474.2 |
| q08-regional-cohorts | pintail | ok | 1025.4 | 1097.4 |
| q09-order-lifecycle | mysql | ok | 298.0 | 304.7 |
| q09-order-lifecycle | pintail | ok | 856.7 | 864.1 |
| q10-wide-operational-join | mysql | ok | 614.7 | 772.1 |
| q10-wide-operational-join | pintail | ok | 1406.8 | 1686.0 |
