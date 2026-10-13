# Fixed and avoidable cost on the query path

Where a statement's time goes that is not the work it asked for, measured
on the 20M-row benchmark replica, and what removing three of those costs
bought, in two changes.

- Base `76371ad4`. Head is base plus two changes to the store, named
  below by what they do. Release builds, each in its own target directory,
  one replica copy per arm.
- Host: 8-vCPU cloud host (AMD Ryzen 9 9950X), 16 GB.
- Data: `benchmark/schema.sql` and `benchmark/seed.sql` at full scale (20M
  orders, 100,000 users, 10,000 products) in MySQL 8.4, copied by a native
  snapshot. The orders table is 74 segments.
- Server: result memo off (`PINTAIL_DISABLE_SETTLED_MEMO=1`), 4 GiB query
  ceiling, default compaction; every measurement started after the
  server's idle CPU had fallen to zero.
- Client: the MySQL wire protocol on loopback, one connection per client.
- Every answer of both arms equals MySQL's (rows compared as a set where
  the statement leaves ties unordered).

## Method

Four instruments, all on the base build first:

- **Sampling profile.** `perf record` on the server at 399 Hz with DWARF
  call graphs while one query runs back to back (and while the mixed
  workload runs at 8 and 32 clients). Shares below are of the server's
  on-CPU samples, counted inclusively by the frame that names the cost.
- **Statement trace.** `PINTAIL_QUERY_TRACE`: when each phase of a
  statement ended, in microseconds since it arrived.
- **Operator profile.** `PINTAIL_PROFILE=1`, for which operator held the
  time and how the scan folded.
- **Timing**, two shapes. *Interleaved*: 6 rounds × 15 cycles, one run of
  each query on each arm per cycle, order rotating (n = 90 per cell);
  wall median, minimum, and the server's CPU (utime + stime). *Back to
  back*: one connection sending one statement without pause for 3 s, arms
  alternating, 6 or 8 rounds; microseconds of wall and of server CPU per
  statement. The second shape is the instrument for a statement that
  takes a tenth of a millisecond: interleaved, such a statement mostly
  measures how long an idle thread takes to wake.

Noise floor: two builds of the base commit, interleaved the same way,
differed by 0.2-2.7% per query on the median and 5.1% on Q6.

## Where the time went (base)

Share of server CPU samples per query. A row is one cost; a sample can
sit in more than one row.

| cost | Q1 | Q2 | Q3 | Q4 | Q5 | Q6 | Q7 | Q8 | mixed ×8 | mixed ×32 |
|---|---|---|---|---|---|---|---|---|---|---|
| threads sleeping and waking (futex) | 17.4 | 12.9 | 10.5 | 9.2 | 10.4 | 10.9 | 10.4 | 9.1 | 8.7 | 16.2 |
| idle scan workers spinning (yield, steal) | 0 | 11.7 | 7.0 | 6.5 | 5.4 | 5.4 | 4.2 | 4.0 | 5.1 | 3.7 |
| reading blocks out of the page cache | 0 | 0.9 | 12.2 | 9.8 | 7.2 | 10.9 | 4.4 | 15.9 | 9.1 | 8.0 |
| block checksums | 0 | 0.2 | 1.6 | 1.3 | 1.0 | 1.5 | 0.8 | 2.3 | 1.3 | 1.0 |
| opening the scan | 28.1 | 0.5 | 0.2 | 0.2 | 0.1 | 0.2 | 0.1 | 0.1 | 0.2 | 0.3 |
| `stat` of segment files | 0 | 5.7 | 1.3 | 1.1 | 1.3 | 0.8 | 1.8 | 1.6 | 1.2 | 0.9 |
| opening segment files | 0 | 2.6 | 0.7 | 0.3 | 0.7 | 0.4 | 0.5 | 0.7 | 0.5 | 0.4 |
| allocator | 13.6 | 4.8 | 4.0 | 3.1 | 6.3 | 3.9 | 5.9 | 4.2 | 4.2 | 4.5 |
| the per-statement log line | 6.9 | 1.4 | 1.1 | 1.2 | 1.0 | 1.3 | 0.5 | 0.6 | 0.9 | 0.8 |

The sleeping-and-waking row overstates itself: part of every context
switch sampled is the profiler's own.

