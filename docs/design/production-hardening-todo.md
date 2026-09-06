# Production hardening todo

Opened 2026-09-06. The date-predicate rewrite in 0.1.2-rc4 did what it
promised: a reporting query that had taken tens of seconds now answers in
about a second. Running that release then surfaced three further failures
within ten minutes, none of which any gate could have caught. This list is
those failures, their fixes, and the test gaps that let them through.

The triggering workload, described generically because that is all the
engine needs to know: several multi-CTE reports, each joining about ten
tables with `LEFT JOIN` chains, grouping per entity with `COUNT(DISTINCT)`
aggregates, issued concurrently one per entity. Each ran 10-25 seconds.
Reproductions use invented schemas of the same shape.

Ordering rule: a production failure outranks the test that would have
caught it, but every fix lands with the gate that proves it, in the same
slice. No item is done until something fails when the product breaks.

## A. The three failures

Observed on a four-core container with a 12 GiB memory cap, a 1.5 GiB
per-query ceiling and a 9 GiB shared budget.

| # | Client error | Cause established |
|---|---|---|
| A1 | `HY000 ... aggregate spill create: Too many open files (os error 24)` | 1024 descriptor soft limit; a spilling grouped aggregation holds one run file open per run until its final merge |
| A2 | `ER_CON_COUNT_ERROR 1040 08004: too many concurrent queries` | admission saturated by concurrent long reports |
| A3 | `HY000 ... server memory limit exceeded: 9662624953 bytes used, 1052560 requested, 9663676416 limit` | the shared budget read full while the process held 325 MiB resident |

## B. Configuration the deployment set and the product ignored

- [x] **B1. The container shipped a desktop descriptor limit.** Docker's
  default soft limit is 1024. `docker-compose.yml` set no `ulimits`, so the
  shipped deployment ran a columnar engine under it. Fixed in `f394111`
  and recorded in `docs/limitations.md`. Reaches a deployment only on its
  next redeploy. The review found that fix incomplete: `scripts/install.sh`
  generates its own compose file, which also had no `ulimits`, and a
  re-run only moves the image tag, so an existing install would never have
  received it. The template now sets the limit and a re-run warns when an
  older file lacks it.
- [x] **B2. `PINTAIL_MAX_CONCURRENT_QUERIES` never reached the server.**
  The compose file passes ten other `PINTAIL_*` keys through and silently
  dropped this one, so a deployment that configured it ran the default
  (`cores * 4`, minimum 16) instead. Every other knob is honoured; this one
  read as configured and was not. Pass it through, and add a compose/env
  consistency check so a dropped knob fails a gate rather than a dashboard.
- [x] **B3. Nothing reported the settings actually in force.** A dropped
  environment variable is invisible today. Log one line at startup naming
  the effective admission limit, per-query ceiling, shared budget, process
  memory ceiling, descriptor soft and hard limits, and spill directory and
  its ceilings. That line makes B1 and B2 self-evident in the first minute.
- [x] **B4. Raise the soft descriptor limit at startup, after the engine
  fixes.** Best-effort, to a documented finite target capped by the
  inherited hard limit, never lowering a higher soft limit and never
  touching the hard limit, with an opt-out and an honest report on
  failure. This does nothing under the updated compose file, where soft
  and hard are already equal, but it covers direct-binary and other
  deployment paths. Not a substitute for bounded spill behaviour.
  Done: the binary raises its soft limit toward 65,536, capped by the hard
  limit, never lowering and never touching the hard limit; a refusal is
  logged and the limits line reports what the kernel actually holds.
  `PINTAIL_KEEP_OPEN_FILE_LIMIT=1` opts out.

## C. Engine: bound what a spilling query holds

Design settled by the review of 2026-09-06. Keep the existing sorted-run
format, close each run after writing, and merge in bounded passes of at
most K runs. Intermediate passes copy serialized records and do NOT
combine aggregate states; only the final pass combines. That matters:
combining is not idempotent and only conditionally associative. Replaying
a partial COUNT counts it again, decimal accumulators can overflow under a
different grouping, floating-point sums change with parenthesization,
DISTINCT must preserve its `seen` set rather than add finished counts, and
equal extrema can change which representative survives. Copying records
preserves the existing final fold order, so no result changes.

