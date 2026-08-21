# TPC-H-derived correctness workload — smoke (scale 0.0005)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 18ms | 76ms | 6 |
| q03-shipping-priority | join-topn | ok | 10ms | 24ms | 2 |
| q05-local-supplier-volume | join-wide | ok | 12ms | 24ms | 2 |
| q10-returned-item-reporting | join-high-cardinality | ok | 8ms | 11ms | 13 |
