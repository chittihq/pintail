# TPC-H — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 98ms | 112ms | 6 |
| q03-shipping-priority | join-topn | ok | 15ms | 58ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 12ms | 53ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 191ms | 58ms | 20 |
