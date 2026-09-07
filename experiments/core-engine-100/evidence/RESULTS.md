# Changing-data experiment results

Ratios compare each algorithm with its explicit reference over the same changing
fixture. They are **not speedups over the current Pintail SQL executor**. The
query figure includes a real storage scan and materialization followed by the
experimental operator. Cycle time also waits for concurrent mutation/maintenance.
Setup for candidate indexes and intermediate state is included; initial fixture
creation and independent correctness checks are excluded.

990 processes; 7920 checked snapshots; 100 alternatives.

| Case | Approach | Query ratio | Cycle ratio | Operator ratio | Worst scenario median |
|---|---|---:|---:|---:|---:|
| 1 | 1: selective-predicate-first | 1.02× | 1.00× | 1.23× | 0.94× |
| 1 | 2: selection-vector | 0.99× | 1.02× | 1.11× | 0.96× |
| 1 | 3: full-bitmap | 0.99× | 1.00× | 0.72× | 0.95× |
| 1 | 4: block-bitmap-set-bits | 0.99× | 1.00× | 0.95× | 0.96× |
| 1 | 5: parallel-block-filter | 1.01× | 1.00× | 1.26× | 0.98× |
| 1 | 6: build-zone-maps | 1.01× | 1.01× | 0.78× | 1.01× |
| 1 | 7: build-sorted-value-index | 0.97× | 0.97× | 0.11× | 0.89× |
| 1 | 8: build-low-key-buckets | 0.99× | 0.97× | 0.48× | 0.99× |
| 1 | 9: columnar-filter-projection | 0.99× | 1.02× | 0.59× | 0.98× |
| 1 | 10: two-phase-block-survivors | 1.00× | 1.00× | 0.97× | 0.99× |
| 2 | 1: hash-latest | 0.97× | 0.98× | 0.84× | 0.93× |
| 2 | 2: sort-version-reduce | 1.03× | 1.02× | 1.23× | 1.02× |
| 2 | 3: sorted-two-way | 1.09× | 1.01× | 1.68× | 1.06× |
| 2 | 4: dense-version-slots | 1.07× | 1.03× | 1.43× | 1.06× |
| 2 | 5: binary-patch-base | 1.08× | 1.03× | 1.87× | 1.06× |
| 2 | 6: sparse-overlay-map | 1.05× | 1.01× | 1.27× | 1.04× |
| 2 | 7: block-overlap-search | 1.06× | 1.00× | 1.51× | 1.01× |
| 2 | 8: parallel-key-ranges | 1.04× | 1.00× | 1.32× | 1.00× |
| 2 | 9: visibility-bitmap | 1.04× | 0.98× | 1.32× | 0.95× |
| 2 | 10: copy-winning-runs | 1.08× | 1.00× | 1.47× | 1.02× |
| 3 | 1: hash-group | 0.98× | 0.99× | 0.70× | 0.97× |
| 3 | 2: dense-group | 1.04× | 1.02× | 3.88× | 1.03× |
| 3 | 3: four-independent-dense-lanes | 1.05× | 1.02× | 3.24× | 0.97× |
| 3 | 4: sort-run-reduce | 1.01× | 1.00× | 0.71× | 1.00× |
| 3 | 5: key-partition-hash | 1.00× | 1.01× | 0.43× | 0.96× |
| 3 | 6: parallel-local-hash | 1.05× | 1.02× | 1.23× | 1.02× |
| 3 | 7: parallel-local-dense | 1.04× | 1.01× | 3.58× | 1.03× |
| 3 | 8: adaptive-small-map | 1.03× | 1.01× | 0.71× | 1.03× |
| 3 | 9: group-membership-bitmaps | 1.03× | 1.02× | 1.72× | 1.00× |
| 3 | 10: counting-scatter-groups | 1.00× | 1.02× | 1.51× | 0.94× |
| 4 | 1: hash-heap | 1.05× | 1.03× | 1.59× | 1.00× |
| 4 | 2: dense-heap | 1.10× | 1.00× | 4.21× | 1.02× |
| 4 | 3: sort-reduce-heap | 0.99× | 1.00× | 0.96× | 0.93× |
| 4 | 4: parallel-local-hash-heap | 0.99× | 1.01× | 1.17× | 0.96× |
| 4 | 5: key-owned-local-topk | 1.06× | 1.03× | 2.31× | 1.01× |
| 4 | 6: radix-shuffle-dense-partials | 1.06× | 1.00× | 1.59× | 0.99× |
| 4 | 7: adaptive-domain-group | 1.05× | 1.02× | 3.33× | 1.00× |
| 4 | 8: tree-quickselect | 1.01× | 1.02× | 1.07× | 0.99× |
| 4 | 9: sorted-run-merge-heap | 0.98× | 1.02× | 0.69× | 0.90× |
| 4 | 10: hash-quickselect | 1.01× | 1.01× | 1.78× | 0.96× |
| 5 | 1: hash-bucket-join | 1.01× | 1.00× | 1.34× | 0.97× |
| 5 | 2: dense-bucket-join | 1.03× | 1.02× | 6.26× | 1.01× |
| 5 | 3: sorted-merge-join | 0.97× | 1.01× | 1.03× | 0.89× |
| 5 | 4: sorted-binary-join | 0.99× | 0.99× | 1.49× | 0.99× |
| 5 | 5: radix-partition-join | 1.05× | 1.05× | 1.69× | 0.99× |
| 5 | 6: parallel-hash-probe | 1.05× | 1.05× | 3.30× | 1.00× |
| 5 | 7: parallel-dense-probe | 1.07× | 1.05× | 5.82× | 1.02× |
| 5 | 8: bloom-prefilter-hash | 1.00× | 1.01× | 5.41× | 0.97× |
| 5 | 9: build-side-demand-filter | 0.98× | 0.99× | 0.98× | 0.95× |
| 5 | 10: factorized-fact-aggregate | 1.01× | 1.02× | 1.45× | 0.95× |
| 6 | 1: hash-pairs | 1.04× | 1.02× | 1.56× | 1.03× |
| 6 | 2: per-group-hashsets | 1.01× | 1.02× | 1.24× | 0.99× |
| 6 | 3: sort-deduplicate | 1.04× | 1.03× | 1.37× | 1.03× |
| 6 | 4: dense-group-bitmaps | 1.04× | 0.99× | 3.88× | 1.03× |
| 6 | 5: parallel-local-bitmaps | 1.09× | 1.04× | 3.32× | 1.08× |
| 6 | 6: radix-sort-pairs | 1.02× | 1.03× | 1.29× | 0.99× |
| 6 | 7: sorted-run-union | 0.97× | 0.96× | 0.76× | 0.92× |
| 6 | 8: per-group-sort | 1.01× | 0.99× | 2.02× | 0.98× |
| 6 | 9: adaptive-inline-sets | 1.04× | 1.03× | 1.26× | 1.03× |
| 6 | 10: sparse-word-bitmaps | 1.05× | 1.01× | 1.91× | 1.00× |
| 7 | 1: bounded-heap | 1.06× | 1.02× | 1.40× | 0.98× |
| 7 | 2: quickselect-prefix | 1.08× | 1.01× | 7.49× | 1.08× |
| 7 | 3: sorted-small-vector | 1.05× | 1.02× | 2.15× | 1.01× |
| 7 | 4: chunk-local-heaps | 1.06× | 1.01× | 1.53× | 1.04× |
| 7 | 5: parallel-local-selection | 1.08× | 1.03× | 5.78× | 1.03× |
| 7 | 6: value-bucket-selection | 1.05× | 1.02× | 4.33× | 0.98× |
| 7 | 7: radix-score-order | 1.06× | 1.02× | 1.63× | 1.05× |
| 7 | 8: tournament-tree | 1.05× | 1.02× | 2.15× | 1.00× |
| 7 | 9: block-bound-pruning | 1.07× | 1.02× | 2.36× | 0.98× |
| 7 | 10: buffered-selection | 1.08× | 1.03× | 3.66× | 1.05× |
| 8 | 1: prefix-sum-monotone-min | 1.05× | 1.05× | 1.13× | 1.02× |
| 8 | 2: running-sum-monotone-min | 1.08× | 1.06× | 1.85× | 1.05× |
| 8 | 3: segment-tree-range | 1.10× | 1.05× | 1.37× | 1.05× |
| 8 | 4: sparse-table-min-prefix | 1.10× | 1.05× | 1.35× | 0.99× |
| 8 | 5: two-stack-aggregate-queue | 1.08× | 1.04× | 2.25× | 1.02× |
| 8 | 6: block-min-prefix | 1.03× | 1.05× | 1.17× | 1.01× |
| 8 | 7: parallel-halo-windows | 1.11× | 1.07× | 2.15× | 1.04× |
| 8 | 8: square-root-range-blocks | 1.01× | 1.06× | 1.35× | 1.00× |
| 8 | 9: ordered-multiset-window | 0.94× | 0.98× | 0.53× | 0.91× |
| 8 | 10: lazy-min-heap | 1.02× | 1.03× | 0.64× | 0.92× |
| 9 | 1: hash-membership | 1.03× | 1.02× | 1.84× | 1.02× |
| 9 | 2: sorted-binary-membership | 1.03× | 1.01× | 1.52× | 0.99× |
| 9 | 3: dense-membership-bitmap | 1.05× | 1.02× | 3.39× | 1.04× |
| 9 | 4: bloom-negative-filter | 1.02× | 1.02× | 1.70× | 1.01× |
| 9 | 5: hash-partition-membership | 1.03× | 0.99× | 1.51× | 0.98× |
| 9 | 6: sorted-probe-merge | 1.03× | 1.03× | 1.17× | 1.00× |
| 9 | 7: parallel-hash-membership | 1.04× | 1.03× | 2.40× | 1.01× |
| 9 | 8: eytzinger-search | 1.01× | 1.00× | 1.34× | 1.00× |
| 9 | 9: radix-membership | 1.02× | 1.03× | 1.12× | 0.96× |
| 9 | 10: memoized-probe-outcomes | 0.98× | 0.98× | 1.51× | 0.97× |
| 10 | 1: memo-distinct-outer | 1.11× | 1.06× | 6.40× | 0.99× |
| 10 | 2: hash-decorrelation | 1.14× | 1.04× | 3.90× | 1.10× |
| 10 | 3: dense-decorrelation | 1.14× | 1.06× | 15.29× | 1.13× |
| 10 | 4: sorted-range-lookup | 1.10× | 1.02× | 2.56× | 1.03× |
| 10 | 5: key-row-position-index | 1.11× | 1.05× | 2.73× | 1.10× |
| 10 | 6: demand-filtered-aggregate | 1.15× | 1.07× | 9.19× | 1.07× |
| 10 | 7: parallel-partial-decorrelation | 1.10× | 1.05× | 4.72× | 1.03× |
| 10 | 8: parallel-dependent-scans | 1.14× | 1.06× | 3.48× | 1.10× |
| 10 | 9: sorted-prefix-sums | 1.13× | 1.07× | 2.36× | 1.08× |
| 10 | 10: demand-bitmap-dense-fold | 1.19× | 1.09× | 62.10× | 1.16× |

## Observed alternatives per workload

Selection is exploratory; these same samples selected the winners. Confirm on
new seeds, larger tables and the real SQL path before adopting anything.

- Case 1: selective-predicate-first, 1.02× query, 1.00× cycle.
- Case 2: sorted-two-way, 1.09× query, 1.01× cycle.
- Case 3: parallel-local-hash, 1.05× query, 1.02× cycle.
- Case 4: dense-heap, 1.10× query, 1.00× cycle.
- Case 5: parallel-dense-probe, 1.07× query, 1.05× cycle.
- Case 6: parallel-local-bitmaps, 1.09× query, 1.04× cycle.
- Case 7: quickselect-prefix, 1.08× query, 1.01× cycle.
- Case 8: parallel-halo-windows, 1.11× query, 1.07× cycle.
- Case 9: dense-membership-bitmap, 1.05× query, 1.02× cycle.
- Case 10: demand-bitmap-dense-fold, 1.19× query, 1.09× cycle.
