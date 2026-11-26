# Computed projections on fused-round workers

An aggregate over computed arguments - TPC-H Q1's
`SUM(l_extendedprice * (1 - l_discount))` - projects those arguments into
columns first. That projection declined every fused round, so the grouped
general path and the ungrouped fold pulled their input a batch at a time
and evaluated the arguments on the query's own thread; only the grouping
of each round's morsels ran on the pool.

## Where the time went (before)

`PINTAIL_PROFILE=1`, TPC-H SF1 (6,000,749 line items), base `ce534f86`:

| Query | HashAggregate self | Scan | Wall | Process CPU |
|---|---:|---:|---:|---:|
| q01-pricing-summary | 3,463 ms | 37 ms | 3,421 ms | 4,480 ms |
| x2 (ungrouped `SUM(p * (1 - d))`, `SUM(p * (1 - d) * (1 + t))`) | 3,202 ms | 7 ms | 3,365 ms | 3,390 ms |

Wall within 25% of process CPU on an 8-vCPU box: almost all of it ran on
one thread. With the change the same work moves under the scan's fused
round (q01: Scan 825 ms wall across the workers, HashAggregate self
375 ms) and process CPU is unchanged (4,560 ms).

## Before / after

An 8-vCPU box (AMD EPYC, `/dev/kvm` present), release builds, one server
per arm each on its own copy of one SF1 replica, settled memo off,
`PINTAIL_COMPACTION_INPUT_ROWS=1` on every arm, every arm idle before the
first query. Six cycles, each query once per arm per cycle, arm order
rotating. Every answer was identical across arms; q01/q03/q05/q10 matched
MySQL 8.4 byte for byte when the replica was built, and x1-x4 match the
MySQL answers to the same text.

- base: `ce534f86`
- head1: computed projections on fused-round workers, and the grouped
  general path reading its rounds ahead in fused rounds
- head2: head1, plus the ungrouped fold's declined input read the same way

| Query | base median (min-max) ms | head1 median (min-max) ms | head2 median (min-max) ms | CPU ms base / head2 |
|---|---:|---:|---:|---:|
| q01-pricing-summary | 3,397 (3,329-3,437) | 1,116 (1,107-1,126) | 1,122 (1,098-1,140) | 4,540 / 4,620 |
| q03-shipping-priority | 75 (65-78) | 77 (67-79) | 76 (68-78) | 230 / 250 |
| q05-local-supplier-volume | 158 (128-169) | 162 (128-165) | 153 (136-160) | 260 / 270 |
| q10-returned-item-reporting | 200 (186-205) | 201 (197-212) | 195 (189-212) | 410 / 410 |
| x1 ungrouped `SUM(p * d)` under a date and range filter | 13 (12-14) | 14 (12-16) | 13 (11-14) | 60 / 50 |
| x2 ungrouped `SUM(p * (1 - d))`, `SUM(p * (1 - d) * (1 + t))`, `COUNT(*)` | 3,229 (3,185-3,351) | 1,907 (1,886-1,919) | 778 (749-790) | 3,310 / 3,530 |
| x3 `GROUP BY` text, `SUM(int * int)` | 59 (54-63) | 52 (48-61) | 62 (56-72) | 100 / 120 |
| x4 `GROUP BY YEAR(date)`, `SUM(p * (1 - d))` | 1,179 (1,140-1,231) | 1,198 (1,169-1,249) | 1,190 (1,161-1,241) | 1,440 / 1,500 |

q01 is 3.0x faster and x2 4.2x; the join queries and the small ones are
inside their spread. x4 is unchanged: its date-part key takes the
two-pass aggregate, whose fused rounds do not run for this shape, and it
still pulls; that path is not changed here.

## Correctness

- `crates/pintail-exec/tests/integration/fused_computed_projection.rs`
  runs each shape as written and with a `RAND()` aggregate beside it that
  keeps the projection on the query's thread, and asserts the same rows,
  the same division-by-zero warning count (and the count of zero
  divisors), and the batches projected on workers by the
  `projected_batches_on_workers` counter: every batch but the one that
  raises warnings, which is projected on the query's thread.
- A projection with a volatile function, a clock read, a statement
  variable or a prepared membership set never folds.

## On top of the decimal packed-key fold

With wide-declared decimal aggregates folding in the packed-key fold, Q1
leaves the general hash aggregate: the profile reads "packed-key fold ...
6 groups in a dense table of 12 slots by text class ... 0 batches folded
row by row". That fold takes its input in fused rounds, and the computed
projection declined them, so it pulled. With this change the projection
evaluates on the round's workers and feeds the fold in place.

Same box, same replica and method, six interleaved cycles; base is the
packed-key fold commit, head is these two commits rebased onto it. Every
answer identical across arms.

| Query | base median (min-max) ms | head median (min-max) ms | CPU ms base / head |
|---|---:|---:|---:|
| q01-pricing-summary | 3,231 (3,176-3,375) | 543 (533-599) | 3,450 / 3,350 |
| q03-shipping-priority | 59 (55-60) | 62 (57-74) | 200 / 200 |
| q05-local-supplier-volume | 150 (135-164) | 147 (124-153) | 270 / 250 |
| q10-returned-item-reporting | 210 (192-216) | 211 (186-226) | 410 / 420 |
| x1 | 13 (12-23) | 14 (10-21) | 50 / 60 |
| x2 | 3,293 (3,253-3,339) | 755 (740-775) | 3,370 / 3,440 |
| x3 | 60 (48-68) | 54 (48-62) | 90 / 90 |
| x4 | 1,182 (1,166-1,252) | 1,171 (1,165-1,210) | 1,490 / 1,440 |

q01 is 5.9x faster on this base (3.0x on the one before it, where the
general path's read-ahead carried it).
