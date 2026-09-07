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
| 4 | 2: dense-heap | 1.05× | 0.98× | 4.42× | 1.00× |

## Observed alternatives per workload

These arms were selected from the independent screen before these measurements.
Small confirmation samples do not certify an installed engine optimization.

- Case 4: dense-heap, 1.05× query, 0.98× cycle.
