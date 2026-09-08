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
| 8 | 6: block-min-prefix | 1.15× | 1.07× | 2.20× | 1.15× |

## Observed alternatives per workload

These arms were selected from the independent screen before these measurements.
Small confirmation samples do not certify an installed engine optimization.

- Case 8: block-min-prefix, 1.15× query, 1.07× cycle.
