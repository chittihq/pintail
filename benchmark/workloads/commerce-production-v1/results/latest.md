# commerce-production-v1 — ci profile

Run: 2026-08-13T16:02:31.033Z → 2026-08-13T16:46:11.612Z. Engines: mysql, pintail. Scale: 0.01.

## Phase: cold

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 162.7 | 325.3 |
| q01-tenant-revenue | pintail | ok | 435.7 | 602.6 |
| q02-customer-history | mysql | ok | 10.0 | 24.3 |
| q02-customer-history | pintail | ok | 713.9 | 925.7 |
| q03-fulfillment-backlog | mysql | ok | 13.2 | 17.8 |
| q03-fulfillment-backlog | pintail | ok | 403.9 | 478.7 |
| q04-inventory-risk | mysql | ok | 10.3 | 27.2 |
| q04-inventory-risk | pintail | ok | 420.7 | 550.2 |
| q05-payment-failures | mysql | ok | 162.0 | 164.4 |
| q05-payment-failures | pintail | ok | 4.7 | 298.8 |
| q06-refund-rate | mysql | ok | 970.3 | 973.6 |
| q06-refund-rate | pintail | ok | 1142.8 | 1487.1 |
| q07-product-performance | mysql | ok | 943.0 | 1080.0 |
| q07-product-performance | pintail | ok | 1167.9 | 1282.3 |
| q08-regional-cohorts | mysql | ok | 431.3 | 469.8 |
| q08-regional-cohorts | pintail | ok | 798.4 | 848.2 |
| q09-order-lifecycle | mysql | ok | 266.3 | 270.3 |
| q09-order-lifecycle | pintail | ok | 648.0 | 683.1 |
| q10-wide-operational-join | mysql | ok | 522.0 | 674.9 |
| q10-wide-operational-join | pintail | ok | 1397.6 | 1720.6 |
| q11-dormant-customers | mysql | ok | 20.1 | 38.8 |
| q11-dormant-customers | pintail | ok | 1015.3 | 1201.0 |
| q12-per-customer-revenue | mysql | ok | 15.9 | 36.1 |
| q12-per-customer-revenue | pintail | ok | 373.1 | 382.0 |

## Phase: mixed-light

```json
{
  "mutationStats": {
    "inserts": 9820,
    "updates": 3010,
    "deletes": 69,
    "transactions": 3784,
    "cascadeDeletes": 7,
    "errors": 0
  },
  "readerPasses": 22,
  "sourceToVisibleLagMs": 1590,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 22,
      "medianMs": 458.0308330000262,
      "p95Ms": 643.18987500004,
      "maxMs": 709.765625
    },
    {
      "id": "q02-customer-history",
      "passes": 22,
      "medianMs": 919.2472919999855,
      "p95Ms": 1182.0931660000351,
      "maxMs": 1354.61341599992
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 22,
      "medianMs": 477.89454200002365,
      "p95Ms": 660.7130420000176,
      "maxMs": 689.9042080000509
    },
    {
      "id": "q04-inventory-risk",
      "passes": 22,
      "medianMs": 465.51229199999943,
      "p95Ms": 555.0677920000162,
      "maxMs": 623.5507499999367
    },
    {
      "id": "q05-payment-failures",
      "passes": 22,
      "medianMs": 23.6102499999688,
      "p95Ms": 33.98754200001713,
      "maxMs": 35.92820899997605
    },
    {
      "id": "q06-refund-rate",
      "passes": 22,
      "medianMs": 1290.0584999999846,
      "p95Ms": 1942.0550420000218,
      "maxMs": 2190.174791999976
    },
    {
      "id": "q07-product-performance",
      "passes": 22,
      "medianMs": 1369.2295000000158,
      "p95Ms": 1491.6987499999814,
      "maxMs": 1702.7918749999953
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 22,
      "medianMs": 935.9877080000006,
      "p95Ms": 1358.903667000006,
      "maxMs": 1778.022042000026
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 22,
      "medianMs": 750.6796660000691,
      "p95Ms": 1096.6161250000587,
      "maxMs": 1199.8474170000409
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 22,
      "medianMs": 1854.1253330000327,
      "p95Ms": 2620.902457999997,
      "maxMs": 2822.637791999965
    },
    {
      "id": "q11-dormant-customers",
      "passes": 22,
      "medianMs": 1295.6181250000373,
      "p95Ms": 1523.9906250000931,
      "maxMs": 1575.6811659999657
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 22,
      "medianMs": 416.1356669999659,
      "p95Ms": 524.9884589998983,
      "maxMs": 528.0359160000226
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 454344,
      "pintail": 454357
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "1683280",
      "pintail": "1683305"
    }
  ]
}
```

## Phase: mixed