Hash or radix partitioning was considered and rejected for now: the
encoded key and sorted-run format already exist, and partitioning does not
help a skewed group or one enormous DISTINCT set. It stays a later
performance project.

Reservation rule: closing an input descriptor must not release its spill
reservation, because the file still occupies disk. Each intermediate
output takes its own reservation, inputs and output are both charged while
both exist, and consumed files are deleted before their accounting is
released. A query that previously fit its spill quota can now fail on it,
and the merge must not bypass the quota to succeed.

Resulting descriptor bound: one writer during build, K+1 during
intermediate passes, K during the final merge, excluding upstream
operators.

- [x] **C1. Aggregate: closed runs and bounded merge passes.** Every
  `AggregateSpillRun` owns an open reader; the build loop appends runs with
  no bound, and the merge initializes and retains every head. Split the run
  into closed metadata plus an active cursor, close after a checked flush,
  and merge in chunks of K.
- [x] **C1b. Sort has the same defect.** `SpilledRun` retains its
  `BufReader` after writing, `materialize_with_spill` appends without
  bound, and `SpilledMerge::new` loads every head. Its comment treats input
  bytes over the memory ceiling as a sufficient bound, which is exactly the
  assumption this incident disproved. Same closed-run treatment.
  Done, both: one run machinery in the spill module (a writer that closes
  into a descriptor-free run, a reader that reopens on demand, a merge
  over at most the fan-in, and a reduction that copies records in passes
  and never combines them), with the aggregate combining partial states
  only in its final pass and in the order the runs were written. Per-query
  active and peak handle counters cover creation, reopen and close, and
  `EXPLAIN ANALYZE` prints the peak. The grace join creates a partition
  file on first append rather than up front, so an empty partition costs
  nothing.
- [x] **C1c. Grace join: a separate real bug, fix before bounding it.**
  The serve loop calls `reader()` on a build partition, consuming the
  writer; on overflow it drops that reader and `split_grace_partition`
  calls `reader()` on the same run again, which returns
  `"grace run read twice"`. The existing split unit test calls splitting
  directly on unread runs and so never crosses that transition. Separately,
  16 build plus 16 probe files are created up front and each split adds 32
  more, with `MAX_GRACE_DEPTH` bounding recursion depth rather than pending
  partitions. Pending sealed partitions should hold paths, not writers.
  Done: a run seals on first read and reopens from its path on every
  read, every run seals once probe routing finishes, and the serve-then-
  split sequence is a unit test. The up-front file count and the growth
  per split are unchanged and belong to C1.
- [x] **C1d. `two_pass.rs` does not spill at all.** Its partitions are
  in-memory buckets and maps, with no file creation. My earlier assumption
  that it shared the defect was wrong; nothing to do there.
  Amended by D3: it holds no descriptors, but it held its whole state,
  and a per-entity DISTINCT count over a large table failed at a ceiling
  smaller than that state instead of spilling. Done since: the partition
  maps spill as sorted runs through the shared machinery before any flush
  that finds the query past half its ceiling, the scatter window is sized
  so one flush's growth fits in the other half, and the maps are charged
  by measured growth so a spill hands back the distinct sets too. The
  remainder merges with the runs exactly as the other aggregate paths do.
- [x] **C2. Replace the linear k-way merge scan with a heap** while that
  code is open, if it is free to do so. Not done, on purpose: the merge
  now sees at most the fan-in of sixteen runs, so the linear scan is a
  sixteen-element loop and a heap would buy nothing measurable.
