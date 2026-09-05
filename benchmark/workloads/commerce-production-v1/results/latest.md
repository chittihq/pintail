# commerce-production-v1 — ci profile

Run: 2026-09-05T19:03:30.327Z → 2026-09-05T19:05:26.423Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 198.8 | 293.0 |
| q01-tenant-revenue | pintail | ok | 3.8 | 168.4 |
| q02-customer-history | mysql | ok | 8.4 | 47.8 |
| q02-customer-history | pintail | ok | 4.9 | 362.6 |
| q03-fulfillment-backlog | mysql | ok | 8.0 | 9.4 |
| q03-fulfillment-backlog | pintail | ok | 3.6 | 73.9 |
| q04-inventory-risk | mysql | ok | 8.2 | 28.9 |
| q04-inventory-risk | pintail | ok | 7.8 | 146.0 |
| q05-payment-failures | mysql | ok | 107.4 | 138.7 |
| q05-payment-failures | pintail | ok | 4.9 | 320.5 |
| q06-refund-rate | mysql | ok | 939.3 | 1134.7 |
| q06-refund-rate | pintail | ok | 11.6 | 1076.6 |
| q07-product-performance | mysql | ok | 879.9 | 888.5 |
| q07-product-performance | pintail | ok | 51.9 | 938.5 |
| q08-regional-cohorts | mysql | ok | 434.4 | 624.2 |
| q08-regional-cohorts | pintail | ok | 624.7 | 657.0 |
| q09-order-lifecycle | mysql | ok | 318.7 | 612.4 |
| q09-order-lifecycle | pintail | ok | 498.6 | 527.9 |
| q10-wide-operational-join | mysql | ok | 496.8 | 608.3 |
| q10-wide-operational-join | pintail | ok | 5.0 | 2030.7 |
| q11-dormant-customers | mysql | ok | 14.6 | 36.8 |
| q11-dormant-customers | pintail | ok | 649.3 | 861.2 |
| q12-per-customer-revenue | mysql | ok | 26.9 | 386.8 |
| q12-per-customer-revenue | pintail | ok | 4.7 | 83.3 |
