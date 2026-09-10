# TPC-H-derived correctness workload — smoke (scale 0.0005)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 6ms | 15ms | 6 |
| q03-shipping-priority | join-topn | ok | 1ms | 5ms | 2 |
| q05-local-supplier-volume | join-wide | ok | 2ms | 6ms | 2 |
| q10-returned-item-reporting | join-high-cardinality | ok | 2ms | 4ms | 13 |
