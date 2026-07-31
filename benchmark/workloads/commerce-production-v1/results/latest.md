# commerce-production-v1 — smoke profile

Run: 2026-07-31T05:30:13.640Z → 2026-07-31T05:30:54.319Z. Engines: mysql. Scale: 0.0001.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 203.1 | 211.2 |
| q02-customer-history | mysql | ok | 163.1 | 295.4 |
| q03-fulfillment-backlog | mysql | ok | 159.4 | 188.5 |
| q04-inventory-risk | mysql | ok | 180.8 | 214.5 |
| q05-payment-failures | mysql | ok | 177.6 | 188.9 |
| q06-refund-rate | mysql | ok | 176.3 | 214.4 |
| q07-product-performance | mysql | ok | 179.9 | 195.2 |
| q08-regional-cohorts | mysql | ok | 206.7 | 220.9 |
| q09-order-lifecycle | mysql | ok | 204.6 | 243.3 |
| q10-wide-operational-join | mysql | ok | 173.2 | 175.2 |
