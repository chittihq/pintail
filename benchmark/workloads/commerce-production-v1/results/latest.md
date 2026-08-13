# commerce-production-v1 — smoke profile

Run: 2026-08-13T08:44:24.487Z → 2026-08-13T09:21:22.893Z. Engines: mysql, pintail. Scale: 0.0001.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 9.7 | 13.2 |
| q01-tenant-revenue | pintail | ok | 12.8 | 35.8 |
| q02-customer-history | mysql | ok | 153.9 | 161.2 |
| q02-customer-history | pintail | ok | 11.0 | 31.7 |
| q03-fulfillment-backlog | mysql | ok | 8.4 | 15.3 |
| q03-fulfillment-backlog | pintail | ok | 8.1 | 11.4 |
| q04-inventory-risk | mysql | ok | 9.9 | 11.8 |
| q04-inventory-risk | pintail | ok | 9.7 | 14.6 |
| q05-payment-failures | mysql | ok | 14.2 | 101.7 |
| q05-payment-failures | pintail | ok | 5.8 | 13.4 |
| q06-refund-rate | mysql | ok | 16.0 | 34.5 |
| q06-refund-rate | pintail | ok | 16.2 | 21.4 |
| q07-product-performance | mysql | ok | 22.1 | 26.7 |
| q07-product-performance | pintail | ok | 17.9 | 20.1 |
| q08-regional-cohorts | mysql | ok | 11.6 | 12.2 |
| q08-regional-cohorts | pintail | ok | 9.4 | 10.6 |
| q09-order-lifecycle | mysql | ok | 10.3 | 289.8 |
| q09-order-lifecycle | pintail | ok | 16.9 | 24.5 |
| q10-wide-operational-join | mysql | ok | 11.1 | 13.9 |
| q10-wide-operational-join | pintail | ok | 13.4 | 18.0 |
| q11-dormant-customers | mysql | ok | 94.9 | 135.2 |
| q11-dormant-customers | pintail | ok | 20.8 | 26.3 |
| q12-per-customer-revenue | mysql | ok | 9.9 | 29.1 |
| q12-per-customer-revenue | pintail | ok | 6.9 | 8.3 |

## Phase: warm

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 9.3 | 141.6 |
| q01-tenant-revenue | pintail | ok | 8.5 | 9.8 |
| q02-customer-history | mysql | ok | 8.5 | 137.5 |
| q02-customer-history | pintail | ok | 9.7 | 10.4 |
| q03-fulfillment-backlog | mysql | ok | 10.6 | 175.8 |
| q03-fulfillment-backlog | pintail | ok | 8.0 | 9.9 |
| q04-inventory-risk | mysql | ok | 8.8 | 134.2 |
| q04-inventory-risk | pintail | ok | 7.8 | 9.5 |
| q05-payment-failures | mysql | ok | 20.3 | 100.0 |
| q05-payment-failures | pintail | ok | 4.1 | 4.9 |
| q06-refund-rate | mysql | ok | 14.2 | 146.2 |
| q06-refund-rate | pintail | ok | 12.8 | 15.5 |
| q07-product-performance | mysql | ok | 20.1 | 25.9 |
| q07-product-performance | pintail | ok | 17.5 | 17.7 |
| q08-regional-cohorts | mysql | ok | 15.3 | 107.9 |
| q08-regional-cohorts | pintail | ok | 9.7 | 12.5 |
| q09-order-lifecycle | mysql | ok | 11.8 | 26.2 |
| q09-order-lifecycle | pintail | ok | 10.6 | 14.6 |
| q10-wide-operational-join | mysql | ok | 9.9 | 142.1 |
| q10-wide-operational-join | pintail | ok | 11.3 | 15.4 |
| q11-dormant-customers | mysql | ok | 9.6 | 140.3 |
| q11-dormant-customers | pintail | ok | 13.9 | 15.9 |
| q12-per-customer-revenue | mysql | ok | 11.8 | 263.1 |
| q12-per-customer-revenue | pintail | ok | 6.7 | 8.7 |

## Phase: mixed-light

```json
{
  "mutationStats": {
    "inserts": 7083,
    "updates": 2190,
    "deletes": 55,
    "transactions": 2735,
    "cascadeDeletes": 4,
    "errors": 0
  },
  "readerPasses": 295,
  "sourceToVisibleLagMs": 707,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 295,
      "medianMs": 19.14299999998184,
      "p95Ms": 42.42833399999654,
      "maxMs": 110.87654100000509
    },
    {
      "id": "q02-customer-history",
      "passes": 295,
      "medianMs": 22.94945800001733,
      "p95Ms": 46.34887499996694,
      "maxMs": 100.98220899997978
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 295,
      "medianMs": 18.472208999999566,
      "p95Ms": 38.65620800000033,
      "maxMs": 165.6848750000354
    },
    {
      "id": "q04-inventory-risk",
      "passes": 295,
      "medianMs": 21.394625000015367,
      "p95Ms": 41.72629100002814,
      "maxMs": 63.42241699999431
    },
    {
      "id": "q05-payment-failures",
      "passes": 295,
      "medianMs": 12.257209000003058,
      "p95Ms": 35.0655839999672,
      "maxMs": 76.22729100001743
    },
    {
      "id": "q06-refund-rate",
      "passes": 295,
      "medianMs": 31.46287499999744,
      "p95Ms": 60.92041700001573,
      "maxMs": 111.77941699998337
    },
    {
      "id": "q07-product-performance",
      "passes": 295,
      "medianMs": 32.43466699999408,
      "p95Ms": 60.34637500002282,
      "maxMs": 80.58983300000546
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 295,
      "medianMs": 20.54008399997838,
      "p95Ms": 37.45112500002142,
      "maxMs": 87.24587499999325
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 295,
      "medianMs": 20.32445899999584,
      "p95Ms": 37.23354200000176,
      "maxMs": 161.96495799999684
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 295,
      "medianMs": 26.690333999998984,
      "p95Ms": 49.58258400001796,
      "maxMs": 186.78174999999464
    },
    {
      "id": "q11-dormant-customers",
      "passes": 295,
      "medianMs": 26.469499999977415,
      "p95Ms": 53.94795800000429,
      "maxMs": 79.94558300002245
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 295,
      "medianMs": 16.728457999997772,
      "p95Ms": 33.899167000025045,
      "maxMs": 80.07879100000719
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4475,
      "pintail": 4493
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "16597",
      "pintail": "16683"
    }
  ]
}
```

