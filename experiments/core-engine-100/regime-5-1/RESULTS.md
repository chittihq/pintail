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
| 5 | 10: factorized-fact-aggregate | 2.06× | 1.44× | 47.21× | 2.06× |

## Observed alternatives per workload

These arms were selected from the independent screen before these measurements.
Small confirmation samples do not certify an installed engine optimization.

- Case 5: factorized-fact-aggregate, 2.06× query, 1.44× cycle.
