# TPC-H-derived correctness workload — smoke (scale 0.0005)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 62ms | 67ms | 6 |
| q03-shipping-priority | join-topn | ok | 54ms | 9ms | 2 |
| q05-local-supplier-volume | join-wide | ok | 55ms | 10ms | 2 |
| q10-returned-item-reporting | join-high-cardinality | ok | 142ms | 18ms | 13 |
