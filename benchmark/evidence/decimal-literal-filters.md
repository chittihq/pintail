# Filters on an updated table, and a DECIMAL column against a decimal literal

The filter benchmark (`benchmark/run-filters.ts`: 20,000,000 rows, then one
row in a hundred updated, one in a thousand deleted and 99,900 appended)
was run again on `05694eaf` to see which of the rows recorded on
2026-09-10 are still slow, and one of them was fixed here.

Measured 2026-11-14 on an 8-vCPU, 16 GB box (AMD Ryzen 9 9950X), release
build, Pintail run natively beside a MySQL 8.4 container (8 CPUs, 8 GB,
4 GB buffer pool, the benchmark's secondary indexes). The settled-answer
memo is off. The statements, their constants and the mutation pass are
`run-filters.ts`'s own; every answer was compared with MySQL's row for row
and every one was exact, in every run below.

## Where the benchmark stands

Each statement runs twice per constant and the second run is timed; the
figure is the median over five constants, in milliseconds. "Overlapped"
is the replica with compaction held off (`PINTAIL_COMPACTION_INPUT_ROWS=1`),
so the changes stay layered over the snapshot's segments for good: 156
segments. "Settled" is the same replica left alone: background compaction
folded the changes in while the benchmark's first pass ran, within three
minutes of the last change, leaving 67 segments that do not overlap.

| Filter | MySQL | 2026-09-10 | Overlapped, before | Overlapped, after | Settled, before | Settled, after |
|---|---:|---:|---:|---:|---:|---:|
| created_at, one hour: count | 1.3 | 466 | 57 | 58 | 1.3 | 2.0 |
| created_at, one day: count and sum | 38 | 872 | 75 | 76 | 1.4 | 1.0 |
| created_at, one day: newest 50 rows | 1.4 | 1,272 | 128 | 120 | 5.4 | 7.2 |
| created_at, one day: every row, three columns | 60 | 1,030 | 98 | 95 | 17 | 19 |
| created_at, thirty days: per state | 1,627 | 3,913 | 94 | 91 | 13 | 6.9 |
| created_at, one day, and a state | 48 | 701 | 74 | 71 | 1.2 | 1.8 |
| created_at from a moment: first 100 rows | 1.7 | 31,401 | 282 | 103 | 207 | 43 |
| scheduled_at (scattered), one day: count | 8.6 | 48,033 | 16 | 12 | 8.1 | 9.8 |
| account_id point: every row | 1.9 | 66,490 | 8.6 | 10 | 8.1 | 11 |
| account_id IN 100 ids: count | 4.0 | 80,944 | 15 | 17 | 7.0 | 6.9 |
| primary key point | 0.8 | 6.7 | 3.3 | 3.6 | 0.7 | 0.8 |
| primary key range of 10,000: sum | 3.6 | 23 | 13 | 14 | 1.0 | 0.8 |
| state = an updated value: count | 518 | 27,727 | 12 | 11 | 15 | 19 |
| amount range, no index anywhere: count | 1,989 | 33,566 | 3,205 | 11 | 3,344 | 5.4 |
| DATE(created_at) = a day: count | 1,409 | 605 | 57 | 58 | 0.3 | 0.9 |
| note LIKE a prefix, and a channel: count | 3,476 | 36,245 | 123 | 129 | 51 | 90 |
| note IS NULL within a week: count | 368 | 1,084 | 78 | 66 | 3.3 | 5.5 |
| states, amount and a month together: count | 1,484 | 4,212 | 135 | 110 | 5.8 | 3.6 |

The 2026-09-10 column is `benchmark/results-filters.md`, from another host,
there for the order of magnitude only. A single pass of five constants is
noisy at a few milliseconds; the rows that changed are measured properly
below.

The rows that took tens of seconds no longer do. A scan over overlapped
segments reads each base segment where it is stored and masks the rows a
newer segment supersedes, a segment prunes on its values when everything
overlapping it is newer, blocks skip on their own value bounds, and a
lookup on a column that is not the key goes through the side index, under
overlap as well: the point lookup reads 100 rows in 9 ms overlapped and
8 ms settled, the IN list of 100 values 15 ms and 7 ms. What is left of
those two is one block decoded per matching row, and it is within a small
multiple of MySQL's index lookup.

Two rows were still slow, overlapped or not, and neither was slow because
of the overlap: the amount range, which this change fixes, and the first
100 rows from a moment (`limited-sort-ordered-input.md`).

## A decimal literal

`amount BETWEEN 100.00 AND 101.50` over a `DECIMAL(10,2)` column took
3.3 s for 20,000,000 rows where a range written in whole numbers took
95 ms.
A decimal literal travels as text, and the step that reads a string
literal beside an exact number as a double took it for a string: the
comparison became one of doubles, no packed kernel answers a DECIMAL
column against a double, and the filter went row by row - each value
spelled as text and parsed as a double, on one thread. It also compared
decimals as doubles, which cannot tell apart two that differ past the
fifteenth digit: over `DECIMAL(20,2)`, `amount > 12345678901234567.89` did
not find `12345678901234567.90`.

The literal now keeps its type, the comparison is the exact one, and a
DECIMAL column held as 64-bit units is compared with the literal at its
scale as integers.

Both binaries, each on its own copy of the replica, one run of the
statement per binary per cycle with the order alternating, 6 rounds of 15
cycles over five constants (overlapped: 6 rounds of 5):

| Amount range: count | Before median | Before min | Before CPU | After median | After min | After CPU |
|---|---:|---:|---:|---:|---:|---:|
| Settled | 3,686 ms | 3,175 ms | 3,806 ms | 5.9 ms | 3.5 ms | 31 ms |
| Overlapped | 3,564 ms | 3,400 ms | 3,818 ms | 15.5 ms | 11.3 ms | 84 ms |

CPU is the server process's, per statement. The statements this change
does not touch, measured in the same cycles, are the floor: the one-day
count read 1.0 ms and 1.1 ms, the scattered-column count 7.2 ms and
7.5 ms, the point lookup 5.7 ms and 5.7 ms.

The single passes of the first table moved some rows this change does not
touch - the `LIKE` row read 51 ms before and 90 ms after. Measured the
same interleaved way on the settled replica they are equal: the `LIKE` row
97 ms and 92 ms, the count of an updated state 17.1 ms and 17.5 ms, one
day's rows 14.7 ms and 14.2 ms, the week's NULLs 8.1 ms and 7.9 ms, the
newest fifty rows 5.5 ms and 5.5 ms.
