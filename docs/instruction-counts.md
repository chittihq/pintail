# Instruction counts

Wall-clock medians cannot decide a change of a few percent on a machine
whose speed moves between runs. `benchmark/instruction-suite` counts the
instructions each query shape executes instead: the same binary gives the
same count on every run, so a change of half a percent is a finding rather
than noise.

## What it measures

26 cases over an invented, generated dataset (400,000 orders with users
and products, 400,000 events), each one statement through parse, bind,
plan and execute over a real `TableStore`, in process: the timed
benchmark's Q1-Q8 and N1-N4 shapes, a two-column text filter, a
100,000-group aggregate, a star join, `COUNT(DISTINCT)`, `ORDER BY ...
LIMIT`, a correlated scalar subquery page, and day and hour aggregates
over a time window read as a `DATETIME` and as a `TIMESTAMP` in a session
zone of fixed offset and of a named zone.

Each case runs in its own process under Valgrind's Callgrind, driven by
`iai-callgrind`. The fixture is loaded and the statement run once with
instrumentation off; then instrumentation is switched on for one more
run. Loading is therefore not counted, and neither is anything built on
first use. The counted region is process-wide, not one function: scans run
on the store's own pool, and Callgrind's per-function toggle only sees the
thread that entered the function.

Every pool has one worker (`RAYON_NUM_THREADS=1`,
`PINTAIL_SCAN_THREADS=1`) and the result memo is off. With more workers,
which thread takes which piece of work differs between runs and the count
with it.

## Running it

Linux, with `valgrind` (its headers included), `libclang` and the runner:

```sh
cargo install iai-callgrind-runner --version 0.16.1 --locked
bun run scripts/instruction-suite.ts run --out counts.tsv
bun run scripts/instruction-suite.ts compare <rev-a> <rev-b>
bun run scripts/instruction-suite.ts diff a.tsv b.tsv
```

`run` measures the working tree and takes about three and a half minutes
once built. `compare` checks both revisions out next to the suite's target
directory, lays the working tree's suite over each so both engines answer
the same cases, builds each (about three minutes a revision, cold) and
prints one table. `diff` compares two saved runs, for example a plain and
a profile-guided build of the same revision: `run --target <dir>` builds
into a second target directory, so `RUSTFLAGS=-Cprofile-use=<profile>` can
be measured beside the plain build.

The suite is a crate outside the workspace, with its own lockfile. The
runner's client requests need Valgrind's headers and libclang at compile
time, which the gate's machines do not carry, so as a workspace member it
would break `clippy --workspace --all-targets` there; and standing apart
it cannot change the release binary's dependency graph. The price is that
`cargo fmt --all` and the workspace's clippy do not reach it: run both in
`benchmark/instruction-suite` after touching it.

`benchmark/instructions` is the older, smaller gate (four statements over
4,096 rows, counted on the calling thread, compared against a banked
baseline in CI). It stays as it is; this suite is the instrument for
deciding whether a change moved the engine.

## Reading it

- **instructions**: the count. Deterministic; the first thing to compare.
- **L1 misses**, **LL misses**: reads and fetches the simulated first-level
  and last-level caches missed. The simulation is of a fixed cache
  geometry, not of the machine.
- **estimated cycles**: instructions weighted by where the simulated cache
  served them. A change here with instructions flat means the same work
  touching memory differently.
- **answer**: a digest of the rows, from a native run of the same build.
  `DIFFERS` means the two builds do not agree on a case, and its counts
  are not comparable.

A case whose instructions, misses and estimated cycles are all flat
between two builds while its wall time moved is not doing more work: look
at code placement, the branch predictor and the machine before the logic.

## How repeatable it is

Three runs of one binary agree to within 0.1% on 14 of the 26 cases and
within 0.31% on the rest, except the full-table count: it executes about
160,000 instructions, and the 1,600 that vary are 1% of it. Treat a
difference under half a percent as no difference. Three things move a
count between runs of the same binary:

- **Hash tables seeded at random.** The engine's `std` hash sets and maps
  take a fresh seed per process, so the same keys probe differently. One
  such set in the aggregate's choice of path accounted for the whole
  spread of the case examined.
- **Waiting threads.** A worker looks for work a few rounds before it
  sleeps, and how many rounds depends on when the other thread hands over.
  A few thousand instructions.
- **Buffer addresses.** The C library copies by different paths for
  different alignments. With one allocator arena per thread this moved two
  cases by 1%; the suite sets `MALLOC_ARENA_MAX=1`, which removed it.

The suite uses the system allocator, not the server's. Counts inside the
allocator are therefore not the server's.

## What it cannot see

- **Scaling across threads.** One worker per pool by construction.
  Contention, work stealing and a parallel plan's merge step are absent.
- **I/O and the server.** No wire protocol, HTTP, admission, replication
  or disk wait; segments are read from the page cache.
- **The real cache and branch predictor.** The simulated cache has no
  prefetcher and no other tenant, and branch misprediction is not
  modelled. An effect of code alignment on the real front end is exactly
  what this suite does not show, which is how it tells such an effect
  from added work.
- **The machine's shape.** The planner sizes some partitions from the CPU
  count, so compare counts taken on machines of the same shape, with the
  same compiler and Valgrind.
- **Small tables only.** A cost that appears at twenty million rows and
  not at four hundred thousand is the timed benchmark's to find.
