# commerce-production-v1 — ci profile

Run: 2026-08-10T05:44:37.439Z → 2026-08-10T05:48:34.681Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 103.6 | 357.6 |
| q01-tenant-revenue | pintail | ok | 436.3 | 506.5 |
| q02-customer-history | mysql | ok | 32.1 | 111.3 |
| q02-customer-history | pintail | ok | 622.5 | 754.1 |
| q03-fulfillment-backlog | mysql | ok | 9.8 | 12.1 |
| q03-fulfillment-backlog | pintail | error | — | — |
| q04-inventory-risk | mysql | ok | 21.6 | 122.3 |
| q04-inventory-risk | pintail | error | — | — |
| q05-payment-failures | mysql | ok | 98.1 | 138.9 |
| q05-payment-failures | pintail | ok | 3.9 | 311.2 |
| q06-refund-rate | mysql | ok | 965.4 | 1098.8 |
| q06-refund-rate | pintail | error | — | — |
| q07-product-performance | mysql | ok | 991.8 | 1191.1 |
| q07-product-performance | pintail | ok | 1149.9 | 1164.5 |
| q08-regional-cohorts | mysql | ok | 406.3 | 461.9 |
| q08-regional-cohorts | pintail | ok | 769.6 | 836.9 |
| q09-order-lifecycle | mysql | ok | 269.7 | 270.6 |
| q09-order-lifecycle | pintail | ok | 629.9 | 632.3 |
| q10-wide-operational-join | mysql | ok | 533.2 | 692.6 |
| q10-wide-operational-join | pintail | ok | 1080.4 | 1509.4 |