```json
{
  "mutationStats": {
    "inserts": 94489,
    "updates": 28931,
    "deletes": 502,
    "transactions": 36485,
    "cascadeDeletes": 74,
    "errors": 215
  },
  "readerPasses": 102,
  "sourceToVisibleLagMs": 4376,
  "underLoadLatency": [
    {
      "id": "q01-tenant-revenue",
      "passes": 102,
      "medianMs": 674.458583000116,
      "p95Ms": 915.1287910002284,
      "maxMs": 1418.1414580000564
    },
    {
      "id": "q02-customer-history",
      "passes": 102,
      "medianMs": 1142.8861670000479,
      "p95Ms": 1806.4450000000652,
      "maxMs": 3262.429625000106
    },
    {
      "id": "q03-fulfillment-backlog",
      "passes": 102,
      "medianMs": 697.669666999951,
      "p95Ms": 1127.6041249996051,
      "maxMs": 1870.4315830001142
    },
    {
      "id": "q04-inventory-risk",
      "passes": 102,
      "medianMs": 703.7949580000713,
      "p95Ms": 1023.4233329999261,
      "maxMs": 1578.5700000000652
    },
    {
      "id": "q05-payment-failures",
      "passes": 102,
      "medianMs": 169.6708749998361,
      "p95Ms": 284.94591700006276,
      "maxMs": 512.4632920001168
    },
    {
      "id": "q06-refund-rate",
      "passes": 102,
      "medianMs": 1625.272333000088,
      "p95Ms": 2136.5305830000434,
      "maxMs": 3461.7294999998994
    },
    {
      "id": "q07-product-performance",
      "passes": 102,
      "medianMs": 1578.3299589999951,
      "p95Ms": 2341.865749999881,
      "maxMs": 2715.324874999933
    },
    {
      "id": "q08-regional-cohorts",
      "passes": 102,
      "medianMs": 1180.7449590000324,
      "p95Ms": 1665.5087919998914,
      "maxMs": 2517.133374999976
    },
    {
      "id": "q09-order-lifecycle",
      "passes": 102,
      "medianMs": 933.339999999851,
      "p95Ms": 1382.7243329999037,
      "maxMs": 2192.385666999966
    },
    {
      "id": "q10-wide-operational-join",
      "passes": 102,
      "medianMs": 2126.081707999925,
      "p95Ms": 2987.488124999916,
      "maxMs": 4212.6675419998355
    },
    {
      "id": "q11-dormant-customers",
      "passes": 102,
      "medianMs": 1568.608125000028,
      "p95Ms": 2148.968708000146,
      "maxMs": 2547.8974170000292
    },
    {
      "id": "q12-per-customer-revenue",
      "passes": 102,
      "medianMs": 666.4637919999659,
      "p95Ms": 946.7315829999279,
      "maxMs": 2226.2324999999255
    }
  ],
  "fingerprintMismatches": [],
  "expectedFingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 454132,
      "pintail": 454183
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "1682569",
      "pintail": "1682742"
    }
  ]
}
```

## Phase: post-compaction

| Query | Engine | Status | Median ms | p95 ms |
|---|---|---|---:|---:|
| q01-tenant-revenue | mysql | ok | 97.8 | 154.7 |
| q01-tenant-revenue | pintail | ok | 748.2 | 814.0 |
| q02-customer-history | mysql | ok | 8.7 | 42.3 |
| q02-customer-history | pintail | ok | 1416.3 | 1765.3 |
| q03-fulfillment-backlog | mysql | ok | 17.9 | 64.8 |
| q03-fulfillment-backlog | pintail | ok | 1044.9 | 1602.9 |
| q04-inventory-risk | mysql | ok | 10.8 | 92.8 |
| q04-inventory-risk | pintail | ok | 784.6 | 875.2 |
| q05-payment-failures | mysql | ok | 98.2 | 113.8 |
| q05-payment-failures | pintail | ok | 244.5 | 294.7 |
| q06-refund-rate | mysql | ok | 1103.5 | 1153.9 |
| q06-refund-rate | pintail | ok | 1863.8 | 1998.5 |
| q07-product-performance | mysql | ok | 862.8 | 943.2 |
| q07-product-performance | pintail | ok | 1604.8 | 1678.7 |
| q08-regional-cohorts | mysql | ok | 494.9 | 598.3 |
| q08-regional-cohorts | pintail | ok | 1253.9 | 1540.1 |
| q09-order-lifecycle | mysql | ok | 276.3 | 470.3 |
| q09-order-lifecycle | pintail | ok | 993.5 | 1165.4 |
| q10-wide-operational-join | mysql | ok | 26.2 | 46.9 |
| q10-wide-operational-join | pintail | ok | 2162.7 | 2653.8 |
| q11-dormant-customers | mysql | ok | 12.9 | 50.1 |
| q11-dormant-customers | pintail | ok | 1699.7 | 2051.4 |
| q12-per-customer-revenue | mysql | ok | 13.2 | 36.0 |
| q12-per-customer-revenue | pintail | ok | 681.7 | 731.9 |

## Phase: restart

```json
{
  "fingerprintMismatches": [
    {
      "table": "shipment_items",
      "field": "rows",
      "mysql": 454132,
      "pintail": 454183
    },
    {
      "table": "shipment_items",
      "field": "amountSum",
      "mysql": "1682569",
      "pintail": "1682742"
    }
  ]
}
```
