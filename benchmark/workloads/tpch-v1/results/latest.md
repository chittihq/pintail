# TPC-H-derived correctness workload — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 142ms | 192ms | 6 |
| q03-shipping-priority | join-topn | ok | 23ms | 53ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 27ms | 59ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 20ms | 44ms | 20 |
