# commerce-production-v1 — ci profile

Run: 2026-08-08T07:57:22.318Z → 2026-08-08T08:02:03.153Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 97.9 | 362.2 |
| q01-tenant-revenue | pintail | ok | 480.6 | 511.9 |
| q02-customer-history | mysql | ok | 23.8 | 126.5 |
| q02-customer-history | pintail | ok | 648.9 | 853.6 |
| q03-fulfillment-backlog | mysql | ok | 47.0 | 61.1 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 56.8 | 60.6 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 155.7 | 189.4 |
| q05-payment-failures | pintail | ok | 4.7 | 347.4 |
| q06-refund-rate | mysql | ok | 1035.7 | 1041.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 1043.3 | 1061.9 |
| q07-product-performance | pintail | ok | 1194.1 | 1251.7 |
| q08-regional-cohorts | mysql | ok | 423.5 | 492.4 |
| q08-regional-cohorts | pintail | ok | 979.3 | 1205.4 |
| q09-order-lifecycle | mysql | ok | 294.8 | 342.7 |
| q09-order-lifecycle | pintail | ok | 663.2 | 673.8 |
| q10-wide-operational-join | mysql | ok | 589.1 | 740.9 |
| q10-wide-operational-join | pintail | ok | 1044.7 | 1447.3 |