Phase ends from the statement trace (base, medians of 7, microseconds
since arrival), for the fixed part of a statement:

| | handed to a worker | plan found, admitted | scans open | executed | answered |
|---|---|---|---|---|---|
| Q1 | 9 | 18 | 82 | 91 | 113 |
| Q2 | 9 | 16 | 76 | 1,501 | 1,521 |
| Q3 | 19 | 36 | 192 | 10,205 | 10,286 |
| Q8 | 27 | 49 | 233 | 17,699 | 17,780 |

Parse, bind and plan do not appear: a repeated statement runs from its
kept plan, found in 3-13 µs. Result encoding is 1-10 µs. Opening the
scans was the largest fixed phase of every statement, 60 µs at the least
and 150-190 µs for Q3-Q8.

## Ranked

1. **Scan workers sleeping, waking and spinning**: 13-25% of CPU samples
   on every query that scans. A statement runs several parallel rounds;
   each wakes the pool and lets it go back to sleep. Not changed here.
2. **Blocks read from the page cache on every statement**: 5-18% on
   Q3-Q8 with their checksums. An uncompressed block is copied out of the
   kernel each time it is read; the block cache deliberately does not
   hold such blocks. Not changed here.
3. **Opening a scan**: 28% of Q1, 60-190 µs of every statement. Most of
   it was copying each segment's manifest entry - keys, file name, key
   filter, per-column aggregates and sketches - once or twice per segment
   per statement. Fixed (first change).
4. **A `stat` per column of every slice**: 5.7% of Q2, 0.8-1.8% elsewhere.
   Every cache of segment contents names the file by its length and
   modification time, and asked the file system each time. Fixed (second
   change): once per segment per scan.
5. **An `open` and a `close` per slice**: 2.6% of Q2 (3.5% once the stats
   were gone), 0.3-0.7% elsewhere. Fixed (second change): a scan keeps
   the handle.
6. **Per-slice lookups in process-wide caches keyed by path** (footer
   verified, block directory, block-cache slot, postings, text values):
   about 6% of Q2 after the fixes, each a path hashed under a shared
   lock. Not changed.
7. **A worker hand-off, a log line, a plan copy and the session settings
   per statement**: about 15%, 7%, 3% and 3% of Q1. Not changed.
8. **First filtered read of a text column after a restart**: 94-395 ms
   for Q2's first execution against 2.5 ms after, the column's postings
   being rebuilt in memory. Not changed.

## What each change bought

Back to back (µs per statement, median / minimum, and server CPU per
statement). The first row is base against the build with the first
change; the second is that build against head, 8 rounds for Q2 and 12
for Q1:

| change | query | before | after | CPU before | CPU after |
|---|---|---|---|---|---|
| a scan shares a segment's manifest entry | Q1 | 103.9 / 100.4 | 77.5 / 70.2 | 89.9 | 66.4 |
| a scan asks about and opens each segment file once | Q2 | 1,843 / 1,726 | 1,710 / 1,613 | 9,589 | 8,372 |
| | Q1 | 76.8 / 70.1 | 79.3 / 74.7 | 65.3 | 67.8 |

The second change gives Q1 nothing to gain - it reads no file - and what
it shows there, +3%, is inside the floor.

Base against head, interleaved (ms, median / minimum / CPU):

| | base | head | median |
|---|---|---|---|
| Q1 | 0.629 / 0.430 / 0.2 | 0.535 / 0.379 / 0.2 | **-15%** |
| Q2 | 2.887 / 2.261 / 13.9 | 2.675 / 2.020 / 11.3 | **-7%** |
| Q3 | 13.47 / 10.68 / 79.0 | 13.09 / 10.68 / 79.0 | -2.8% |
| Q4 | 18.32 / 15.40 / 113.0 | 17.69 / 14.91 / 109.6 | -3.4% |
| Q5 | 17.44 / 13.73 / 105.3 | 17.01 / 13.70 / 102.2 | -2.5% |
| Q6 | 29.97 / 22.76 / 176.6 | 30.96 / 22.10 / 175.3 | +3.3% |
| Q7 | 49.42 / 35.79 / 284.6 | 49.51 / 35.65 / 285.0 | +0.2% |
| Q8 | 18.64 / 15.15 / 88.0 | 18.29 / 14.82 / 86.6 | -1.8% |

