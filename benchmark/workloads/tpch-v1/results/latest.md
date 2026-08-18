# TPC-H-derived correctness workload — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 287ms | 187ms | 6 |
| q03-shipping-priority | join-topn | ok | 25ms | 59ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 83ms | 55ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 13ms | 46ms | 20 |
