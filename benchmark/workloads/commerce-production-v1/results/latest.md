# commerce-production-v1 — smoke profile

Run: 2026-07-31T05:13:51.178Z → 2026-07-31T05:14:36.193Z. Engines: mysql. Scale: 0.0001.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 173.8 | 181.1 |
| q02-customer-history | mysql | ok | 168.6 | 178.8 |
| q03-fulfillment-backlog | mysql | ok | 199.3 | 252.9 |
| q04-inventory-risk | mysql | ok | 162.1 | 285.0 |
| q05-payment-failures | mysql | ok | 167.1 | 187.6 |
| q06-refund-rate | mysql | ok | 184.3 | 205.6 |
| q07-product-performance | mysql | ok | 199.3 | 211.7 |
| q08-regional-cohorts | mysql | ok | 182.6 | 235.4 |
| q09-order-lifecycle | mysql | ok | 181.8 | 192.8 |
| q10-wide-operational-join | mysql | ok | 174.5 | 183.5 |

## Phase: warm

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 171.5 | 347.7 |
| q02-customer-history | mysql | ok | 175.2 | 259.8 |
| q03-fulfillment-backlog | mysql | ok | 199.9 | 311.5 |
| q04-inventory-risk | mysql | ok | 185.7 | 262.0 |
| q05-payment-failures | mysql | ok | 214.6 | 390.0 |
| q06-refund-rate | mysql | ok | 181.6 | 222.1 |
| q07-product-performance | mysql | ok | 191.3 | 278.1 |
| q08-regional-cohorts | mysql | ok | 194.8 | 302.6 |
| q09-order-lifecycle | mysql | ok | 190.6 | 253.3 |
| q10-wide-operational-join | mysql | ok | 189.6 | 250.7 |