- [x] **C3. Understand the 30x reservation overestimate (A3).** The shared
  budget read within 0.02% of its ceiling across eight seconds of
  consecutive refusals while the process held 325 MiB resident. Refusals
  later stopped on their own, so the budget drains: this is
  over-reservation, not a permanent leak. Establish where a ten-way
  `LEFT JOIN` chain with no useful statistics inflates its estimate, and
  note that the arithmetic is structurally unsatisfiable regardless: the
  admission limit times the per-query ceiling can exceed the shared budget
  several times over, so under load the budget is always the binding
  constraint and refuses work the box could do. Either admission and the
  budget agree on a number, or the budget stops hard-refusing reservations
  the process is not actually holding.
  Measured: an in-process ten-way `LEFT JOIN` chain over an invented
  eleven-table schema with 850K rows reserved 718 MiB at its peak against
  a resident-set growth of about the same, so the reservations were real
  memory, not an estimate; the "30x" came from comparing a budget read
  under sixteen concurrent reports with a resident set read while idle.
  The structural half was the real defect: a build side refused by the
  process budget failed the query, where the same refusal from the query
  ceiling would have spilled. The hash join now treats a memory refusal
  from either ceiling as a spill signal whenever it has rows to spill, so
  under load a query slows down instead of failing, and the budget is a
  backpressure valve rather than a verdict. A test in its own process sets
  a 40 MiB budget under a 1 GiB query ceiling and checks the join spills
  and answers exactly. The admission-times-ceiling arithmetic still
  exceeds the budget on paper, by design: a ceiling is what one query may
  take when the box is otherwise idle.

## D. Test gaps that let all of this through

Approved by the owner 2026-09-06. Each converts a known-but-dismissed
observation into a gate.

- [x] **D1. Assert a descriptor bound where spilling is already forced.**
  Worse than "untested": `tests/sqllogic/tests/agg_spill.rs` explicitly
  SKIPS the 16 MiB ceiling because of this exhaustion, so the suite
  encodes the bug as expected. Two layers, per the review. First, track
  active and peak spill handles per query in the shared file wrapper,
  covering creation, reopen and close, and assert build peak <= 1,
  intermediate <= K+1, final <= K, and zero active handles and bytes after
  teardown, at two input sizes that produce very different run counts. The
  existing spill `files` metric counts files created, not open, so it
  cannot establish this. Second, run one representative case in a FRESH
  CHILD PROCESS with the soft `RLIMIT_NOFILE` lowered to 128 or 256, which
  catches retention the counters miss. Do not call `setrlimit` in an
  ordinary parallel test: limits are process-wide and shared by threads, so
  restoring afterwards does not prevent interference. Use a `rustix`
  dev-dependency for the safe call, since the workspace forbids unsafe.
  Then reinstate the skipped 16 MiB case as the integration regression.
  Done as specified: handle counters in the spill module, the 16 MiB
  ceiling back in the sweep with the fan-in bound asserted at every
  ceiling, and a child process under a 128-descriptor soft limit running
  the ceiling that spills hundreds of runs.
- [x] **D2. Run one gate inside the shipped compose file.** Today
  `docker-compose.yml` gets `config --quiet`, `up --wait` and a curl of
  `/health`. Every functional gate launches the bare binary on the host, so
  the file that defines a deployment's resource envelope is never under
  test. Running an existing gate through the composed container catches
  both B1 and B2 by construction.
  Done: `tests/compose/run.ts`, the `compose` stage of the rc and stable
  profiles. It builds the image from the tree on the docker host, brings
  the stack up through `docker-compose.yml` beside a MySQL source, checks
  the limits line for the file's descriptor limit and the concurrency the
  environment asked for, snapshots a table through the container, runs a
  sixty-thousand-group aggregation under a 64 MiB ceiling, and compares
  the answer with MySQL byte for byte while EXPLAIN ANALYZE proves the
  spill ran within the descriptor bound.
