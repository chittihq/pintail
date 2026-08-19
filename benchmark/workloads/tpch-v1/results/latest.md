# TPC-H-derived correctness workload — smoke (scale 0.0005)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 27ms | 40ms | 6 |
| q03-shipping-priority | join-topn | ok | 9ms | 9ms | 2 |
| q05-local-supplier-volume | join-wide | ok | 8ms | 7ms | 2 |
| q10-returned-item-reporting | join-high-cardinality | ok | 10ms | 6ms | 13 |
