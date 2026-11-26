# Decimal `+`, `-`, `*` chains on scaled units

A pricing-summary aggregate - `SUM(price * (1 - discount))` and
`SUM(price * (1 - discount) * (1 + tax))` over DECIMAL(15,2) columns, four
groups, most rows selected - spent almost all of its time evaluating the two
arguments. Only a single step over two leaves (`1 - discount`) had a units
kernel; every other step went through exact fractions, a `DecimalRational`
per row per node, each one reduced by an `i128` gcd on construction, add and
multiply.

The chain evaluator now computes any chain of decimal `+`, `-`, unary minus
and `*` over columns and constants on the operands' scaled `i128` units,
provided every step's exact answer has the scale the binder declared for it
(a sum or difference at the wider operand's scale, a product at the two
scales added - which is every product below the 30-digit scale cap). Such a
chain rounds nothing, so the units are the exact answer. A division, a capped
product, a function inside the chain that declines, or a selected row whose
units leave 128 bits sends the batch to the exact fractions as before.
Unselected rows that overflow become NULL and never decline the batch.
`ExecCounters::decimal_unit_batches` counts the batches this path answers.

## How measured

In-process, `crates/pintail-exec/tests/integration/decimal_chain_cost.rs`
(`cargo test --profile recovery -p pintail-exec --test integration
decimal_chain_cost:: -- --ignored --nocapture`, with
`PINTAIL_DISABLE_SETTLED_MEMO=1`), 2,000,000 rows, an 8-vCPU box. The two
test binaries ran alternately, three rounds each; each round runs every case
nine times interleaved. Cells are the three rounds' medians (ms), then the
fastest single run. Every case's answer digest is identical across the
binaries. `control` is the same query without the two computed sums.

| case | before (median per round) | after (median per round) | before min | after min |
|---|---|---|---:|---:|
| summary (both sums + 6 others) | 1,324 / 1,526 / 1,574 | 429 / 479 / 537 | 1,229 | 417 |
| discounted (`price * (1 - discount)`) | 546 / 634 / 689 | 223 / 259 / 260 | 494 | 217 |
| charged (`... * (1 + tax)`) | 843 / 1,004 / 1,019 | 279 / 337 / 343 | 788 | 265 |
| control (no computed sum) | 23 / 29 / 30 | 20 / 24 / 24 | 20 | 17 |

The summary is about 3x faster. What remains over the control is no longer
the arithmetic: the computed arguments are projected and folded by the
general hash aggregate on one thread, while the control is answered by the
per-segment grouped fold. A profile of the after binary shows allocation,
per-row group hashing and the projection's per-row memory bound ahead of the
chain kernel itself.

## Shared subtrees within one projection

`SUM(price * (1 - discount))` and `SUM(price * (1 - discount) * (1 + tax))`
both compute `price * (1 - discount)`. A projection (the aggregate-argument
projection included) now finds the decimal subtrees that occur more than once
among its computed expressions and computes each once per batch. Only plain
chains are shared: columns, constants, and decimal `+`, `-`, unary minus and
`*` evaluated on units by the path above, so the shared lane is the exact
answer at its declared scale. Anything that calls a function is never shared,
so no volatile or warning expression is merged. Lanes are tied to the
projection's batch and are never handed to a kernel evaluating over a batch
of its own. `ExecCounters::decimal_lanes_shared` counts reused subtrees.

Same harness, three binaries interleaved (before, units only, units plus
sharing), three rounds each:

| case | units only (median per round) | + sharing (median per round) | units only min | + sharing min |
|---|---|---|---:|---:|
| summary | 429 / 479 / 537 | 409 / 415 / 498 | 417 | 367 |
| discounted | 223 / 259 / 260 | 240 / 242 / 287 | 217 | 213 |
| charged | 279 / 337 / 343 | 296 / 300 / 344 | 265 | 270 |

Sharing only acts where a subtree repeats, so only `summary` can move: about
5-10% by median and minimum, near this box's run-to-run spread. The single
sums are the no-sharing control and stay within noise. The remaining cost is
the general aggregate path, not arithmetic.

## TPC-H Q1 through the server, on top of the parallel packed-key fold

Measured after rebasing on the packed-key fold for wide decimal aggregates
and the fused-round evaluation of computed projections, which made the
in-process "general path" remark above obsolete: Q1 now folds on the
workers, and what they spend is the two products. Release builds, an 8-vCPU
box, `PINTAIL_DISABLE_SETTLED_MEMO=1` and `PINTAIL_COMPACTION_INPUT_ROWS=1`
on every arm, each binary on its own copy of one replica, Q1 run over HTTP
in rotating order, 15 cycles per round after two warm-up runs. The TPC-H
seed was stopped at 81% of SF1 (4,843,000 line items, 4,500,266 of them
passing Q1's filter) because the single-connection seeder would not finish
inside the box's lifetime. Every arm's answer is the same and is
byte-identical to MySQL 8.4's over the same data; the profile reports the
packed-key fold with 0 batches folded row by row.

| arm | median per round (ms) | fastest | process CPU, median per run |
|---|---|---:|---:|
| dev before this lane | 643 / 564 / 589 / 659 / 597 / 670 | 526 | 3,620-4,130 ms |
| units + sharing | 133 / 115 / 120 / 130 / 111 / 141 | 89 | 720-840 ms |

Q1 is about 5x faster in wall clock and CPU. The share of sharing alone,
interleaved against a build of the units slice only (four rounds):

| arm | median per round (ms) | fastest | process CPU, median per run |
|---|---|---:|---:|
| units only | 137 / 126 / 110 / 106 | 94 | 670-750 ms |
| units + sharing | 108 / 106 / 98 / 98 | 84 | 580-630 ms |

Sharing the product saves about 12% of Q1's CPU and 8-20% of its wall time.

Before either the fold or the projection change, on the same data, the
units path alone took Q1 from 2,486-2,895 ms to 531-665 ms per round median
and sharing took it to 433-542 ms; on the packed-key fold without the
fused-round projection, both together took it from 2,486-2,687 ms to
262-326 ms.
