# commerce-production-v1 — ci profile

Run: 2026-08-18T20:17:24.086Z → 2026-08-18T20:22:37.005Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 58.7 | 445.5 |
| q01-tenant-revenue | pintail | ok | 423.7 | 503.9 |
| q02-customer-history | mysql | ok | 9.3 | 10.4 |
| q02-customer-history | pintail | ok | 582.2 | 721.6 |
| q03-fulfillment-backlog | mysql | ok | 9.1 | 152.4 |
| q03-fulfillment-backlog | pintail | ok | 372.3 | 400.8 |
| q04-inventory-risk | mysql | ok | 11.3 | 133.6 |
| q04-inventory-risk | pintail | ok | 387.4 | 461.3 |
| q05-payment-failures | mysql | ok | 101.2 | 219.9 |
| q05-payment-failures | pintail | ok | 4.5 | 306.8 |
| q06-refund-rate | mysql | ok | 984.9 | 1051.0 |
| q06-refund-rate | pintail | ok | 1278.5 | 1672.5 |
| q07-product-performance | mysql | ok | 932.4 | 1127.9 |
| q07-product-performance | pintail | ok | 1274.7 | 1358.9 |
| q08-regional-cohorts | mysql | ok | 427.7 | 483.5 |
| q08-regional-cohorts | pintail | ok | 804.4 | 847.4 |
| q09-order-lifecycle | mysql | ok | 272.0 | 278.8 |
| q09-order-lifecycle | pintail | ok | 674.8 | 732.0 |
| q10-wide-operational-join | mysql | ok | 487.8 | 641.6 |
| q10-wide-operational-join | pintail | ok | 1342.1 | 1476.9 |
| q11-dormant-customers | mysql | ok | 14.4 | 43.4 |
| q11-dormant-customers | pintail | ok | 996.3 | 1222.5 |
| q12-per-customer-revenue | mysql | ok | 18.1 | 36.2 |
| q12-per-customer-revenue | pintail | ok | 353.5 | 362.7 |
