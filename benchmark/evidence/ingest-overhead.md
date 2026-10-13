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

## After: replication lag and apply throughput

Measured 2026-10-13 on an 8-vCPU cloud host (AMD Ryzen 9 9950X), 16 GB,
same source and tables as above, with a rewritten harness; "before" is the
binary the section above calls "after", measured again by this harness.
Four release binaries, one per step, each on a fresh replica, three runs
each in rotating order. Merges are held and the result memo is off, as
above. The host runs in one of two speed states about a third apart, so
each row gives the median and the best run.

### What was measured

- **Trickle**: 50 transactions a second for 30 seconds, each inserting one
  row into a small probe table and updating one row of `orders` (100 row
  changes a second). Lag is the time from the source's commit
  acknowledgement to the first replica answer that includes the row; the
  replica is asked every 2 ms.
- **Burst**: the 1,344,678 row changes of the section above, written on
  four connections (the source finishes in about 1.3 s).
- **Drain**: two bursts (2,689,356 row changes) written while the replica
  is stopped; the clock runs from process start until the replica matches.
  This is the rate the mirror applies at with the source out of the way,
  and so the most it keeps up with.
- **Idle**: process CPU over 30 seconds with no source writes.

The burst and drain figures include a query against the replica every
20 ms, which is how the match is detected.

### Where the lag came from

1. **Waiting, not work.** A cycle read the source's log to its end and
   returned; the next connected a cadence (five seconds) later. Under the
   trickle the lag was uniform between zero and one cadence: median 2.5 s,
   maximum 5.0 s, on a replica using under 1% of a core.
2. **Decode and store took turns on one thread.** One profiled burst cycle
   spent 1.7 s decoding row images, 3.8 s storing them, 0.4 s in log syncs
   and 0.06 s in checkpoint commits; the stream was unread while a batch
   was stored. Batches were also one transaction each, so two tables
   written by different transactions were never stored side by side.
3. **Serial passes inside a flush.** With column encoding already spread
   over the ingest threads, about a third of the storing thread's samples
   were the per-column statistics and the fixed-width probe of a flushed
   segment, both of which parse every date, datetime and decimal from text.

### The changes

- **follow**: a cycle asks the source to hold the stream open and stays on
  it for one cadence, storing each transaction as it arrives; it ends
  where the stream is quiet and between transactions, and the next cycle
  for that database starts at once. Checkpoint order is unchanged.
- **read ahead**: a batch of 4,096 rows or more is stored on another
  thread while the stream is read and decoded ahead of it into the next
  batch. One batch is stored and checkpointed at a time.
- **statistics**: a spread segment write computes its statistics and its
  fixed-width probes a column to a thread.

### Results

Median of three runs (best run in brackets). Each column includes the
steps to its left.

| | before | follow | + read ahead | + statistics |
|---|---|---|---|---|
| trickle lag, p50 | 2,514 ms | 4.7 ms | 4.0 ms | 4.0 ms |
| trickle lag, p95 | 4,760 ms | 10.2 ms | 6.5 ms | 7.2 ms |
| trickle lag, p99 | 4,960 ms | 73 ms | 37 ms | 93 ms |
| trickle lag, maximum (three runs) | 5,001, 5,033, 5,058 ms | 132, 233, 433 ms | 54, 94, 166 ms | 133, 155, 833 ms |
| burst: replica matched after | 9.39 s (8.96) | 5.99 s (5.33) | 4.64 s (3.95) | 3.80 s (3.75) |
| burst: lag after the last source write | 8.07 s (7.74) | 4.52 s (3.95) | 3.07 s (2.73) | 2.52 s (2.39) |
| burst: process CPU | 17.6 s | 18.2 s | 14.9 s | 12.4 s |
| drain: time to match | 11.96 s (11.82) | 15.25 s (12.03) | 10.02 s (8.86) | 8.60 s (8.25) |
| drain: row changes per second | 224,950 (227,489) | 176,396 (223,622) | 268,509 (303,683) | 312,576 (326,194) |
| idle CPU, share of one core | 0.80% | 0.83% | 0.77% | 0.77% |
| peak resident memory | 1,036 MB | 1,256 MB | 1,134 MB | 1,217 MB |

Every run ended with the replica equal to the source on `COUNT(*)`,
`SUM(quantity)` and `SUM(id)` over `orders` and on the probe table.

Reading it:

- The trickle lag is now the cost of storing one transaction: a few
  milliseconds. The tail is the gap between two cycles (close the stream,
  reopen the tables, connect again), once per cadence; the largest single
  figure, 833 ms, is one run's.
- Following does not change how fast rows are applied, and the drain
  shows it: its best run equals the baseline's and its median is two runs
  in the host's slower state. The burst improves under it only because the
  wait for the next cycle is gone.
- Reading ahead and the statistics change raise the apply rate: the drain
  takes 0.70 times as long at best (11.82 s to 8.25 s), about 1.4 times
  the row changes per second, for less CPU.
- Idle cost is unchanged: a followed stream waits on its socket.

### Not done

- The gap between cycles is still paid once per cadence. Holding the
  tables and the connection across cycles, or following for longer than
  one cadence, would remove most of the tail; the end of a cycle is also
  where a database's cascade repair and automatic resync are scheduled,
  so its cadence was left alone.
- Storing one table's batch is still one thread apart from column
  encoding: log encoding, the memtable insert and the pivot of rows into
  columns. A burst on a single table is bounded by it.
- Read-ahead stops at the first event that needs the tables or the
  control plane, including any row for a paused, quarantined or fenced
  table; a database with one such busy table applies as it did before.
- A row change is still built as one heap value per cell, with dates,
  datetimes and decimals as text that the flush parses back (three times
  per value: probe, encode, statistics).
- A transaction stored alone costs one apply-intent write, one log sync
  per table and one checkpoint commit, about 4 ms here. Under a steady
  stream of small transactions that arrive one at a time, that is the
  ceiling on transactions per second.
