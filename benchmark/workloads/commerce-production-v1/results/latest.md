# commerce-production-v1 — smoke profile

Run: 2026-08-13T09:26:27.001Z → 2026-08-13T10:03:04.251Z. Engines: mysql, pintail. Scale: 0.0001.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 19.6 | 155.8 |
| q01-tenant-revenue | pintail | ok | 26.5 | 43.2 |
| q02-customer-history | mysql | ok | 65.9 | 163.2 |
| q02-customer-history | pintail | ok | 12.2 | 13.4 |
| q03-fulfillment-backlog | mysql | ok | 27.9 | 155.3 |
| q03-fulfillment-backlog | pintail | ok | 8.2 | 10.9 |
| q04-inventory-risk | mysql | ok | 15.8 | 140.0 |
| q04-inventory-risk | pintail | ok | 125.6 | 140.3 |
| q05-payment-failures | mysql | ok | 17.1 | 21.5 |
| q05-payment-failures | pintail | ok | 4.9 | 9.7 |
| q06-refund-rate | mysql | ok | 17.3 | 109.6 |
| q06-refund-rate | pintail | ok | 14.8 | 16.2 |
| q07-product-performance | mysql | ok | 21.6 | 22.9 |
| q07-product-performance | pintail | ok | 18.2 | 21.0 |
| q08-regional-cohorts | mysql | ok | 13.3 | 14.0 |
| q08-regional-cohorts | pintail | ok | 10.2 | 11.4 |
| q09-order-lifecycle | mysql | ok | 15.1 | 348.3 |
| q09-order-lifecycle | pintail | ok | 21.8 | 24.8 |
| q10-wide-operational-join | mysql | ok | 10.4 | 11.2 |
| q10-wide-operational-join | pintail | ok | 14.5 | 16.6 |
| q11-dormant-customers | mysql | ok | 91.6 | 141.8 |
| q11-dormant-customers | pintail | ok | 15.4 | 19.7 |
| q12-per-customer-revenue | mysql | ok | 95.1 | 373.9 |
| q12-per-customer-revenue | pintail | ok | 11.4 | 15.8 |

## Phase: warm

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 9.9 | 27.8 |
| q01-tenant-revenue | pintail | ok | 8.3 | 8.8 |
| q02-customer-history | mysql | ok | 8.5 | 93.0 |
| q02-customer-history | pintail | ok | 9.2 | 10.3 |
| q03-fulfillment-backlog | mysql | ok | 8.6 | 143.6 |
| q03-fulfillment-backlog | pintail | ok | 7.1 | 7.8 |
| q04-inventory-risk | mysql | ok | 9.3 | 148.4 |
| q04-inventory-risk | pintail | ok | 9.1 | 10.2 |
| q05-payment-failures | mysql | ok | 14.2 | 38.1 |
| q05-payment-failures | pintail | ok | 4.0 | 6.3 |
| q06-refund-rate | mysql | ok | 14.8 | 161.3 |
| q06-refund-rate | pintail | ok | 13.9 | 17.9 |
| q07-product-performance | mysql | ok | 23.6 | 400.2 |
| q07-product-performance | pintail | ok | 19.0 | 21.4 |
| q08-regional-cohorts | mysql | ok | 12.5 | 155.1 |
| q08-regional-cohorts | pintail | ok | 10.8 | 18.0 |
| q09-order-lifecycle | mysql | ok | 11.5 | 383.6 |
| q09-order-lifecycle | pintail | ok | 10.5 | 11.8 |
| q10-wide-operational-join | mysql | ok | 13.5 | 138.1 |
| q10-wide-operational-join | pintail | ok | 12.9 | 18.5 |
| q11-dormant-customers | mysql | ok | 9.5 | 102.6 |
| q11-dormant-customers | pintail | ok | 14.5 | 21.4 |
| q12-per-customer-revenue | mysql | ok | 11.8 | 23.2 |
| q12-per-customer-revenue | pintail | ok | 7.1 | 7.5 |

## Phase: mixed-light

```json
{
  "mutationStats": {
    "inserts": 7524,
    "updates": 2319,
    "deletes": 58,
    "transactions": 2902,
    "cascadeDeletes": 5,
    "errors": 0
  },
  "readerPasses": 331,
  "sourceToVisibleLagMs": 3942,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 331,
      "medianMs": 18.56995800000732,
      "p95Ms": 39.70045800000662,
      "maxMs": 169.73454199999105
    },
    {
      "id": "q02-customer-history",
      "passes": 331,
      "medianMs": 22.70745800001896,
      "p95Ms": 38.613707999989856,
      "maxMs": 97.15341699996497
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 331,
      "medianMs": 17.74845899999491,
      "p95Ms": 36.1070830000026,
      "maxMs": 79.60195899999235
    },
    {
      "id": "q04-inventory-risk",
      "passes": 331,
      "medianMs": 20.609207999994396,
      "p95Ms": 38.81637499999488,
      "maxMs": 70.24037499999395
    },
    {
      "id": "q05-payment-failures",
      "passes": 331,
      "medianMs": 12.254541000002064,
      "p95Ms": 22.72962500003632,
      "maxMs": 58.40037500002654
    },
    {
      "id": "q06-refund-rate",
      "passes": 331,
      "medianMs": 30.242082999997365,
      "p95Ms": 49.5187919999953,
      "maxMs": 152.36675000000105
    },
    {
      "id": "q07-product-performance",
      "passes": 331,
      "medianMs": 31.881374999997206,
      "p95Ms": 47.40729099998134,
      "maxMs": 210.94445799999812
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 331,
      "medianMs": 20.92666699999245,
      "p95Ms": 35.17687500000466,
      "maxMs": 70.89091600000393
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 331,
      "medianMs": 19.85687499999767,
      "p95Ms": 34.56254099996295,
      "maxMs": 70.41654100001324
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 331,
      "medianMs": 26.65450000000419,
      "p95Ms": 53.41770799999358,
      "maxMs": 516.0008329999982
    },
    {
      "id": "q11-dormant-customers",
      "passes": 331,
      "medianMs": 26.59633400000166,
      "p95Ms": 45.49199999999837,
      "maxMs": 162.817584000004
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 331,
      "medianMs": 16.56445900001563,
      "p95Ms": 29.147916999994777,
      "maxMs": 132.86600000000908
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4472,
      "pintail": 4493
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "16586",
      "pintail": "16683"
    }
  ]
}
```

