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
