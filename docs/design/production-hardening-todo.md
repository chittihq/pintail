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
- [x] **G2. A second predicate on the same scan costs five times the
  first.** `WHERE status = 2` scans in 25 ms; `WHERE id >= 1 AND
  status = 2` in 132 ms with twice the peak reservation, even though the
  extra predicate excludes nothing and now stays on the packed kernel.
  Establish which of the two-predicate paths (no prewhere, since every
  projected column is a predicate column) pays the difference.
  Closed 2026-09-07: delta-packed integer columns decode directly into
  their typed buffers. A multi-column all-integer predicate projection
  evaluates borrowed buffers and retains exact survivors from that decode.
  The synthetic count conjunction fell from 37.5/38.4 to 9.7/10.5 ms
  minimum/median, versus 7.9/9.4 ms for the single predicate; peak
  reservation fell from 74.9 to 3.1 MiB. See e75. Scan sizing is unchanged.
  Confirmed on the benchmark replica 2026-09-07: the two-predicate count
  over twenty million rows fell from 155 ms to 74 ms against 46 ms for the
  single predicate, inside the 1.5x target. See e76.
- [x] **G3. Five-group aggregation spends 18 ns per row in the
  aggregate.** `GROUP BY status` over ten million rows: 114 ms in the
  scan, 179 ms of aggregate self time for five groups. That is the
  direct-column path's per-row hash and index work on a key with five
  values; a dictionary or dense-array fold would make it a memory
  pass.
  Closed 2026-09-07: the existing dense slots now fold packed integer SUM
  and COUNT lanes with dispatch outside the row loop, and bounded integer
  keys use the same table with whole-window fallback. The synthetic
  text-key minimum/median fell from 86.8/90.7 to 22.3/25.4 ms; integer keys
  from 105.3/131.2 to 32.6/39.0 ms. Reservations bound persistent and worker
  arrays. See e74 and the dense group slots decision.
  Confirmed on the benchmark replica 2026-09-07, after a correction: the
  synthetic case measured the packed fold, while the replica's five-group
  query averages a decimal and takes a lane the fold declines. That query
  first went from 131 ms to 156 ms, because the fold cut its window into
  one chunk per thread and left the pool nothing to steal. With four
  chunks per thread it runs 118 ms with 74 ms of aggregate self time,
  against 131 ms and 87 ms before the fold. See e76.

- [x] **G4. A narrow read paid for the whole segment.** The block readers
  loaded and checksummed every block of every column before deciding
  whether the scan wanted it, so a key lookup on a table with wide text
  columns read the entire segment (56 ms for ten rows on a laptop, about
  half a second on a deployment host). Closed 2026-09-07: blocks a reader
  has ruled out are skipped with a seek; see "A segment reader skips the
  blocks it will not decode" in `docs/decisions.md`.
- [x] **G5. The row-header pass read the whole key column.** A range
  lookup loaded and checksummed every block of the key, version and
  tombstone columns to find the ones the range touched, and reserved
  header memory for every row of the segment. Closed 2026-09-07: the pass
  seeks by the footer's column directory and sparse key index to the
  touched run in each system column and reserves for that run.
- [x] **G6. The merge path streamed from the first row.** `ScanPart::Merge`
  walked each overlapping segment from row zero to the merged range and on
  to the segment's end. Closed 2026-09-07: each stream seeks to the range's
  lower bound and the merge stops at the upper bound.

- [x] **G7. One memtable row merged a whole segment.** Any memtable row
  inside a segment's key range sent the segment through the row-wise
  merge, the normal state of every replicated table between flushes.
  Closed 2026-09-07: the `Overlay` part decodes the segment directly with
  the superseded rows masked by the key column; see "A segment the
  memtable overlaps is decoded directly" in `docs/decisions.md`.
- [ ] **G8. The partial-range memory fallback may drop key bounds.** A
  bounded direct decode that fails on memory retries over physical rows
  `0..row_count`, and the range decoder applies no key bounds, so
  out-of-range rows could surface when the smaller slices fit. Found by
  inspection during the overlay review; not reproduced.
- [ ] **G9. SMA pruning does not consider stale memtable versions.** A
  segment row failing a predicate can be pruned while a lower-version
  memtable row for the same key passes it; the merge would have kept the
  segment's version. Found by inspection; not reproduced.
