# Ingest overhead: initial copy and CDC apply

Measured 2026-10-13 on an 8-vCPU cloud host (AMD Ryzen 9 9950X), 16 GB,
against MySQL 8.4 in a container on the same host (3 GiB buffer pool, row
binlog, FULL metadata, GTID). Release builds, one before the change and one
after it, each with its own binary.

## Workload

- `orders`, `users`, `products` from `benchmark/schema.sql` and
  `benchmark/seed.sql`: 20,000,000 + 100,000 + 10,000 rows.
- `wide_events`, an invented wide table: a `BIGINT` key and 59 further
  columns (20 `INT`, 10 `DECIMAL(12,2)`, 12 nullable `VARCHAR(40)`, 8
  `DATETIME`, 6 `DOUBLE`, 3 nullable `TEXT`), 610,637 rows.
- **Copy**: a fresh replica snapshots the database; the clock runs from the
  snapshot request until the database reports `streaming`. 20,720,637 rows.
- **Burst**: once the replica is idle and matches the source, 20
  transactions update a 100,000-key range of `orders` each and 10 update a
  20,000-key range of `wide_events` each: 1,344,678 row changes. The clock
  runs from the first statement until `COUNT(*)` and a `SUM` over both
  tables read the same on the replica as on the source.

CPU is the server process's user plus system time over the phase; memory is
its peak resident set at the end of the phase. Background merges are held
(`PINTAIL_COMPACTION_INPUT_ROWS=1`) on both arms so the burst starts from
an idle process, and the result memo is off.

## Where the time went before

The copy of the large table ran on one thread: 62 s of CPU in 65 s of wall.
A chunk of 100,000 rows took about 55 ms to arrive from the source and 270
to 680 ms to convert and write, so the source was idle five sixths of the
time. A sampled profile of the copy put about a third of the samples in
segment encoding (`encode_packed`, `write_column`, `write_block`, the
per-segment statistics, and re-reading dates, datetimes and decimals from
their text form into fixed-width units), about a fifth in row conversion
(building each value, formatting dates and decimals as text) and about a
tenth in the allocator.

## The change

The two parts of a chunk that are independent by construction now run across
a pool of ingest threads (half the machine's threads, at most eight; four
here): each source row converts on its own, and each column of the segment
encodes on its own and is written in schema order, so the file is
byte-identical to the one a single thread writes. A memtable flush encodes
its columns the same way. Merges are untouched.

## Results

Three runs of each binary, alternating, order rotating, a fresh replica
each time.

| Copy, 20,720,637 rows | before | after |
|---|---|---|
| wall, median (runs) | 60.2 s (64.7, 60.2, 59.8) | 32.0 s (32.0, 32.9, 31.9) |
| rows per second, median | 344,008 | 646,570 |
| process CPU, median | 58.9 s | 61.1 s |
| peak resident memory, median | 484 MB | 640 MB |

The copy takes 0.53 times as long for the same CPU, and holds about 150 MB
more while four threads each carry a column's encoded bytes.

| Burst, 1,344,678 row changes | before | after |
|---|---|---|
| source finished writing after, median | 3.9 s | 3.8 s |
| replica matched the source after, median (runs) | 8.4 s (8.5, 7.8, 8.4) | 8.9 s (9.3, 8.9, 8.3) |
| lag after the last source write, median | 4.6 s | 5.2 s |
| row changes per second to matched, median | 160,520 | 151,821 |
| process CPU, median | 9.0 s | 9.4 s |

The burst did not move: the differences are inside the spread of the runs.
What bounds it is not encoding. The stream reads in finite cycles five
seconds apart, so the lag after the last write is mostly the wait for the
cycle that reads the tail, and within a cycle the work is decode, memtable
insert and log write on one thread. One profiled cycle before the change
(merges running) spent 1.5 s decoding the 1,344,678 row images, 4.2 s
storing them and 0.3 s in fsync.

## Not done

- A copied row is still built as one heap value per cell, with dates,
  datetimes and decimals formatted as text and parsed back when the segment
  is written. Carrying those as fixed-width units from the source row to
  the segment would remove most of the remaining conversion and allocation.
- The fetch of the next chunk does not overlap the write of the current
  one; at about 55 ms in 300 it is the smaller part.
- CDC apply is single-threaded per database and paced by the five-second
  cycle.