Base against head, back to back (µs per statement, 8 rounds):

| | base median / min | head median / min | CPU base | CPU head |
|---|---|---|---|---|
| Q1 | 112.4 / 100.0 | 85.4 / 77.5 | 96.9 | 73.1 |
| Q2 | 2,461 / 2,141 | 2,125 / 1,775 | 11,210 | 9,698 |

The host was in its slower state for these two tables; both arms of a
row ran in the same minutes.

Scans open (trace, µs between the plan being ready and the scans open):
Q1 64 → 38, Q2 60 → 42, Q3 156 → 116, Q4 169 → 102, Q5 170 → 106,
Q6 183 → 120, Q7 186 → 120, Q8 184 → 122.

Concurrency. Q1 alone, statements per second (three 5 s rounds each,
sorted):

| clients | base | head |
|---|---|---|
| 1 | 6,504 / 6,853 / 8,398 | 8,363 / 8,458 / 9,301 |
| 8 | 35,967 / 42,110 / 44,852 | 52,857 / 58,453 / 59,437 |
| 32 | 63,674 / 66,080 / 66,908 | 67,851 / 68,771 / 74,513 |

The mixed workload (Q2-Q8 round-robin per call; median of five 8 s
rounds, statements per second): 47.8 → 47.2 at 1 client, 65.1 → 66.6 at
4, 128.8 → 125.2 at 8, 187.1 → 222.2 at 16, 464.6 → 425.3 at 32. Rounds
of one arm spread by 15-30%, so none of these is a difference.

First executions after a restart are unchanged: Q1 3.0-3.4 ms and Q2
94-109 ms on both arms.

## Reading

- Q1 costs a quarter less back to back, in wall and in server CPU, and
  at eight clients answers a third more statements a second. At 32
  clients the single client process is the limit.
- Q2 is 7% faster interleaved and 14% back to back, with 13% less CPU.
  It is the query that decodes the least per slice, so what was paid per
  slice was the largest share of it.
- Q3-Q8 moved by -3.4% to +3.3%, which is the floor: no claim. Their
  fixed part fell by 40-70 µs a statement, under 1% of any of them.
- The mixed workload did not move. It runs with seven of eight cores
  busy; its throughput is the CPU a statement needs, and that is the
  scan itself, the page-cache reads and the pool's waking and sleeping.

## Tried and dropped

- **The file's identity remembered by the manifest entry**, for as long
  as the manifest is loaded, instead of by the scan. Faster: in one
  session Q2 ran 1,741 µs with the first change alone, 1,625 with the
  identity kept per scan and 1,531 kept per entry, because a scan then
  makes no `stat` at all where it now makes one per segment. Dropped: a
  file changed in place under a loaded manifest would no longer be
  noticed until the table was opened again, and the store's tests say it
  is noticed at the next read.
- **The file handle kept by the manifest entry.** A loaded replica that
  is not queried again would keep removed segment files open, and their
  space with them. A scan's handles close with the scan.
- **A text filter's remembered answers read under a shared lock.** The
  answers sit behind one mutex and are looked up by an owned copy of the
  value (3% of Q2's samples, a third of it waiting). Reading them under a
  shared lock by the borrowed text changed nothing measurable (1,344 →
  1,374 µs back to back, CPU 7,362 → 7,355).

## Not changed, with what each is worth

- Page-cache reads and checksums: 5-18% of Q3-Q8's CPU. Needs either a
  mapping of segment files or a block cache sized to hold raw blocks.
- Pool wake and sleep per parallel round: 13-25% of samples, the real
  share unknown until measured without a profiler's context-switch cost.
- The `stat` per segment per scan that remains, and the per-slice
  path-keyed cache lookups: about 6% of Q2 each, under 1% elsewhere.
- Running a statement answered from segment aggregates alone on the
  connection's own thread, as a small read is: about 15% of Q1.
- The log line per statement (written by another thread): 7% of the CPU
  Q1 uses, none of its latency.
- Postings rebuilt on first use after a restart: about 0.1 s, once 0.4 s,
  on the first filtered read of a text column of 20M rows.
- A zeroed buffer per translated code chunk (2.6% of Q3) and per fold
  worker's accumulators (1.9% of Q6).
