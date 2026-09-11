# Columnar execution program

Owner decision, 2026-09-11: close the performance gap against MySQL with a
proper architecture, not point fixes, in gated phases that keep the 1,895-case
MySQL oracle byte-exact at every step.

## Where the gap is

Source: [`benchmark/corpus/results.csv`](../../benchmark/corpus/results.csv),
measured at 5256ec63 (before the per-query-cost and correlated fixes).

| | Scale 1 | Scale 10,000 |
|---|---:|---:|
| Queries both engines answered | 1,768 | 1,722 |
| Pintail slower than MySQL | 1,767 | 1,589 |
| … at least 2× slower | 1,387 | 1,208 |
| … at least 10× slower | 0 | 97 |
| Excess over MySQL | 0.6 s | 79.8 s |

- **Scale 1 is a fixed per-query cost** of about 0.33 ms, flat across result
  size and present for queries that read no table.
- **Scale 10,000 is data-path cost.** Results over 50,000 rows carry 50.8 s of
  the excess; expression-heavy projections, joins and subqueries most of the
  rest.

## Diagnosis

Storage and the filter kernels are columnar, and typed forms already exist
(`TypedValues` carries decimal units and temporal integers). The defect is
that the columnar form is lost at interfaces:

- **`Value` is the interchange format between operators.** One computed
  expression sends a whole projection down the scalar path
  (`execution/mod.rs` Project); sort clones cells into rows and
  `rows_to_columns` clones them back; the wire engine collects
  `Vec<Vec<Value>>`; the encoder allocates a buffer per cell; nothing reaches
  the socket until the whole result is encoded.
- **Joins keep whole rows.** The hash build stores `Vec<Value>` rows; a
  residual builds a one-row batch per candidate; dependent filters recompile
  per outer row.
- **The query lifecycle repeats work.** Short-query admission binds, optimizes
  and plans a query that execution then plans again; freshness is proved by a
  walk over every replica file per query; statements are tokenized several
  times; a shared-query leader clones every result with or without followers.
- **Memory accounting sometimes performs the allocation it bounds**, as the
  scalar estimate materializing a whole column as `Value`s.

## Target architecture

- **Batch ownership.** `ColumnVector` holds immutable shared typed buffers
  with offset, length, validity and the existing selection masks; sparse row
  indices for selective gathers. Backing buffers are charged once through
  reservation tokens; scratch capacity per batch.
- **Expressions.** `BoundExpr` compiles to a typed program evaluated per
  batch under a selection, dispatching once per instruction and type. Bare
  columns are borrowed, never re-evaluated. Unsupported subexpressions run
  through a bounded scalar adapter over the selected lanes only.
- **Semantics carried explicitly.** Precision and scale, signedness, temporal
  kind and fractional precision, session time zone, invalid temporal states,
  ENUM ordinals, collation and padding. Warnings and errors are explicit
  effects; a branch a mask excludes is never evaluated.
- **Relational operators on typed columns.** Sort orders row references by
  precomputed typed keys and collation weights and gathers payload only for
  output. Hash joins retain batches and map normalized keys to row
  references, producing bounded candidate batches for residuals. Aggregates
  fold typed arrays; windows share partition and order preparation.
- **Order and access are plan properties.** A scan that yields key order says
  so, and a sort above it is elided only when that property is proven;
  primary-key equality and ranges lower onto storage's pruned point reads.
- **Query lifecycle.** `ParsedQuery` (source tokens, parsed once) and
  `PreparedQuery` (result metadata, physical plan, cost, dependencies) under
  an explicit session and statement context. Admission classifies with the
  same preparation execution uses — never a disposable second plan.
- **Freshness.** Committed state is published as an immutable replica view
  and its generation together, after it becomes visible, for CDC apply, local
  writes and schema or snapshot changes; each statement pins one view.
  Filesystem reconciliation belongs to startup and recovery.
- **Delivery.** An owned execution cursor yields metadata, batches and a
  terminal diagnostic; batches encode straight into reusable packet buffers
  through a bounded queue, with admission, snapshot and reservations held
  until delivery ends. An error after rows have been sent arrives as an error
  packet after them, as MySQL sends it.

## Phases

Every slice: the owner's rc gate (1,895-case oracle, both e2e ledgers,
browser, compose, BI clients), plus invented cases for batch boundaries,
overflow, warnings, collation, spills and cancellation. Scan, access and
publication changes also run live CDC insert, update, delete and schema
transitions.

| Phase | Work | Targets |
|---|---|---|
| 0 | Per-phase query timers, allocation and materialization counters, path labels; a matched corpus baseline on current `dev` | Measure before building |
| 1a | One parse and one preparation per query; explicit context; tokenize once per bind; the compatibility check honours `sql_mode`; shared-query clone only with followers; a background log writer; memory estimates without materialization | Scale 1's ~0.33 ms per query |
| 1b | Published replica view and generation instead of per-query file walks | The rest of the fixed cost |
| 2 | Shared typed buffers, mixed projection, direct batch encoding, then bounded streaming | The 50.8 s large-result pool |
| 3 | Typed expression programs with native decimal and temporal kernels and a bounded scalar fallback | ~22 s of expression-heavy projections |
| 4 | Order and access properties, primary-key lowering, columnar sort | Selective lookups; ~6.5 s of sorts |
| 5 | Columnar hash joins, batched parameterized subqueries, aggregate and window coverage | ~21 s of joins and subqueries; ~7 s of aggregates and windows |

Phase 1 is split because publication atomicity is the riskiest change in the
program and gets its own gate and live-CDC coverage.

