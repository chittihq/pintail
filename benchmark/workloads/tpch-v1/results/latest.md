# TPC-H-derived correctness workload — sf1 (scale 1)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 8247ms | 9139ms | 6 |
| q03-shipping-priority | join-topn | ok | 930ms | 5418ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 780ms | 47892ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 401ms | 3952ms | 20 |
