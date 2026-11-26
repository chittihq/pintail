# Wide-declared decimal aggregates in the packed-key fold

TPC-H Q1 groups by two text columns, which the packed-key fold takes, but
two of its sums are over products of DECIMAL(15,2) columns: the product
declares 31 digits and the three-factor product 47. A lane carried only
columns declared within 18 digits, so the plan declined ("an aggregate has
no lane") and Q1 ran on the general path, a vector of values per row.

The lane now takes a decimal column of any declared width, and the plan
checks each batch: a batch whose selected units all fit 64 bits folds in
the packed cells (a stored column's narrow units, or a computed column's
full-width units read straight from its vector), and one holding a value
past them folds row by row through the general per-group state. Totals
stay exact in 128 bits; a first batch past 64 bits declines the plan with
"a decimal aggregate's units do not fit a lane". A note the fold writes on
an aggregate over computed arguments now reaches the profile (it was lost
on the projection the executor inserts).

## TPC-H Q1, SF1, end to end

An 8-vCPU box (AMD EPYC, Zen 5, KVM). Release builds of the base and of
this change, each on its own copy of one SF1 replica, memo off,
`PINTAIL_COMPACTION_INPUT_ROWS=1`, interleaved with rotating order, 6
rounds x 5 cycles, every answer compared with MySQL 8.4 (0 mismatches).
`base2` is the base binary on a third copy: the floor.

| arm   | wall median | wall min | wall max | process CPU median |
|-------|------------:|---------:|---------:|-------------------:|
| base  | 3,508 ms    | 3,374 ms | 3,870 ms | 4,760 ms           |
| base2 | 3,507 ms    | 3,415 ms | 3,891 ms | 4,760 ms           |
| head  | 3,218 ms    | 3,128 ms | 3,382 ms | 3,470 ms           |

-8.3% wall, -27% CPU. The profile now reads `packed-key fold: ... 6
groups in a dense table of 12 slots by text class, 6000749 rows folded by
text class, ... 0 batches folded row by row`.

## Where Q1's time is now

`wide_decimal_lanes::bench`, in process, 6M rows of an invented ledger of
the same shape (recovery profile, same box, median of 5). The disabled arm
is `PINTAIL_DISABLE_PACKED_GROUP=1`, the general path.

| shape | packed | general |
|-------|-------:|--------:|
| summary (Q1's shape plus MIN/MAX of a product) | 5,809 ms | 6,493 ms |
| plain columns only | 30 ms | 1,281 ms |
| one two-factor product | 1,202 ms | 1,610 ms |
| one three-factor product | 2,115 ms | 2,679 ms |

The aggregation itself is now a few milliseconds a million rows. What is
left is evaluating the products: about 200 ns a row for a two-factor
product and 350 ns for a three-factor one, through the exact row-by-row decimal evaluation (`(1 - cut)` is not a leaf, so the
unit-step kernel declines).
