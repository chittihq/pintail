# commerce-production-v1 — ci profile

Run: 2026-08-15T06:23:09.886Z → 2026-08-15T06:25:55.509Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 147.9 | 337.9 |
| q01-tenant-revenue | pintail | ok | 489.3 | 715.1 |
| q02-customer-history | mysql | ok | 24.2 | 39.3 |
| q02-customer-history | pintail | ok | 936.8 | 1023.4 |
| q03-fulfillment-backlog | mysql | ok | 10.4 | 73.2 |
| q03-fulfillment-backlog | pintail | ok | 408.6 | 422.4 |
| q04-inventory-risk | mysql | ok | 12.1 | 168.2 |
| q04-inventory-risk | pintail | ok | 400.2 | 487.1 |
| q05-payment-failures | mysql | ok | 171.8 | 219.3 |
| q05-payment-failures | pintail | ok | 4.6 | 334.2 |
| q06-refund-rate | mysql | ok | 959.0 | 963.9 |
| q06-refund-rate | pintail | ok | 1225.3 | 1627.1 |
| q07-product-performance | mysql | ok | 946.7 | 971.1 |
| q07-product-performance | pintail | ok | 1197.1 | 1225.7 |
| q08-regional-cohorts | mysql | ok | 574.7 | 623.3 |
| q08-regional-cohorts | pintail | ok | 848.6 | 895.2 |
| q09-order-lifecycle | mysql | ok | 277.9 | 280.8 |
| q09-order-lifecycle | pintail | ok | 654.2 | 762.4 |
| q10-wide-operational-join | mysql | ok | 620.0 | 649.0 |
| q10-wide-operational-join | pintail | ok | 1824.1 | 1891.2 |
| q11-dormant-customers | mysql | ok | 19.7 | 35.7 |
| q11-dormant-customers | pintail | ok | 1016.0 | 1242.5 |
| q12-per-customer-revenue | mysql | ok | 14.1 | 85.8 |
| q12-per-customer-revenue | pintail | ok | 411.4 | 551.1 |