- [x] **G10. Two overlapping segments never compact.** `compaction_plan`
  returned before the overlap check when the manifest held fewer segments
  than the fan-in (four), so an update-heavy table flushed once sat on a
  base and an overlapping tail and merged on every scan until two more
  flushes arrived. Closed 2026-09-08: an overlapping pair is admitted
  below the fan-in and outside the size tier, the per-pass row budget
  still bounding one pass. On two million rows with one percent changed
  the scan went from 1430 ms to 19 ms, against a one-off rewrite of
  1780 ms that repays after 1.1 scans (e91). The size tier still refuses
  a base-plus-tiny-tail rewrite whose only prize is fewer files. Left
  open by this: a table written far more often than it is read now
  compacts on every flush, and a read-rate-aware trigger is the answer
  if that appears.
- [ ] **G11. The `auto_resync` repair recopies the whole database.** The
  supervisor starts a forced snapshot for flagged keyless tables, so one
  flagged table resets every table's store; with the not-ready guard the
  whole database answers not ready for the copy. The per-table resync is
  the scoped path.
- [ ] **G13. A grouped aggregate over an expression key split one group
  across two output rows, once.** Observed during the dense-fold work
  while several test binaries ran concurrently: the general
  expression-keyed reference produced two rows for one key whose counts
  summed to the correct total, on a 720,000-row two-segment table with a
  text expression key. Sixty repeats at twelve concurrent binaries on a
  32-core host did not reproduce it, nor did the crate suite. Unresolved
  and unattributed: it was seen on the reference path, not on the dense
  fold under test. If it returns, the partition merge in the general
  aggregate is where two partials for one key could fail to combine.
- [ ] **G12. The streaming two-pass aggregate fails instead of spilling
  over an input with no transient floor.** Its proactive relief keys off
  the scan's reported floor; an input that reports none (a join, a
  subquery, the static test provider) can fill the ceiling with buffered
  windows until the next input batch's own reservation fails. Reproduced
  in-process: a text-keyed `COUNT(*)` over 20,000 groups at a 1 MiB
  ceiling fails on a 42 KB batch with the tracker at 1,015,832 bytes. The
  general path over the same input spills and completes.
- [x] **G14. `AVG` on a decimal column answered wrong, nondeterministically.**
  Closed 2026-09-08, on the second attempt, with a reproduction this time.

  The settled aggregate memo is keyed by the table's directory and manifest
  generation, and neither identifies a table. A directory is reclaimed when
  a table is dropped, and its successor starts from an empty manifest at
  generation zero and walks the same generations, so it presents a key the
  previous table already answered. The map is process-global and is only
  cleared wholesale on overflow, so nothing removed the entry in between.
  Every store opening now takes an identity from a process counter, carried
  on its snapshots and into the memo key.

  Why it looked like arithmetic, and was not. `AVG` read a unit in the last
  place away from `MySQL` while `SUM(_) / COUNT(*)` over the same rows
  matched exactly, which reads as the engine disagreeing with itself. Both
  columns come from one memoized row; they round differently, so
  `ROUND(SUM/COUNT, 4)` can agree across two incarnations of a table while
  `ROUND(AVG, 4)` differs. Nothing was miscomputed - the answer belonged to
  data that no longer existed. That asymmetry sent the search into the
  aggregate lanes, where nine in-process arms found nothing, because there
  was nothing there.

  Why no test could see it. Every in-process test opens its store under a
  fresh temporary directory, and a path used once cannot collide with
  itself. The regression test reuses one directory and fails deterministically
  without the fix.

  The grouped segment fold shares the key shape and the defect - it names a
  segment by its file name, and that counter also restarts with an empty
  manifest. Its key carries the opening now. That is a guard by
  construction, not a proven fix: two attempts at a test for it passed
  against deliberately broken code and were removed rather than kept as
  evidence they were not.

  What the first closure got wrong, kept as the lesson. The two-pass float
  lane guard below is real and stays. `fix/operational-blockers`, which
  forked before it, failed the check in all twelve e2e phases and passed
  once the guard was merged in with nothing else changed. That differential
  closes the path it varied; it says nothing about a second cause the same
  corpus reaches only sometimes. Read as proof of the symptom, it retired a
  limitation entry whose own closing sentence had set the right standard -
  that it stays until a run which would have failed passes for a reason
  that can be pointed at.

  The two-pass lane is chosen from a batch column's storage type, and the
  arm for a `Float64` column returned the float accumulator without asking
  whether the planner had typed the aggregate as an exact decimal - the arm
  beside it, for a decimal column, does ask. An average that fell through
  accumulated in `f64`, whose addition is not associative, so the answer
  moved with how the rows were split across workers.

## H. Closing the gap to ClickHouse with the settled memo off, 2026-09-07

G2 and G3 above are their own brief; this section is everything else the
"Engine speed (memo DISABLED)" table in `benchmark/results.md` and the
`experiments/RESULTS.md` profiler entries (e65, e70) point at.