## Decisions

- An error raised after rows have been streamed is sent after them, as MySQL
  does (owner, 2026-09-11).
- Sort elision is a proven physical property, not a heuristic over integer
  keys.
- CPU-bound execution stays off the async I/O threads; any inline fast path
  needs a bounded design first.
- Freshness trusts a table's published generation only while this process
  holds the table's writer lock; any other table is walked as before. This
  is a proof, where a periodic walk would only have bounded how long a
  missed change went unseen, so it replaces the backstop the review
  proposed. Replication opens its writers one cycle at a time, so the
  server keeps each table's lock as a lease between writers (the store
  library does not by default); a lease is adopted by the next writer only
  while its lock file is the one on disk, and at most 512 are kept.
- A replicated transaction that touches several tables reaches their files
  one table at a time, so a query that loads between two of those writes
  can see one table's rows and not the other's. The file walk had the same
  window, and publication neither widens nor closes it. Closing it means
  filtering rows by commit sequence at read time, which is a separate
  project.

## Progress

Measured at scale 10,000 after the second slice of phase 5 (9074d2be),
against the baseline above: the excess over MySQL fell from 73.3 s to
30.9 s, queries at least twice MySQL's time from 1,293 to 393, timeouts
from 62 to 29 and errors from 20 to 3, with no answer that differs where
it agreed before. Plain projections now carry the largest share (12.4 s,
7.0 s of it scalar expressions), then joins and subqueries under a sort.

- [x] Phase 0 — per-query trace (`PINTAIL_QUERY_TRACE`) and the traced
  baseline at a2c22194 in `benchmark/corpus/results.csv`. At scale 1 the
  server-side time splits into freshness checks and short-query
  classification (about half), then execution, session handling, binding,
  parsing and plan start.
- [x] Phase 1a — one parse and one preparation per query, shared by
  short-query admission and execution. Its gate also found that one table
  whose store would not open took its whole database down; that table now
  keeps reading under the definition its rows were written with, or refuses
  only its own reads.
- [x] Phase 1b — a table whose writer is open in this process is proven
  current by the generation that writer publishes after every change, so
  a query on a replicated database touches no table file to prove its
  replica fresh; other tables are walked as before. The corpus rerun
  showed replication closing its writers between cycles, which left most
  queries walking anyway; the server now keeps each table's lock as a
  lease between writers, so the generation holds between cycles too.
  The corpus rerun at scale 1 then read a median of 0.39 ms against
  MySQL's 0.26, down from 0.51, with 125 of 1,895 queries at least twice
  MySQL's time where 1,038 had been.
- [x] Phase 2 — columns share their built buffers, a projection evaluates
  only its computed columns, rows encode straight into packet buffers,
  and a result over 16,384 rows streams to the client through a bounded
  queue while it is produced; an error after streamed rows follows them.
  A 130,000-row result went from 58.9 ms to 32.0 ms on the wire.
- [ ] Phase 3 — first slice merged: batch kernels for date parts, interval
  arithmetic, `DATE`/`LAST_DAY`, `DATEDIFF`/`TIMESTAMPDIFF`, comparisons,
  `IS NULL`, `AND`/`OR`/`XOR`, signed integer and exact decimal arithmetic,
  decimal comparison and the widening casts compared operands carry. A
  kernel's argument may be another kernel's answer. A row that would raise
  an error declines the batch to row evaluation, and warnings are held as
  effects until the whole expression answers. The second slice adds
  `CASE`/`IF`, `COALESCE`/`IFNULL` and `NULLIF`, each branch evaluated
  over only the rows it answers; `GREATEST`/`LEAST`, `ABS`, `SIGN`,
  `ROUND`/`TRUNCATE`, `CEIL`/`FLOOR` and unary minus over packed numbers;
  `UPPER`, `LOWER`, `LENGTH`, `CHAR_LENGTH` and `LIKE` read in place, ENUM
  and SET labels included; and a bounded adapter that evaluates any other
  function over the selected rows of a batch, so one such call no longer
  sends the whole expression row by row. A `CASE` projection over 130,000
  rows went from about 60 ms to 8 ms, `GREATEST`/`LEAST` from 269 ms to
  108 ms. JSON and `TIME` functions run through the adapter only: their
  cost is parsing each document or time value per row, which a parsed
  JSON form or packed `TIME` units would remove.
- [ ] Phase 4 — merged so far: a sort the scan's key order already
  satisfies is left out, where the order is proven (integer key columns
  that are never NULL, named in key order, ascending, from the scan's own
  relation); the in-memory sort orders row references over the input's
  own batches by packed keys and gathers each output column once, in the
  row sort's exact order. `ORDER BY id` over 130,000 rows went from about
  99 ms to 21 ms; a sort over a 100,000-row join result from 33 ms to 8 ms.
- [ ] Phase 5 — merged so far: a resident hash join with no residual
  probes a batch at a time, gathering its probe columns and copying each
  build row once. A three-table join over 100,000 rows went from 355 ms to
  215 ms, a left join from 203 ms to 80 ms. A join's residual is evaluated
  over one batch of candidates, where it built a batch per candidate; a
  derived table - which is how a subquery rewritten as a join reads its
  table - drops the columns nothing reads; and a key range starting inside
  a segment decodes that segment's run of rows as columns, where it went
  row by row. A correlated `IN` with a residual went from 1,147 ms to
  118 ms, a `NOT EXISTS` from 449 ms to 51 ms, and `WHERE id > 3` over
  100,000 rows from 65 ms to 2 ms.
