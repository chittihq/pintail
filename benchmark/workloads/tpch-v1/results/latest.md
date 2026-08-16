# TPC-H-derived correctness workload — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 585ms | 217ms | 6 |
| q03-shipping-priority | join-topn | ok | 20ms | 54ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 82ms | 61ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 11ms | 40ms | 20 |
