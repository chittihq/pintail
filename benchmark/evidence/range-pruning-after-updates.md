# Range filters on an updated table: value pruning before and after

A range filter on a timestamp column over a 20,000,000-row table that has
taken updates, measured in process by
`crates/pintail-exec/tests/range_prune_bench.rs` (release build, 32-core
build host, warm page cache, background compaction off so the updated
state is measured as it stands).

The table is loaded in key order with `created_at` rising with the key -
twenty one-million-row segments - and then one row in a hundred is updated
across the whole key range and flushed: one newer segment overlapping every
base segment, which is the shape a replicated table keeps while its source
is written to.

| Filter | Before (`8312adf8`) | After (`4af9901b`) |
|---|---:|---:|
| One day: `COUNT(*)`, `SUM(amount)` | 21,671 ms | 1,156 ms |
| One day: newest 50 rows | 25,857 ms | 1,480 ms |
| One week: per-state count | not reached | 1,332 ms |

Medians of five runs, each over a different window. The before run was
stopped by the host during its third shape after spending over twenty
seconds on each of the first two.

Before, value pruning dropped a segment only when it overlapped no other
segment, so the updated segment lying across the base switched it off for
all twenty and every filter read the whole table. After, a segment prunes
when every segment overlapping it is newer; the filter reads the one base
segment its window reaches plus the updates.

What remains is the merge of that base segment with the updates: pruning
is per segment, so a one-day window still merges a million-row segment to
find its 43,200 rows. Pruning within a segment - value bounds per block -
is the next step for narrow windows.
