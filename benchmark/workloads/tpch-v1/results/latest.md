# TPC-H-derived correctness workload — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 142ms | 192ms | 6 |
| q03-shipping-priority | join-topn | ok | 37ms | 50ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 37ms | 51ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 39ms | 43ms | 20 |