## Phase: mixed

```json
{
  "mutationStats": {
    "inserts": 88602,
    "updates": 27065,
    "deletes": 469,
    "transactions": 34178,
    "cascadeDeletes": 66,
    "errors": 133
  },
  "readerPasses": 638,
  "sourceToVisibleLagMs": 4422,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 638,
      "medianMs": 106.47658300003968,
      "p95Ms": 280.4983339998871,
      "maxMs": 1427.4374170000665
    },
    {
      "id": "q02-customer-history",
      "passes": 638,
      "medianMs": 120.16766699997243,
      "p95Ms": 327.0384999997914,
      "maxMs": 2053.4081670001615
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 638,
      "medianMs": 106.95129200001247,
      "p95Ms": 280.1344579998404,
      "maxMs": 2619.622291999869
    },
    {
      "id": "q04-inventory-risk",
      "passes": 638,
      "medianMs": 119.69295799999963,
      "p95Ms": 308.23391700023785,
      "maxMs": 2971.217292000074
    },
    {
      "id": "q05-payment-failures",
      "passes": 638,
      "medianMs": 87.11920799990185,
      "p95Ms": 226.00716699985787,
      "maxMs": 2856.7746250000782
    },
    {
      "id": "q06-refund-rate",
      "passes": 638,
      "medianMs": 160.05945900001097,
      "p95Ms": 465.3155000000261,
      "maxMs": 2540.94837499992
    },
    {
      "id": "q07-product-performance",
      "passes": 638,
      "medianMs": 131.57512499997392,
      "p95Ms": 364.3743750001304,
      "maxMs": 1930.2907920000143
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 638,
      "medianMs": 123.43612500000745,
      "p95Ms": 349.558333999943,
      "maxMs": 1846.6465000000317
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 638,
      "medianMs": 103.70666699996218,
      "p95Ms": 292.35612499993294,
      "maxMs": 1402.9125840000343
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 638,
      "medianMs": 126.7409579999512,
      "p95Ms": 376.6824169999454,
      "maxMs": 1802.5052079998422
    },
    {
      "id": "q11-dormant-customers",
      "passes": 638,
      "medianMs": 137.85291699995287,
      "p95Ms": 385.1434579999186,
      "maxMs": 1761.908415999962
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 638,
      "medianMs": 102.71037500002421,
      "p95Ms": 280.8830420002341,
      "maxMs": 1144.9564580000006
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4311,
      "pintail": 4336
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "15967",
      "pintail": "16061"
    }
  ]
}
```

## Phase: post-compaction

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 47.9 | 126.7 |
| q01-tenant-revenue | pintail | ok | 280.9 | 301.8 |
| q02-customer-history | mysql | ok | 22.9 | 37.4 |
| q02-customer-history | pintail | ok | 311.0 | 384.6 |
| q03-fulfillment-backlog | mysql | ok | 19.5 | 330.1 |
| q03-fulfillment-backlog | pintail | ok | 272.1 | 287.4 |
| q04-inventory-risk | mysql | ok | 10.9 | 274.0 |
| q04-inventory-risk | pintail | ok | 297.8 | 321.6 |
| q05-payment-failures | mysql | ok | 13.5 | 26.5 |
| q05-payment-failures | pintail | ok | 246.6 | 335.0 |
| q06-refund-rate | mysql | ok | 173.4 | 192.9 |
| q06-refund-rate | pintail | mismatch | 463.6 | 709.1 |
| q07-product-performance | mysql | ok | 23.7 | 36.3 |
| q07-product-performance | pintail | ok | 339.9 | 449.5 |
| q08-regional-cohorts | mysql | ok | 83.0 | 120.7 |
| q08-regional-cohorts | pintail | ok | 338.9 | 355.7 |
| q09-order-lifecycle | mysql | ok | 17.8 | 101.9 |
| q09-order-lifecycle | pintail | ok | 271.2 | 286.2 |
| q10-wide-operational-join | mysql | ok | 11.5 | 318.0 |
| q10-wide-operational-join | pintail | ok | 319.5 | 669.7 |
| q11-dormant-customers | mysql | ok | 10.2 | 344.6 |
| q11-dormant-customers | pintail | ok | 345.8 | 349.7 |
| q12-per-customer-revenue | mysql | ok | 10.3 | 12.7 |
| q12-per-customer-revenue | pintail | ok | 267.6 | 290.3 |

## Phase: restart

```json
{
  "fingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 4311,
      "pintail": 4336
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "15967",
      "pintail": "16061"
    }
  ]
}
```
