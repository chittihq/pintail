# Per-row costs in aggregates, JSON and TIME readers, and spilled windows

Measured 2026-11-14 on an 8-vCPU, 16 GB box (AMD EPYC 4585PX, KVM), in
process, with the instruction suite's `answers` binary
(`benchmark/instruction-suite`): one statement through parse, bind, plan
and execute over a real store, result memo off, default thread pools.
The table is the suite's `readings` fixture: 400,000 rows in 150,000
groups, a mixed-case tag of twelve values, a TIME that goes negative and
past a day, and a JSON document of three members.

Base is the commit before the changes, head the commit after; each was
built into its own target directory. The two binaries ran interleaved, one
invocation each per round, order alternating, six rounds. An invocation
loads the fixture and runs the statement fifteen times, reporting the
fastest; the table gives the median and the minimum of the six.

| case | statement | base median / min (ms) | head median / min (ms) |
|---|---|---:|---:|
| `groups_concat` | `GROUP_CONCAT(tag ORDER BY id SEPARATOR '\|')` per group | 1,651 / 1,266 | 489 / 355 |
| `groups_concat_distinct` | `GROUP_CONCAT(DISTINCT tag ORDER BY tag)` per group | 1,981 / 1,789 | 586 / 556 |
| `groups_sum_distinct` | `SUM(DISTINCT decimal)`, `SUM(DISTINCT integer)` per group | 606 / 563 | 299 / 210 |
| `groups_count_distinct` | `COUNT(DISTINCT integer)` per group | 66 / 59 | 34 / 22 |
| `groups_text_distinct` | `COUNT(DISTINCT tag)` per group | 350 / 304 | 317 / 285 |
| `json_extracts` | four JSON readers of one column | 952 / 930 | 642 / 635 |
| `time_functions` | `TIME_TO_SEC`, `HOUR`, `MINUTE`, `ADDTIME` of a TIME column | 1,281 / 1,264 | 798 / 774 |
| `window_spilled` | `ROW_NUMBER() OVER (PARTITION BY tag ORDER BY qty, id)` under a 48 MiB ceiling | 2,240 / 2,133 | 1,417 / 1,341 |
| `window_text_order` | `ROW_NUMBER() OVER (ORDER BY label, id)`, in memory | 511 / 448 | 497 / 460 |
| `many_groups` (unchanged path) | `COUNT(*)`, `SUM` over 100,000 groups | 7.8 / 7.1 | 8.7 / 7.3 |
| `count_distinct` (unchanged path) | two ungrouped `COUNT(DISTINCT)` | 8.1 / 7.7 | 7.8 / 7.5 |

Every case answers with the same digest on both binaries.

## What moved, and what did not

- **`GROUP_CONCAT`.** At base the statement spilled three runs under a
  2 GiB ceiling: each group reserved room for sixty-four rows. It no
  longer spills, and a row is one allocation where it was three or four.
- **`SUM(DISTINCT)`.** The decimal half keys and adds packed units; both
  halves hold a group's first four keys in the state.
- **`COUNT(DISTINCT text)` per group** is within the box's noise: its set
  of text keys is still a table per group.
- **JSON.** The second to fourth reader of a row reuse the first one's
  parse; one parse per row remains.
- **TIME.** `TIME_TO_SEC`, `HOUR` and `MINUTE` read packed microseconds;
  the `ADDTIME` in the statement still reads each value as text and is
  most of what is left.
- **The window in memory** already ordered text by weights built once a
  row (`sort_keys_prepared`); the two runs differ by noise. The spilled
  path now does the same and reads its rows back in batches.

The two unchanged-path rows are the floor: the same code on both sides
reads within about 10% run to run on this box.

## Instruction counts

`bun run scripts/instruction-suite.ts run` on each tree, then `diff`
(one worker per pool, Callgrind; see `docs/instruction-counts.md`).

| case | base | head | change |
|---|---:|---:|---:|
| `groups_concat` | 6,395,691,879 | 1,910,063,758 | -70.1% |
| `groups_concat_distinct` | 9,439,840,043 | 2,854,113,967 | -69.8% |
| `groups_sum_distinct` | 3,258,745,534 | 1,726,617,850 | -47.0% |
| `groups_count_distinct` | 794,841,441 | 579,191,023 | -27.1% |
| `groups_text_distinct` | 1,595,882,095 | 1,572,481,949 | -1.5% |
| `json_extracts` | 20,113,148,202 | 13,113,702,503 | -34.8% |
| `time_functions` | 22,330,121,765 | 14,215,051,038 | -36.3% |
| `window_spilled` | 35,351,070,371 | 18,750,864,876 | -47.0% |
| `window_text_order` | 4,789,272,400 | 4,795,159,087 | +0.1% |

Of the 29 cases the suite had before, the largest rise is
`datetime_distinct_days` at +0.62% and `datetime_groups` at +0.60%; the
rest are within half a percent, and `count_distinct` falls 1.8%. The
first head measured `datetime_distinct_days` at +3.15%: the in-place
keys' insert, inlined, widened the loop a large set inserts in, and it
was moved out of line. The nine rows above were counted before that
move and the 29 existing cases after it.