- [x] **H0. The resource sampler raced an SSH connection, not the query.**
  `docker stats --no-stream` in a loop over the ssh:// docker context
  could start and stop without a single call completing on a sub-second
  query, reading 0% CPU. Closed: one long-lived `docker stats` stream per
  container; see e83 in `experiments/RESULTS.md`. The README's generated
  benchmark table now shows the memo-off table first.
- [x] **H1. The HTTP path's fixed cost.** `execute_query` built a fresh
  `ReplicaEngine` per request, hashed the API key against metadata on
  every call, and serialised rows through an intermediate
  `serde_json::Value` tree — 25-40 ms outside the engine on every query
  (e65). Closed: one `ReplicaEngine` held on `ApiState` and cloned per
  request, a 30s API-key cache invalidated on disable/delete, and rows
  serialize straight from column values into the response writer; see e84
  in `experiments/RESULTS.md`. The wire-vs-HTTP timing this item asked
  for is now in `benchmark/run.ts`, reported alongside HTTP rather than
  replacing it — banking a number needs the containerized benchmark.
- [x] **H2. Dense join build for a contiguous build key.** Q8's fused
  join-aggregate probed a general hash table at about 20 ns a probe (e65).
  Closed in part: `PartitionedBuild` now finalizes itself into a
  hash-free direct-index table in place when eligible, so every reader
  of `get` benefits, not only the fused aggregate, and the fused path
  resolves each distinct key's output group once instead of once per
  probe row; measured a 5-9% minimum-time improvement at Q8's real key
  cardinality (e85). Batching the probe into two passes, also asked for
  here, measured slower and was dropped (e85, `docs/decisions.md`). Q8's
  gap to ClickHouse is not closed by this alone - the two scans plus this
  probe still trail the target of 1.5x the scan cost.
- [x] **H3. Bitset `COUNT(DISTINCT)`.** A hash set per group for an
  integer column now promotes to a bitmap once past a count threshold and
  a span cap, charged to the tracker; the hash-set form stays as the
  fallback for a wide or unknown range. First shape shipped and measured
  1.5-30x SLOWER (thrashed between representations as a column's real
  range became apparent one value at a time); growing the bitmap with
  doubling headroom instead fixed it, banking a real ~25% win (e86,
  `docs/decisions.md`).
- [ ] **H4. High-cardinality `GROUP BY` into a top-K.** Not attempted;
  investigated and re-scoped. Q6's actual shape (a bare int column,
  `COUNT`/`SUM`) already routes to the streaming two-pass aggregate
  (e13), not the general full-materialization path this item assumed -
  measured 223 ms at 10M rows against the general path's 4.2 s for an
  unrelated (expression-keyed) shape (e87). Radix-partitioning the build
  key so each worker's groups are disjoint and can stream into a top-K
  heap is a new execution-model capability (a key-based shuffle, not a
  row-range split), not a local change; `build_buffered_hash_aggregate`
  already documents that naively parallelizing this exact shape
  regressed Q6 from seconds to minutes and "needs a partitioned design
  and its own experiment first" - unchanged by this brief. Next attempt
  should start from the 223 ms baseline in e87, not the 4.2 s figure this
  item was written against.
- [ ] **H5a. Scan pool default width.** e65 measured sixteen scan
  threads beating eight on an 8-CPU host; tried defaulting the scan pool
  (not the execution pool) to `2 x` CPU count. Reverted: no gain
  reproduced on bare metal with no CPU quota to hide behind (e88), so
  there was nothing to weigh against `tests/e2e` catching G14's
  pre-existing nondeterministic `AVG` defect (above) more often at the
  wider pool (not confirmed as caused by it - G14 reproduced at the
  CPU-count default too; e90). Stays at the CPU count, still overridable
  by `PINTAIL_SCAN_THREADS` for a deployment that wants e65's container
  benefit.
- [ ] **H5b. Overlap the sliced scan's rounds.** e70 noted the sliced
  scan idles between rounds. Investigated, not attempted: the round
  decode is a `&self` method call inside the same function later called
  with `&mut self`, and also takes a per-call borrowed `prewhere`
  predicate - both need to become an owned, `Send`, cross-call unit
  before a background prefetch is possible without `unsafe`, which the
  workspace forbids. See e89 in `experiments/RESULTS.md` for the specific
  blocker.

## F. Still open from earlier reviews

- The rename orphan left in `snapshotting` (owner decision 2026-09-05:
  retire it to the retained-data state at probe time).
- Capped exponential backoff with jitter for pending keyless copy retries.
- `docs/design/quality-performance-todo.md` item 4: the instruction gate
  never passed in CI until `3b12ba5`.
- A source whose binlog retention is shorter than a full snapshot takes
  will resnapshot in a loop. Unrelated to these three failures, and a
  deployment note rather than an engine defect.