- [x] **D3. Gate production-shaped SQL at production scale.**
  `tests/e2e/bi-dogfood.ts` and `tests/corpus/bi-captured` exist for
  exactly this and are in no profile. The oracle's generated families stop
  at three-table joins with no CTE chains; the E2E corpus is about 1,300
  rows; the benchmark runs eight hand-picked queries. Nothing runs many
  query shapes at scale, which is the same gap the rc4 date-predicate
  slowness fell through. Add invented-schema cases of the shape described
  at the top of this file, run them against a large replica under the
  shipped container's limits, and assert both answers and completion.
  Done as an in-process suite, `tests/sqllogic/tests/report_shapes.rs`:
  an invented eleven-table schema at 600K rows, six report shapes (a
  ten-way LEFT JOIN chain from a filtered driving table, a window over
  the grouped chain, a two-level CTE with NOT EXISTS, an organisation-wide
  chain, a per-entity summary with distinct sets, a status-by-month
  report with HAVING), each run at a roomy ceiling, unoptimized, at the
  compose file's default ceiling and at a 24 MiB ceiling, all four
  answers equal and the tight run spilling within the descriptor bound.
  Its first runs found four defects: a direct-column aggregate path that
  never spilled, a double-counted batch reservation in the buffered
  aggregate, a join output batch refused for its own columnar copy, and
  COUNT(DISTINCT) over text counting a value once per spill run. The
  captured BI corpus stays out of the repository; this is the shape,
  not the data.

## E. Sequence

1. B2 and B3, with D2 as their gate. Small, and D2 proves both.
2. Redeploy so B1 takes effect.
3. C1c first: grace join is a live bug, not just an unbounded one.
   Then C1 and C1b, with D1 as their gate. C2 only if free.
4. C3, which needs its own measurement before any change.
5. D3 last: it is the broadest and will surface more of the same class.

## G. Found by the profiler, 2026-09-06

Measured in-process on a ten-million-row, three-column table with the
settled memo off, using the per-operator profile `EXPLAIN ANALYZE` now
prints.

- [x] **G1. A scan retains every prefetched segment at once.** With no
  LIMIT the stream adopts all prefetched chunks into ready batches in one
  pull, so the scan's peak reservation is the scan width times the
  segment size: 1.3 GiB for a plain `GROUP BY` over the table, 1.9 GiB
  with one predicate. Under a 1 GiB ceiling that plain `GROUP BY` fails
  with `query memory limit exceeded` on the first pull; the shipped
  default ceiling is 512 MiB. The store's chunk budget is meant to bound
  this and does not. Bound adoption to what the ceiling can hold, or
  adopt chunks as they are consumed.
  Progress 2026-09-06: most of the figure was capacity, not data - a
  chunk sliced into batches left every prefix holding the whole chunk's
  allocation (118 MB retained for 16 MB of data on a two-column 1M-row
  segment); prefixes are now right-sized and the plain GROUP BY runs
  under the shipped ceiling. Closed 2026-09-07: the scan's work unit is a
  block-aligned 131,072-row segment slice, a round takes at most half the
  remaining ceiling (one slice under 64 MiB), and rows in flight are bounded
  by width times a slice whatever the segment size; see "The scan's work
  unit is a segment slice" in `docs/decisions.md`.
- [ ] **G2. A second predicate on the same scan costs five times the
  first.** `WHERE status = 2` scans in 25 ms; `WHERE id >= 1 AND
  status = 2` in 132 ms with twice the peak reservation, even though the
  extra predicate excludes nothing and now stays on the packed kernel.
  Establish which of the two-predicate paths (no prewhere, since every
  projected column is a predicate column) pays the difference.
- [ ] **G3. Five-group aggregation spends 18 ns per row in the
  aggregate.** `GROUP BY status` over ten million rows: 114 ms in the
  scan, 179 ms of aggregate self time for five groups. That is the
  direct-column path's per-row hash and index work on a key with five
  values; a dictionary or dense-array fold would make it a memory
  pass.

## F. Still open from earlier reviews

- The rename orphan left in `snapshotting` (owner decision 2026-09-05:
  retire it to the retained-data state at probe time).
- Capped exponential backoff with jitter for pending keyless copy retries.
- `docs/design/quality-performance-todo.md` item 4: the instruction gate
  never passed in CI until `3b12ba5`.
- A source whose binlog retention is shorter than a full snapshot takes
  will resnapshot in a loop. Unrelated to these three failures, and a
  deployment note rather than an engine defect.
