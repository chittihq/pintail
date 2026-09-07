# Changing-data experiment results

Ratios compare each algorithm with its explicit reference over the same changing
fixture. They are **not speedups over the current Pintail SQL executor**. The
query figure includes a real storage scan and materialization followed by the
experimental operator. Cycle time also waits for concurrent mutation/maintenance.
Setup for candidate indexes and intermediate state is included; initial fixture
creation and independent correctness checks are excluded.

6 processes; 48 checked snapshots; 1 alternatives.

| Case | Approach | Query ratio | Cycle ratio | Operator ratio | Worst scenario median |
|---|---|---:|---:|---:|---:|
| 10 | 3: dense-decorrelation | 1.22× | 1.06× | 39.00× | 1.22× |

## Strongest observed alternative per workload

Selection is exploratory; these same samples selected the winners. Confirm on
new seeds, larger tables and the real SQL path before adopting anything.

- Case 10: dense-decorrelation, 1.22× query, 1.06× cycle.
