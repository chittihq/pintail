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

## 2026-10-14: the next layer

What the items left above truly cost, measured without a sampling
profiler, and the one change that came of it.

- Base `8a98276f`; head is base plus one change to how a scan folded in
  place is cut into rounds. Release builds, each in its own target
  directory, one replica copy per arm.
- Host: 8-vCPU cloud host (AMD Ryzen 9 9950X), 16 GB, one thread per core.
  Its speed moved by up to half between sessions over the four hours, so
  every pair below ran interleaved in the same minutes and no figure is
  compared across tables.
- Data, server settings, client and the two timing shapes as above; five
  or six rounds each. Every answer of both arms equals MySQL's.
- Noise floor: two builds of base, interleaved 6 × 15, differed by
  0.4-3.0% per query on the median; two more same-binary pairs gave up
  to 5%, and 7% on Q1.

### What the parallel rounds cost

The profile's "sleeping, waking and spinning" rows were measured again
with counters that do not sample: `perf stat` on the server and the
per-thread context-switch counts in `/proc`, one query back to back, the
pools at eight threads and at one.

| per statement | Q2, 8 threads | Q2, 1 | Q3, 8 | Q3, 1 | Q6, 8 | Q6, 1 | Q8, 8 | Q8, 1 |
|---|---|---|---|---|---|---|---|---|
| user instructions (M) | 71 | 91 | 564 | 578 | 830 | 796 | 906 | 928 |
| kernel instructions (M) | 13.2 | 8.3 | 27.7 | 13.9 | 38.3 | 20.4 | 27.4 | 18.7 |
| user cycles (M) | 29 | 27 | 264 | 165 | 635 | 262 | 310 | 250 |
| kernel cycles (M) | 9.2 | 6.1 | 75 | 41 | 145 | 55 | 100 | 51 |
| `sched_yield` calls | 4,046 | 1,798 | 6,940 | 1,860 | 9,052 | 1,995 | 5,364 | 1,799 |
| `futex` calls | 346 | 226 | 517 | 233 | 670 | 250 | 399 | 226 |
| context switches | 167 | 116 | 443 | 120 | 428 | 129 | 242 | 117 |

- Workers that spin execute almost nothing in user space: a statement
  runs the same user instructions on eight threads as on one.
- What eight threads add in the kernel - yields, sleeps, wakes, and reads
  slowed by seven neighbours - is at most the difference of the kernel
  cycle rows: 8% of Q2's cycles, a tenth of Q3's, an eighth of Q6's and
  Q8's. That is the true size of the profile's 13-25%.
- The processor time a statement costs does rise with the thread count
  (Q3: 62 ms on eight threads, 34 ms on one), but as cycles per
  instruction, not as work: eight cores reading the same memory at once.
  No change to the pool recovers that.
- A statement still ran only 2.3-4.5 times faster on eight threads than
  on one. What held it back was not the spinning but the ends of rounds.

A scan folded in place was read in rounds of four slices per thread, so
the 221 slices of the orders table took seven rounds. A round ends when
its last slice does, with the other workers idle, and the next starts by
waking them. Read in one round instead (an experiment build), Q2 ran 22%
faster, Q6 16%, Q8 14%, Q5 13%, Q4 10% and Q3 9%; widening the pulled
scan's prefetch the same way made Q3, Q4 and Q6 up to twice as slow, so
that path is left as it was.

### The change: long rounds after a short first one

The round's width bounded two things: the batches a round may hand back
to the caller, and how far it read. Only the first has to be small. The
store now hands slices out in the scan's order, one to a worker as it
finishes its last, and stops where the caller says, so the slices a
round decoded are always its first so many. The scan operator asks for
sixteen times as many slices after a round that handed nothing back,
ends a round once it has handed back as many batches as a short round
could, and goes past a short round's length only while no other scan is
folding.

Base against head, interleaved (ms, median / minimum / CPU). The host
was in a slow, uneven state for this table; both arms of a row ran in
the same minutes:

