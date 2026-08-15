# TPC-H — ci (scale 0.01)

| query | class | status | mysql | pintail | rows |
|---|---|---|---|---|---|
| q01-pricing-summary | scan-aggregate | ok | 105ms | 192ms | 6 |
| q03-shipping-priority | join-topn | ok | 41ms | 51ms | 10 |
| q05-local-supplier-volume | join-wide | ok | 169ms | 65ms | 5 |
| q10-returned-item-reporting | join-high-cardinality | ok | 26ms | 47ms | 20 |
