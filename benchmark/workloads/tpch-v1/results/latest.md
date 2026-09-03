# TPC-H-derived correctness workload — smoke (scale 0.0005)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 16ms | 47ms | 6 |
| q03-shipping-priority | join-topn | ok | 11ms | 8ms | 2 |
| q05-local-supplier-volume | join-wide | ok | 147ms | 15ms | 2 |
| q10-returned-item-reporting | join-high-cardinality | ok | 9ms | 9ms | 13 |