| | base | head | median | CPU |
|---|---|---|---|---|
| Q1 | 1.067 / 0.621 / 0.7 | 1.046 / 0.601 / 0.9 | -2.0% | |
| Q2 | 6.031 / 3.193 / 20.4 | 4.201 / 2.518 / 17.9 | **-30%** | -12% |
| Q3 | 22.16 / 14.56 / 115.7 | 17.95 / 13.33 / 103.1 | **-19%** | -11% |
| Q4 | 29.80 / 19.31 / 163.1 | 25.61 / 17.80 / 148.0 | **-14%** | -9% |
| Q5 | 28.00 / 19.08 / 151.7 | 23.27 / 15.91 / 139.2 | **-17%** | -8% |
| Q6 | 64.35 / 31.26 / 310.2 | 51.06 / 30.76 / 263.1 | **-21%** | -15% |
| Q7 | 86.63 / 55.74 / 471.4 | 88.32 / 62.81 / 466.1 | +2.0% | -1% |
| Q8 | 30.88 / 19.60 / 138.4 | 24.58 / 18.86 / 121.2 | **-20%** | -12% |

Back to back, in a calmer stretch (µs per statement, median / minimum;
CPU is user + system per statement; switches are voluntary +
involuntary):

| | base | head | median | CPU base | CPU head | switches base | head |
|---|---|---|---|---|---|---|---|
| Q1 | 91.3 / 88.9 | 97.3 / 89.3 | +7% | 77 | 81 | 4 | 4 |
| Q2 | 1,942 / 1,851 | 1,451 / 1,400 | **-25%** | 9,171 | 7,134 | 241 | 72 |
| Q3 | 13,334 / 12,615 | 12,470 / 12,311 | -6.5% | 77,532 | 74,914 | 595 | 383 |
| Q4 | 17,967 / 16,488 | 15,708 / 15,335 | **-13%** | 110,359 | 100,998 | 591 | 324 |
| Q5 | 17,048 / 15,896 | 15,088 / 14,611 | **-11%** | 102,159 | 93,881 | 570 | 339 |
| Q6 | 44,245 / 36,805 | 33,062 / 28,442 | **-25%** | 230,650 | 180,550 | 610 | 432 |
| Q7 | 65,711 / 63,771 | 64,827 / 62,254 | -1.3% | 373,797 | 367,956 | 971 | 798 |
| Q8 | 23,624 / 19,197 | 18,770 / 18,426 | **-21%** | 102,992 | 87,213 | 365 | 89 |

Q1 reads no slice and is unchanged: its minimum is the same and its
rounds were 172, 111, 91, 91, 89 against 134, 111, 91, 97, 89. Q7 did not
move in any session: its filter keeps two rows in five and it counts
distinct users, and the time of its rounds is the work in them. An
earlier pair of the same change, in another session, read Q2 -16%,
Q3 -10%, Q4 -11%, Q5 -12%, Q6 -16%, Q7 +2%, Q8 -11%.