## Phase: mixed

```json
{
  "mutationStats": {
    "inserts": 90133,
    "updates": 27534,
    "deletes": 475,
    "transactions": 34773,
    "cascadeDeletes": 66,
    "errors": 140
  },
  "readerPasses": 682,
  "sourceToVisibleLagMs": 4720,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 682,
      "medianMs": 114.45616699999664,
      "p95Ms": 255.19020899990574,
      "maxMs": 305.1841659999918
    },
    {
      "id": "q02-customer-history",
      "passes": 682,
      "medianMs": 136.57962500001304,
      "p95Ms": 289.4764580000192,
      "maxMs": 410.9923749999143
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 682,
      "medianMs": 115.61095800006296,
      "p95Ms": 259.67283300007693,
      "maxMs": 938.2835419999901
    },
    {
      "id": "q04-inventory-risk",
      "passes": 682,
      "medianMs": 132.5870420000283,
      "p95Ms": 294.88950000004843,
      "maxMs": 514.8042090001982
    },
    {
      "id": "q05-payment-failures",
      "passes": 682,
      "medianMs": 93.35654199996497,
      "p95Ms": 214.6977079999633,
      "maxMs": 757.8643340000417
    },
    {
      "id": "q06-refund-rate",
      "passes": 682,
      "medianMs": 177.52045900002122,
      "p95Ms": 398.25350000010803,
      "maxMs": 466.4098750001285
    },
    {
      "id": "q07-product-performance",
      "passes": 682,
      "medianMs": 145.22412500006612,
      "p95Ms": 307.3108749999665,
      "maxMs": 391.96762500004843
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 682,
      "medianMs": 136.75537500006612,
      "p95Ms": 306.4380419999361,
      "maxMs": 432.8028749998193
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 682,
      "medianMs": 114.75729199999478,
      "p95Ms": 250.98949999990873,
      "maxMs": 333.1012499999488
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 682,
      "medianMs": 137.81366700003855,
      "p95Ms": 301.38066699984483,
      "maxMs": 566.4247910003178
    },
    {
      "id": "q11-dormant-customers",
      "passes": 682,
      "medianMs": 151.7082909999881,
      "p95Ms": 336.2888750000857,
      "maxMs": 636.3828750000102
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 682,
      "medianMs": 115.71525000000838,
      "p95Ms": 257.99762499984354,
      "maxMs": 424.64200000005076
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4310,
      "pintail": 4335
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "15966",
      "pintail": "16060"
    }
  ]
}
```

## Phase: post-compaction

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 35.8 | 383.3 |
| q01-tenant-revenue | pintail | ok | 269.5 | 272.1 |
| q02-customer-history | mysql | ok | 14.6 | 22.4 |
| q02-customer-history | pintail | ok | 295.0 | 338.8 |
| q03-fulfillment-backlog | mysql | ok | 17.7 | 275.4 |
| q03-fulfillment-backlog | pintail | ok | 268.7 | 313.9 |
| q04-inventory-risk | mysql | ok | 13.3 | 1367.1 |
| q04-inventory-risk | pintail | ok | 291.0 | 308.9 |
| q05-payment-failures | mysql | ok | 13.5 | 289.7 |
| q05-payment-failures | pintail | ok | 216.5 | 232.9 |
| q06-refund-rate | mysql | ok | 163.8 | 198.2 |
| q06-refund-rate | pintail | ok | 398.1 | 479.3 |
| q07-product-performance | mysql | ok | 25.1 | 35.0 |
| q07-product-performance | pintail | ok | 322.4 | 351.6 |
| q08-regional-cohorts | mysql | ok | 69.1 | 1118.2 |
| q08-regional-cohorts | pintail | ok | 315.3 | 376.3 |
| q09-order-lifecycle | mysql | ok | 16.2 | 22.1 |
| q09-order-lifecycle | pintail | ok | 255.5 | 269.3 |
| q10-wide-operational-join | mysql | ok | 13.7 | 290.3 |
| q10-wide-operational-join | pintail | ok | 317.5 | 321.9 |
| q11-dormant-customers | mysql | ok | 14.5 | 289.7 |
| q11-dormant-customers | pintail | ok | 360.7 | 395.7 |
| q12-per-customer-revenue | mysql | ok | 12.3 | 277.7 |
| q12-per-customer-revenue | pintail | ok | 277.3 | 284.3 |

## Phase: restart

```json
{
  "fingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4310,
      "pintail": 4335
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "15966",
      "pintail": "16060"
    }
  ]
}
```
