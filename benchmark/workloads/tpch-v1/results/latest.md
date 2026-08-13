# TPC-H — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 96ms | 143ms | 6 |
| q03-shipping-priority | join-topn | error | 16ms | -ms | - |
| q05-local-supplier-volume | join-wide | error | 15ms | -ms | - |
| q10-returned-item-reporting | join-high-cardinality | error | 13ms | -ms | - |