The mixed workload (Q2-Q8 round-robin per call, five 8 s rounds per arm,
arms alternating; statements per second as minimum / median / maximum,
then the median of the rounds' p95 in ms):

| clients | base | head | p95 base | p95 head |
|---|---|---|---|---|
| 1 | 23.4 / 34.7 / 36.7 | 28.7 / 32.8 / 39.7 | 67 | 75 |
| 4 | 37.0 / 46.5 / 52.6 | 45.0 / 48.3 / 55.5 | 207 | 194 |
| 8 | 78.3 / 94.3 / 99.8 | 82.0 / 99.1 / 103.5 | 205 | 187 |
| 16 | 152.2 / 159.7 / 177.0 | 142.6 / 183.9 / 201.3 | 250 | 202 |
| 32 | 230.8 / 323.9 / 361.8 | 256.1 / 302.4 / 329.8 | 246 | 262 |

The rounds of one arm spread by 20-55%, so no row is a difference: the
mixed workload neither gained nor lost. It runs with seven of eight
cores busy, where the workers one statement's round leaves idle are
another statement's to use.

### Tried and changed: long rounds whatever else is running

The first form of the change ran long rounds unconditionally. Alone, a
statement gained the same. Under the mixed workload the median latency
fell and the tail rose: at 8, 16 and 32 clients p50 went from 78, 84 and
101 ms to 72, 62 and 86, and p95 from 194, 232 and 304 ms to 256, 271
and 384. A short round is also how statements take turns - each round
queues behind the rounds others have asked for - and a round sixteen
times as long made a statement wait that much longer for its turn.
Hence the last clause above: with another scan folding, a round stops at
a short round's length, and the tail is where it was.

### Tried and dropped: raw blocks held in the block cache

The page-cache copy and the checksum were measured by removing them.

| experiment (interleaved median against the same build without it) | Q2 | Q3 | Q4 | Q5 | Q6 | Q7 | Q8 |
|---|---|---|---|---|---|---|---|
| checksum not computed at all | -3.2% | -3.4% | +0.4% | -3.1% | -3.5% | -2.6% | -1.8% |
| raw blocks held at once, 2 GiB budget | +1.2% | -6.9% | -2.9% | -3.9% | -4.0% | -4.8% | -3.6% |
| raw blocks held on the second ask, default budget (122 MiB) | +2.8% | -7.9% | -6.9% | -1.9% | -5.8% | -5.9% | -6.0% |
| the same, 16 MiB budget | +1.4% | +30% | +21% | +23% | +25% | +13% | +41% |
| held only in spare room, default budget, on top of the long rounds | -2.4% | -1.9% | -3.4% | -2.7% | -0.5% | -8.3% | -5.1% |
| the same, 16 MiB budget | +4.0% | +0.7% | -4.0% | +1.4% | -5.0% | -0.3% | +0.3% |

- Verifying a block's checksum once per file identity can gain at most
  the first row, which is the floor, less the lookup that would replace
  the hash. Not worth a weaker check.
- Holding raw blocks removes the system time (Q3: 16.2 ms a statement to
  3.5) and gives part of it back as user time, the decoder now reading
  memory the read used to bring into the core's cache (Q8: 64.8 ms to
  72.1). Net 4-8% of CPU and 3-8% of wall while everything the queries
  return to fits the budget, for 100-145 MB more resident memory.
- Where it does not fit - the fourth row, a budget a tenth of the
  scanned columns, which is where any table larger than the cache lives -
  every read copies a block in and pushes one out, and statements ran a
  fifth to two fifths slower, the mixed workload 20-30% slower.
- Admitting raw blocks only into room nothing else holds removes that
  loss (last row) and, after the long rounds, leaves 2-5% on five of the
  seven queries, inside or at the floor, with 124 MB more resident
  memory and a cache that keeps whatever filled it first. Dropped: the
  gain does not clear the floor on most queries and the cache's default
  budget is a decision about memory, not about scans.

### Not changed, with what each is worth

- The hand-off of a statement to a worker thread: `SELECT 1` answered on
  the connection's own thread takes 27 µs back to back and 46 µs when
  sent to a worker, so the hand-off is about 19 µs of Q1's 71-91 µs. To
  run a statement answered from segment aggregates in place, admission
  has to prove before executing that it will be - that every segment
  carries its aggregates, none overlaps another and the memtable holds
  only new keys - or a table scan runs on a connection's thread. Not
  attempted.
- The remaining `stat` per segment per scan and the path-keyed lookups
  per slice: not measured again. A scan could keep, beside the identity
  it already keeps per segment, the block directory and the cache slot
  found under that identity, which keeps an in-place change noticed at
  the next scan.
- The log line, the plan copy and the session settings of a statement:
  not measured again.
- Postings rebuilt on first use after a restart: not attempted; keeping
  them is a change to the segment format.
