# TPC-H — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 218ms | 372ms | 6 |
| q03-shipping-priority | join-topn | ok | 40ms | 99ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 58ms | 114ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 44ms | 47ms | 20 |
