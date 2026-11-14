# The first rows from a moment: a limited sort over input in its own order

`SELECT ... WHERE created_at >= <moment> ORDER BY created_at, id LIMIT 100`
over the filter benchmark's table (`benchmark/run-filters.ts`: 20,000,000
rows appended in time order, then one row in a hundred updated, one in a
thousand deleted and 99,900 appended). The scan hands the sort every row
from the moment on - between four and eleven million of them, for the
benchmark's five moments - and all but the first hundred order after the
hundredth the sort already holds.

Measured 2026-11-14 on an 8-vCPU, 16 GB box (AMD Ryzen 9 9950X), release
builds of `05694eaf` and of this change, the settled-answer memo off. Both
binaries ran at once, each on its own copy of the replica, one run of the
statement per binary per cycle with the order alternating, 6 rounds of 15
cycles over the five moments (overlapped: 6 rounds of 5). Every answer
equalled MySQL 8.4's row for row.

| First 100 rows from a moment | Before median | Before min | Before CPU | After median | After min | After CPU |
|---|---:|---:|---:|---:|---:|---:|
| Settled (67 segments, none overlapping) | 185 ms | 62 ms | 335 ms | 33 ms | 9.8 ms | 166 ms |
| Overlapped (compaction held off, 156 segments) | 376 ms | 223 ms | 619 ms | 168 ms | 71 ms | 410 ms |

CPU is the server process's, per statement. Statements this change does
not touch, measured in the same cycles: the one-day count read 1.0 ms
before and 1.1 ms after, the scattered-column count 7.2 ms and 7.5 ms, the
point lookup on a column that is not the key 5.7 ms and 5.7 ms. MySQL
answers the statement in 1.7 ms from its index on the column.

Before, the profile put the time in the sort, not the scan: 110-200 ms of
`Sort top_k` against 13-33 ms of scan. The sort narrowed each batch by
reading every row's key into a list, ranking the list for the batch's own
hundredth key and testing the rows one by one, and it held no cutoff from
the batches before until its first cut, thirty-two batches in. Now the
bound a batch was narrowed by is the cutoff for the batches after it, and
a batch whose packed key column lies wholly after the cutoff - seen from
its least and greatest value, one pass over the buffer - is left out
whole.

What remains is the scan: it still decodes every row from the moment on,
which is the 166 ms of CPU, spread over the scan's threads. The sort's
cutoff does not reach it; a scan that took the cutoff as a value bound
while it runs would skip the segments and blocks past it, as it does for a
bound the statement itself states.
