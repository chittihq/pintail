# Known limitations

This document records only what does **not** work: deliberate compatibility
boundaries, known divergences from MySQL, and gaps. A query rejected with an
explicit error is preferable to a plausible but incorrect result.

What Pintail *does* support, and how closely it matches MySQL 8.4, belongs in
`parity.md` — not here. The two documents are disjoint on purpose, so this one
stays readable as a list of things to fix.

## M2 query engine

### SQL surface

- Correlated subqueries decorrelate when the inner side is a single filtered
  table and every correlated conjunct is a comparison spanning the two scopes
  (equalities key the hash join; ranges ride the residual or nested loop);
  `EXISTS`, `NOT EXISTS`, `IN` and `NOT IN` whose inner side joins tables,
  or groups under `EXISTS`, decorrelate through a derived table. Other
  shapes - an ungrouped aggregate, `HAVING`, `LIMIT`, or a correlation
  inside the inner join's `ON` - fall back to bounded dependent execution where the engine
  classifies them, and reject otherwise. On that path the inner query is
  planned and executed once per DISTINCT outer tuple - a statement-local
  memo shares the answer across outer rows that substitute the same values
  - but each execution still plans the inner query from scratch, so a
  correlated `IN` whose inner side holds a join pays roughly 20× the
  per-execution cost of a scalar or `EXISTS` shape
  (`benchmark/evidence/dependent-subquery-memo.md`). An inner query using
  `RAND()` or `UUID()` is never memoized. A correlated `NOT IN` over
  nullable columns decorrelates as a `WHERE` conjunct only; elsewhere both
  membership sides must be provably non-nullable, because with a possible
  NULL MySQL's three-valued `NOT IN` diverges from an anti join, and those
  shapes reject.
- A subquery in a LEFT (or RIGHT) join's ON condition that reaches the
  join's preserved side, and is not a non-negated `IN` or `EXISTS` over one
  table correlated by equalities, runs on the dependent join path: the
  subquery resolves once per distinct correlation value and each candidate
  pair is tested row by row. The ON condition's plain equalities bucket the
  candidates, so the cost follows the pairs those keys reach; with no such
  equality every pair is tested. `NOT IN`, `NOT EXISTS`, inequality
  correlations and subqueries with their own joins or grouping answer
  there, slower than a hash join.
- MySQL 8.4 with its default optimizer switches answers a correlated `IN`
  or `EXISTS` in an outer join's ON condition wrongly when the subquery
  carries filters of its own: its semi-join materialization drops them, so
  the join matches rows the subquery excludes. Pintail answers the
  statement as written, which is what MySQL returns with
  `optimizer_switch='semijoin=off'`, so on this shape Pintail's result
  differs from a default-configured MySQL's.
- A table whose segments overlap after its source updates rows is read
  through a merge on every scan until compaction removes the overlap, and
  value pruning skips only segments older than everything overlapping them.
  On 20,000,000 rows with one in a hundred updated, an unindexed full scan
  took 33 s against 2.5 s in MySQL, and point or IN-list lookups on an
  indexed column took 30-80 s against 1-2 ms, with every answer exact
  (`benchmark/results-filters.md`). There are no secondary indexes: a point
  lookup on a non-key column reads every segment its value might be in.
- `x op ANY` and `x op ALL` compare a number with text values as numbers,
  as MySQL 8.4 does over a table column. MySQL answers lexically when the
  subquery reads string constants through a derived table (`3 < ANY (SELECT v
  FROM (SELECT '2' AS v UNION ALL SELECT '10') t)` is 0 there), and Pintail
  does not reproduce that form.
- A join with no hashable equality key searches a sorted right input only
  for an inequality between plain integer, double, date or same-precision
  date-time columns. Any other theta condition, or a right input that
  spills, tests every row pair, so over large inputs it runs until the
  memory ceiling or `max_execution_time` stops it.
- Compound temporal `RANGE` interval qualifiers reject because sqlparser does
  not accept their MySQL spelling (#13, #25).
- A window frame with a bounded start recomputes its aggregate over the frame
  width rather than sliding incrementally, because `MIN`/`MAX` cannot be
  un-accumulated when a row leaves the window. Cost is proportional to the
  frame width, so a very wide bounded frame is expensive; a frame anchored at
  `UNBOUNDED PRECEDING` accumulates once and is linear.
- A `WITH RECURSIVE` member's column storage types must match the anchor's;
  MySQL instead converts member values to the anchor's types. Pintail bounds
  `cte_max_recursion_depth` to `1..=1000000`; MySQL's unbounded value `0` is
  rejected so a session cannot disable the recursive resource guard.
- An aliased parenthesized join group rejects, as it does in MySQL (a
  parenthesized join is not a derived table and cannot take an alias). A
  nested group can be a later join's right input; a RIGHT JOIN *inside* a
  parenthesized group still rejects.
- The diagnostics area records `GROUP_CONCAT` truncation (1260), division by
  zero (1365), text read as a number past its numeric prefix (1292
  "Truncated incorrect ..."), unreadable dates (1292 "Incorrect datetime
  value") and `STR_TO_DATE` mismatches (1411). Other warning classes - a date
  result past year 9999 (1441), JSON and regex notes, optimizer notes - are
  not recorded. Where a statement repeats a warning, the count follows how
  often Pintail evaluates the expression, which can differ from `MySQL`'s
  (an `ORDER BY` over a computed column makes `MySQL` warn twice per row).
- `JSON_TYPE` matches MySQL for JSON parsed from text — `DOUBLE`, `INTEGER`,
  `STRING`, `BOOLEAN`, `NULL`, `ARRAY`, `OBJECT` all agree, measured. It
  diverges only for a value carrying a SQL type into the document, where MySQL
  reports `DECIMAL`, `DATE`, `DATETIME`, `TIME` or `BLOB` and Pintail reports
  `STRING`. That is the same root cause as the `JSON_OBJECT` encoding entry
  above — the executor has no typed JSON carrier — rather than a separate
  defect, and closing it means a typed carrier, not a change to `JSON_TYPE`
  (#8).
- JSON logical identity now survives scalar and aggregate execution: constructors
  embed JSON columns and quote equal-looking VARCHAR text, and results advertise
  `MYSQL_TYPE_JSON`, and a DECIMAL member encodes as a JSON number keeping its
  scale (`{"d": 10.50}`), matching MySQL. Temporal members encode as JSON
  strings, which is what MySQL does too, except that MySQL pads a DATETIME to
  six fractional digits (`"2024-01-15 10:00:00.000000"`) and Pintail emits the
  value's own width. Reading a document back still normalizes numbers through
  the JSON parser, so a DECIMAL extracted out of a document loses trailing
  zeros; only construction is exact (#8).
- JSON-to-JSON comparison, `IN`/`BETWEEN`, ordering, grouping, and
  DISTINCT/set duplicate handling follow MySQL's JSON type-precedence ladder
  (numbers compare numerically across integer/double spellings; objects are
  equal regardless of member order), and so do text compared with JSON (as a
  JSON string), `MIN`/`MAX` and window `PARTITION BY`. Residuals that still
  reject explicitly: comparing JSON against a date, time or binary scalar
  (MySQL gives those their own JSON types), JSON arithmetic, window
  `ORDER BY` and `GROUP_CONCAT ... ORDER BY` over JSON (MySQL sorts there by
  a key that puts every integer before any other number, which the ladder
  does not reproduce), and the recursive-CTE `UNION DISTINCT` fixpoint over
  JSON rows. The relative
  order of unequal objects is deterministic but unspecified, as in MySQL (#8).
- JSON paths support member steps, numeric and `last`-relative indexes,
  ranges (`[M to N]`), wildcards (`.*`, `[*]`) and recursive descent (`**`)
  in the multi-target functions (`JSON_EXTRACT`, `JSON_CONTAINS_PATH`), with
  MySQL's array autowrap rules. Single-target functions (`JSON_VALUE`,
  `JSON_LENGTH`, `JSON_KEYS`, `JSON_CONTAINS`) accept `last` but refuse
  multi-target tokens, as MySQL does (#8).
- Regex uses Rust's linear-time Unicode engine rather than ICU. The
  compatibility surface is literals, alternation, capturing/non-capturing
  groups without backreferences,
  quantifiers, anchors, dot, Unicode properties and character/POSIX classes;
  lookaround and backreferences reject instead of being reinterpreted. Other
  ICU syntax and same-spelling semantic edges are not claimed. Binary-string
  operands reject, patterns are limited to 64 KiB, compiled
  programs to 1 MiB, and literal programs are owned and reused by the compiled
  query. Their conservative memory bound and generated replacement output are
  charged to the per-query ceiling; dynamic patterns are deliberately uncached
  so no program can outlive the row that requested it.
- SQL conversion does not implement legacy single-byte or multibyte encodings
  beyond Unicode encodings. Wide encodings are not accepted as client/result
  wire encodings or local table declarations. Ill-formed wide text can be
  inspected by byte functions directly behind an introducer, but character
  operations reject when it has no valid Unicode representation. Mixed-set
  coercibility and byte-based `GROUP_CONCAT` truncation remain incomplete.
- Explicit `COLLATE` accepts the four replicated profiles
  (`utf8mb4_0900_ai_ci`, `utf8mb4_general_ci`, `utf8mb4_unicode_ci`,
  `utf8mb4_bin`) and maps the
  legacy names onto them - every `*_bin` and `binary` to `utf8mb4_bin`,
  `*_general_ci` and `*_swedish_ci` to `utf8mb4_general_ci`, `*_unicode_ci`
  to `utf8mb4_unicode_ci` - which is exact
  over ASCII and an approximation outside it; other names reject. Because replicated text is stored transcoded to UTF-8, a supported
  `COLLATE` override is accepted even where MySQL would raise a
  charset-mismatch error (e.g. over a latin1-sourced column).
- The `information_schema` client-discovery interpreter rejects CTEs, set
  operations, window functions, derived tables, and metadata relations outside
  the ten served relations. `VIEWS`, `ROUTINES`, and `CHECK_CONSTRAINTS` are
  deliberately empty: the compact replica does not retain source view/routine
  definitions or CHECK expressions.
- `ENUM` values carry their declaration index and order by it, matching
  MySQL. A value not present in the declaration - which a source can hold
  after the column was altered - has no index and stays plain text, so it
  orders lexically against the labelled values rather than being given an
  invented position.
- Locale-specific collation profiles remain unsupported, and so do the `NONE`
  and `SYSCONST` coercibility rungs: an expression mixing collations carries
  its resolved one rather than `NONE` (#10).
- Date parsing is limited to canonical date and date-time forms. The plain
  `MICROSECOND` interval unit is not implemented; the compound
  `*_MICROSECOND` qualifiers are.

- Unix timestamp conversions round excess fractional digits to microseconds;
  `TIME_TRUNCATE_FRACTIONAL` does not switch them to truncation.

- A stored date a calendar rejects, such as February 30th from a source
  running `ALLOW_INVALID_DATES`, takes part in `MIN`/`MAX` as the date it is
  written as, where `MySQL` answers `NULL` for a `MAX` over them. Window
  functions partition and order such a date as written; under
  `NO_ZERO_IN_DATE` `MySQL` partitions and ranks every such date as one
  value, apart from the real zero date. Grouping, deduplication and unions follow `MySQL`'s
  rewrite, except where `MySQL` reads the column through a source index from
  a derived table or CTE it merges, which Pintail still rewrites. Rows
  ingested before these dates were preserved hold `NULL` until they are
  re-ingested.
- How `MySQL` groups and compares a `TIMESTAMP` in the hour a daylight-saving
  zone repeats depends on its plan. Pintail follows the plans observed: an
  intermediate copy by wall clock under `NO_ZERO_DATE`, `NO_ZERO_IN_DATE` or
  `ALLOW_INVALID_DATES` and by instant otherwise; instants when a single
  unfiltered table is read through a covering source index the column leads;
  wall-clock hash joins, and instant lookups when either joined column leads
  a source index. Still different: `GROUP_CONCAT(DISTINCT ...)` under modes
  without those flags merges the two instants; a derived table or CTE
  `MySQL` merges into an index read keeps the copy rule; and under those
  flags `MySQL` rewrites a value it materializes for a subquery, an `IN` or
  `NOT IN` list or an index lookup key to the first of the two instants,
  which Pintail does not, so such joins and memberships can match
  differently there.
- With `ONLY_FULL_GROUP_BY` off, a column that is neither grouped nor
  aggregated reads the first non-NULL value of its group; `MySQL` reads the
  first row's, NULL included.
- Replicas created before zero-date preservation keep their previous
  normalized values until the affected rows are re-ingested.

- `ADDTIME` and `SUBTIME` over text columns return text, so used in
  arithmetic they coerce as a string would (the leading number), where MySQL
  reads the result as a `TIME` and yields its `HHMMSS` number. Over `TIME`
  values and over literals they follow MySQL.
- Pintail maps an empty scalar-subquery result to `NULL`. During oracle development, MySQL 8.4's constant `SELECT` with `LIMIT 0` produced a special-case result that did not follow this behavior; that MySQL-only corner is excluded from the common-workload corpus.

- A replica created while source `DECIMAL` columns above precision 38 mapped
  to text keeps them as text, without exact-numeric semantics, until that
  table is re-snapshotted.

- `UUID_SHORT` identifiers are not coordinated across servers or restarts within the same second.
- `FORMAT` uses en_US grouping only (no locale argument).
- `max_allowed_packet` is always its 64 MiB default: `SET GLOBAL max_allowed_packet` is accepted and changes nothing, so string functions answer NULL with warning 1301 past 64 MiB even where a MySQL server whose limit was raised builds the value.
- A JSON constructor or modifier (`JSON_ARRAY`, `JSON_OBJECT`, `JSON_SET` and the rest) whose text passes `max_allowed_packet` is NULL wherever it is used. MySQL applies the limit only when the document is read as text, so a JSON function reading it as a document (`JSON_LENGTH(JSON_ARRAY(...))`) still answers there.
- `REGEXP_REPLACE` builds its result at any length. MySQL refuses a result with replacements whose UTF-16 form is longer than `max_allowed_packet` with error 3684, and cuts short some results just under that length.

### Planning and execution

- Concurrent query execution is bounded (`--max-concurrent-queries`,
  default four times the core count with a floor of sixteen). Past the
  bound a query waits up to two seconds for a slot and is then refused
  with `MySQL` 1040 on the wire or HTTP 503, so overload becomes
  backpressure rather than unbounded queueing. The bound is what keeps
  tail latency flat under load; the cost is that median latency rises
  once the queue engages, because admitted queries may wait for a slot.
  Measured in `tests/load/results.md`.
- Connections and prepared statements are bounded separately from query
  execution (`--wire-max-connections`, default 1000; per session
  `--wire-max-prepared-statements`, default 1024, plus 16 MiB of retained
  statement text), refusing with `MySQL` 1040 and 1461. The ceilings are
  a count and a byte figure, not a memory reservation: session state is
  still outside the query memory budget. What that costs is measured in
  `tests/load/results-sessions.md` - 1000 idle authenticated connections
  moved resident memory by under 7 MB at peak, and 51,200 held prepared
  statements by about 4 MB resting, so at the default ceilings a client
  that fills both holds well under 200 MB of session state before its
  first query. Measured on macOS/arm64 without the container's jemalloc
  purge settings; a Linux reading is expected to be lower, not higher.
- The process-wide memory budget (`--total-query-memory-limit-bytes`)
  defaults to three quarters of host memory, and to unbounded when host
  memory cannot be read rather than guessing a ceiling. It
  reports exhaustion as `server memory limit exceeded` rather than
  `query`; spilling operators treat both alike and spill, so the budget
  degrades a query to disk before failing it. Only reservations tracked by
  `MemoryTracker` are charged: batch decode buffers and per-connection
  session state are outside it, so the budget bounds operator memory rather
  than the whole process resident set. The encoded, wire-ready copy of a
  result set - built after the tracker is released - is held to the
  per-query ceiling as a byte bound, not charged to the process budget;
  a result whose encoded form alone exceeds `--query-memory-limit-bytes`
  is refused as a memory-limit error.

- Text keys, numeric/string coercions, out-of-range signedness conversions,
  synthetic append-row IDs, undeclared mappings and composite keys remain
  correct but deliberately skip physical range pruning.
- A snapshot containing only one relevant segment has no smaller storage
  morsels to parallelize.
- Views below 65,536 candidate rows use the simpler materialized merge path,
  which remains covered by the query memory ceiling.
- A window partition, including its frame state and computed values, must fit
  within the per-query memory ceiling. A larger partition is refused.
- A single merged `GROUP_CONCAT` or `JSON_ARRAYAGG` state and its finished
  value must fit within the query ceiling. `JSON_OBJECTAGG` has no spilled
  state encoding.
- External `IN` membership probes scan one hash partition, held in memory
  after its first probe while a budget allows; a partition too large for
  that budget is re-read from its file for every probe. Highly skewed sets
  can require quadratic comparisons across many probes; mixed-type and
  exact-decimal comparisons use a common partition to preserve coercion
  semantics. A correlated external set is rebuilt for each uncached outer
  tuple. One value and its comparison scratch must fit in the query budget.
- `SUM` and `AVG` over a decimal quotient (`AVG((a + b + c) / 3)`) can
  differ from MySQL in the last digits. MySQL's answer depends on its plan:
  a grouping it streams in index order folds each quotient at full internal
  precision, while a grouping through a temporary table first stores each
  quotient at the division's result scale (dividend scale plus
  `div_precision_increment`). Pintail always folds the stored quotient, the
  temporary-table answer, so a query MySQL happens to answer by streaming
  can differ from it from the fifth decimal place on.
- A grouped `SUM` or `AVG` over a `DECIMAL` whose total passes 65 digits
  answers the exact total. MySQL, where it groups through a temporary
  table, keeps the running total in a 65-digit column and holds it to that
  column's largest value row by row, so its answer depends on the order of
  the rows and is not the total. An ungrouped total agrees with MySQL.
- A double `SUM`, `AVG`, `STDDEV` or `VARIANCE` can differ from MySQL in its
  last digits where MySQL reads the rows in an order of its own:
  `WITH ROLLUP` and other groupings MySQL sorts first, and a join MySQL
  reorders. A group spilled to disk under the
  memory ceiling is combined from its runs' partial results, not row by
  row.
- A grace join partition that cannot be reduced by hashing replays its
  build rows for each probe row: what a quarter of the ceiling holds is
  read from the file once, and the rest is re-read per probe. This can
  require quadratic comparisons when many distinct keys collide through
  every hash pass. One candidate pair, its normalized keys, and its
  residual predicate must still fit.
- The correlated join `ON` fallback replays the right side once per left
  row; it has no cardinality-based side selection. Each candidate pair and
  its dependent inner execution must fit within the remaining query budget.
- Spill storage is bounded by `query.spill_limit_bytes` plus the process-wide
  `global_spill_limit_bytes`; exhausting either limit fails the query before
  the write crosses the ceiling.
- Dependent correlated execution can rerun its inner plan for each outer
  row when memoization is unavailable.
- A cross join holds every input after the first in memory; only the first
  streams.
- Aggregation runs below a join only for a GROUP BY over inner joins whose
  `SUM`, `COUNT`, `MIN` and `MAX` all read one base table of at least 50,000
  rows, grouped and joined on integer or temporal columns. Outer joins, text
  join or grouping keys, `AVG`, float sums, `DISTINCT` aggregates and
  ungrouped queries join every row first. The choice is a size rule, not a
  cost comparison, and it does not use the key's distinct-value count.
- `EXPLAIN ANALYZE` scan counters accumulate work from all executions of a
  stable table in the statement, including uncorrelated subqueries.
- Grouped sub-cubes and predicate-covered blocks are not covered by the
  persistent per-segment SMA fold.
- A missing `FLUSH TABLES WITH READ LOCK` privilege can be allowed explicitly,
  but worker start instants can then differ and the result reports the degraded
  guarantee. The same degraded start applies when a source table stays in use
  (a `LOCK TABLES` holder or a long query) for the two seconds a copy waits
  before taking the lock.
- A source changed between resume attempts can leave a mixed-time snapshot
  until the mandatory post-snapshot CDC catch-up replays the overlap. On
  binlog-disabled sources, polling and reconciliation own that convergence.
- PK-less tables use a single-stream `LIMIT`/`OFFSET` scan and generated
  append-row IDs. Source changes between attempts can shift offsets; polling
  reconciliation is required because there is no stable source identity.
- PTSEG v1 uses existing physical carriers: narrow integers use 64-bit values,
  `Float32` uses the 64-bit float carrier, and decimal/temporal/JSON values use
  canonical UTF-8.
- ENUM and SET snapshot values are textual. Virtual generated columns replicate from
  `MySQL` (5.7 and 8), which writes them into every row image. `MariaDB`
  leaves their value out of UPDATE after-images, so there they are skipped
  with a probe warning; the wider images it does write decode by source
  ordinal around the gap.
- Spatial columns are retained byte for byte in MySQL's internal format (the
  four-byte SRID, then WKB) and have no spatial index: a spatial predicate
  scans. The spatial functions compute on the Cartesian plane (SRID 0) only.
  Over SRID 4326 only reading, writing, the accessors and
  `ST_Distance_Sphere` answer; `ST_Distance`, `ST_Length`, `ST_Area`, the
  relational predicates and `ST_Envelope`/`ST_Centroid` are refused (error
  1235) rather than computed on the plane, since `MySQL` measures them on the
  ellipsoid. Every other SRID is refused for every function.
- `ST_Contains`/`ST_Within` are refused when the container is a line and the
  contained geometry is a line or a polygon, or when the container mixes
  points or lines with polygons. The set operations (`ST_Union`,
  `ST_Intersection`, `ST_Buffer`, ...), `ST_Overlaps`, `ST_Touches`,
  `ST_Crosses`, `ST_Equals`, `ST_IsValid`, `ST_Simplify` and the geohash and
  `ST_Transform` functions do not exist.
- A spatial function's result column reports as a binary string, not
  `GEOMETRY`, in result-set metadata.
- Progress row estimates use `information_schema.TABLES.TABLE_ROWS`, which is
  approximate for InnoDB.

## CDC engine

- A source whose table names are case-sensitive can hold two tables whose
  names differ only in case (`T1` and `t1`). Neither is streamed or queryable:
  a query naming either is refused as an unknown table, and every other table
  in the database keeps replicating.
- The binlog decoder is pinned to a fork. Published `mysql_common` panics
  on a transaction-payload header whose field id or compression type falls
  outside the range it narrows to, and `mysql_async` decodes those events
  inside its own stream, so no guard on Pintail's side can prevent it. The
  fork returns an error there instead. Until the fix lands upstream the
  crate resolves from git rather than crates.io, which puts it outside
  `cargo audit` and Dependabot; upstream 0.38 still carries both unwraps.

- The supervisor runs finite catch-up cycles on a five-second cadence, so a
  newly committed event may wait for the next cycle.
- MariaDB GTID text is captured for diagnostics, but `mysql_common` 0.37 does
  not encode MariaDB's GTID dump request, so MariaDB 11 resumes from the
  file/position captured alongside its GTID.
- On tables without a primary or safe UNIQUE key, UPDATE and DELETE have no
  stable source identity, so they enter the DLQ and mark that table
  `needs_resync`. Pintail never applies a before-image to an arbitrary matching
  duplicate. All mutations for that table in the affected source transaction
  are discarded; mutations for other, independently keyed tables may commit as
  the shared source checkpoint advances, so cross-table atomic visibility is
  not promised while a keyless table is quarantined.
- Keyless CDC is insert-only between snapshots. Inserts use a deterministic
  append identity and are idempotent across reconnect/replay. The first UPDATE
  or DELETE requires a whole-table generation rebuild: `quarantine` waits for
  an operator resnapshot, `auto_resync` recopies that one table, and `reject`
  refuses the source during probe. Rebuilding from one source snapshot restores
  exact duplicate multiplicity; Pintail deliberately does not infer candidate
  identities or use collision-prone row fingerprints.
- A keyless table is copied only under the global read lock. That lock is not
  attempted while any table on the source is in use, because a pending one
  queues every write behind it, so on a source that is never briefly idle such
  a table is left uncopied and flagged `needs_resync` until an attempt finds
  the source quiet. Keyed tables copy either way: the replay that follows the
  copy upserts them, while a keyless table's rows are identified by where they
  arrived in the stream and would be held twice.
- A source charset outside utf8mb4/utf8mb3, ASCII and latin1 (cp1252) is
  quarantined through the DLQ.
- Grouping by a case- or accent-insensitive column reports one of the equal
  spellings, not necessarily the one MySQL reports. Both engines agree on the
  grouping and on the counts; each returns the spelling its own scan reached
  first, and the scans do not share an order. MySQL does not define which it
  returns either.
- A `GROUP BY` dependence that runs through a table's SECOND unique index,
  through a derived table's or view's own keys, or through a `UNION` branch is
  not detected: the replica keeps one key per table, and a derived layout
  carries none outward. Such a query is refused where MySQL answers it.
- An argument list mixing column collations is resolved only where the answer
  cannot depend on argument order. MySQL folds `IN`, `COALESCE` and similar
  lists pairwise left to right, so `general_ci_col IN (bin_col, ai_ci_col)`
  resolves to `utf8mb4_bin` there while the same list in another order is an
  illegal mix. Pintail refuses any mixture that keeps more than one
  non-binary collation after the charset step, including the orders MySQL
  answers.
- `ALTER TABLE ... CONVERT TO CHARACTER SET` is treated as metadata-only.
  Stored values are decoded characters rather than source bytes, so a
  conversion that preserves them changes only the collation. A conversion
  MySQL cannot represent losslessly - narrowing utf8mb4 to a charset without
  those characters - does change values, and the replica keeps the originals
  until the table is resnapshotted.
- `binlog_row_metadata` may be MINIMAL or absent (MySQL 5.7, MariaDB): column
  identity is then ordinal against the probed schema, enum/set labels and
  charsets come from probed declarations, and unsigned integers are
  reinterpreted at their declared width because MINIMAL row events omit the
  SIGNEDNESS field. `binlog_format=ROW` and `binlog_row_image=FULL` are hard
  requirements; a non-FULL row image demotes the source to polling.
- A schema change that never reaches the stream as DDL is repaired by
  re-probing the source, but under MINIMAL metadata that repair only covers a
  stream lagging a single change. The row images written before it are
  narrower than the refreshed schema and MINIMAL names no columns, so there is
  nothing to place them against; the table is flagged for resync instead.
  Under FULL metadata the table map names its own columns and any lag is
  repaired without one.
- File/position versions support a 16-bit numeric file suffix, a 32-bit
  event offset and a 16-bit intra-transaction mutation ordinal; a source
  transaction above 65,535 physical mutations fails explicitly in that mode.
  Under GTID a transaction's size is not limited, and the GTID sequence plus
  the version slots large transactions ran on into must fit 40 bits.
- Automatic purge recovery is database-wide and attempted once per runner
  invocation, resetting every included target, because one global source
  coordinate cannot safely advance while a table retains an unfillable gap.

## DDL and polling

- Polling cannot reproduce intermediate states that exist entirely between
  cycles. Hard deletes and updates below the saved cursor can remain stale
  until scheduled reconciliation; a secondary-UNIQUE collision can trigger
  earlier targeted repair.
- Count/MAX tokens are diagnostic only, so Pintail still performs an inclusive
  cursor-boundary read, aggregate-chunk comparison, or append-generation check
  when the token is unchanged — at the cost of source-side check queries on
  every scheduled sync.
- Cursor reconciliation materializes the full projected source rows and current
  replica rows in memory. Large cursor tables therefore need memory proportional
  to their row data during a full reconciliation.
- Source-key reconciliation materializes the full source and replica keysets in
  memory, so very large tables need memory proportional to their key inventory
  until a bloom-assisted or partitioned anti-join is implemented.
- A cascading foreign key that references a parent Pintail does not
  replicate, a parent without a primary key, or a unique key that is not
  the parent's primary key sends its child table through the full compare:
  every source row is read again, in pages, and every replica key is
  verified against the source a thousand at a time. Memory stays bounded,
  but the pass takes as long as reading the table.
- Rows removed by an invisible foreign-key cascade stay visible in the replica
  until that reconciliation runs, so the replica reads AHEAD of the source -
  more rows, larger sums - rather than behind it. A full production run with
  eight writers issued 74 cascade deletes and left `shipment_items` 51 rows
  and 173 units of `amountSum` above the source, still unconverged when the
  phase ended. Ordinary replication lag resolves itself and reads low; this
  does not resolve until reconciliation, and reads high, so the two cannot be
  told apart by direction alone.
- Cursor-less keyed checksums can re-dump adjacent chunks when inserts or
  deletes shift chunk boundaries; correctness holds, but repair work can exceed
  the number of rows that changed.
- Tables without a stable source key use append-generation replacement, so
  individual UPDATE or DELETE identities and intermediate history are
  unknowable.
- The secondary-UNIQUE read policy (polling databases, and CDC tables
  flagged for periodic reconciliation) holds a table's whole projected scan
  in memory, bounded by the query memory ceiling, to find the collisions it
  hides; a table whose projection exceeds that ceiling cannot be queried
  under it. The policy also inherits the collation approximation above.
- ALTER handling evolves in place for pure ADD/DROP COLUMN, pure column
  RENAME (verified mid-stream: stable column IDs carry across, rows before
  and after the rename stay intact with no resync), storage-compatible
  MODIFY/CHANGE type changes, index-only changes, and table RENAME within
  the schema (the store directory and every metadata row keyed by the name
  move at the binlog position; no recopy). A MODIFY/CHANGE is
  storage-compatible only when the source's own declaration says the change
  leaves stored values alone, which the mapped Pintail type cannot decide on
  its own: a narrowing integer, a shrinking string, `DATETIME` becoming
  `TIMESTAMP`, a dropped `ENUM` member, a reordered `SET`, a tightened
  nullability and a rewritten generated expression all keep one mapped type
  while the source rewrites rows underneath it. What still marks the table
  `needs_resync`: those, other storage-incompatible type changes, and
  key-strategy changes. A rename into another schema is treated as a drop.
- A `MODIFY`/`CHANGE` that rewrites the values the source already holds
  marks the table `needs_resync` rather than evolving in place, because an
  `ALTER` carries no row events for the rows it rewrote. The resync is
  charged even when the rewrite was empty in practice: a nullability
  tightening on a column with no NULLs left, a `VARCHAR` shrink every value
  already fits inside, an `ENUM` member dropped that no row used. The
  declaration is all the stream has to go on - it cannot see the source's
  rows - so the reading is the conservative one, and the cost is a full
  recopy of a table where an in-place adoption would have been correct.
- Readers that opened a table's snapshot before a `RENAME TABLE` keep the
  old directory and fail their next read; a client retries and the new
  name answers. The window is the moment the rename applies.
- In polling mode a `RENAME TABLE` is not observed (there is no binlog to
  carry it): the next probe adopts the new name as a table the source
  added and copies it afresh, and the old name is retired then.
- A per-table pause applies from the next supervisor cycle, not the
  instant the request is answered; a row event already in the cycle's
  batch still lands. Changes skipped while a table is paused are not
  kept anywhere: resuming recopies the table instead of replaying them,
  and for a keyless table that recopy waits on the keyless policy the
  same way a quarantine does.
- Adding or removing a stable key is therefore a safe resnapshot boundary, not
  an in-place identity change. After the replacement generation is published,
  the refreshed probe promotes the table to row-level primary/unique-key CDC or
  demotes it to the keyless policy; ambiguous changes are never partially
  applied.
- If several schema changes occur while Pintail is offline and the final source
  schema no longer represents an event's intermediate shape, Pintail
  quarantines the incompatible table rather than reconstructing historical
  layouts from SQL text. A table resnapshot is then required.
- Auto-inclusion uses case-insensitive exact allow/deny names and requires a
  writable target root; glob patterns and dashboard rule editing are not
  implemented. DROP TABLE retains the replica as an orphan, and so does a
  table renamed while its copy was cut short; an operator removes either
  with Remove on the database page. In polling mode, a dropped table can also interrupt a cycle
  before surviving tables advance. The E2E gate observed no progress on a
  surviving table within 90 seconds; an explicit re-probe restored replication.
- A dropped source DATABASE is surfaced, not modelled: replication fails
  loudly (`Unknown database` connection errors, database state `error`) and a
  re-probe correctly refuses, but the statement itself never reaches the
  stream - the runner's connections fail before the binlog event could be
  read - so no table is orphaned and the replica keeps serving the retained
  rows as current until an operator acts.

## HTTP API and dashboard

- The HTTP surface serializes binary values as lowercase `0x` hex strings, and
  JSON columns remain canonical JSON text rather than nested response objects.
- The embedded dashboard is a local control plane, not a multi-tenant security
  boundary. Network exposure and TLS are deployment responsibilities.

## MySQL wire protocol

- A user variable holds the literal its expression answered, so one assigned
  from a DATE or DATETIME reads back as that text rather than as a temporal
  value. The HTTP query API has no session and does not keep them.

- `caching_sha2_password` serves both the fast-auth exchange and the
  full-authentication fallback (RSA key exchange toward a per-process
  keypair, or cleartext from a client that trusts its transport), validated
  against the stored verifiers. Keys from before metadata schema version 6
  lack both verifiers and must still be rotated.
- Combination modes `DB2`, `MAXDB`, `MSSQL`, `ORACLE`, and `POSTGRESQL` remain refused.
- Variable-width expressions outside the declaration rules use a type-derived
  `column_length` fallback of 1024. Only a
  direct `GROUP_CONCAT` projection derives that field and its VARCHAR/BLOB
  threshold from `group_concat_max_len`; wrappers and derived projections do
  not retain that aggregate provenance.
- Certificate rotation requires a restart. The HTTP endpoint still expects a
  TLS-capable ingress when exposed across a network.
- Result key/default flags and numeric `BINARY_FLAG` can differ from the
  source because they reflect temporary-field and execution-plan choices.
  They do not certify source index use or a result's updatability.
- `KILL QUERY <id>` interrupts the target connection's running statement;
  the interrupted side reports MySQL's query-interrupted error. Bare `KILL`
  and `KILL CONNECTION` reject explicitly - terminating another session is
  not meaningful on a read-only replica and pretending otherwise would leave
  clients believing a connection died that did not.
- Desktop BI application UI flows are outside the automated driver and
  Metabase smoke matrix.
- A request waiting on another request's identical execution keeps the
  admission permit it took, so sharing removes executions rather than
  freeing concurrency slots: sixteen simultaneous copies of one statement
  still occupy sixteen slots while one of them runs. It is bounded at
  sixty-four concurrent shared executions and sixty-four waiters each,
  past which a request executes on its own.
## Operations and backup

- A restored copy is not refreshed automatically and provides no failover or
  promotion. Its reported data age measures the installed backup manifest's
  creation time, excluding source replication lag and capture-to-publication
  delay; it is not a source freshness guarantee. Older restores without that
  timestamp report an unknown age.

- Memory cancellation is cooperative, and allocator RSS may stay high after
  a query releases its reservations. The watchdog waits five seconds between
  victims; prolonged pressure can still cancel successive queries. Untracked
  snapshot and response-buffer allocations cannot be attributed to a victim.

- Reserved query admission requires a warm, revalidated replica and a plan
  bounded to 256 Ki physical input rows or 32 MiB of projected fixed-width
  input, with at most 1,000 output rows. The bound counts overlapping
  segments and the pinned WAL tail, not estimated filter selectivity.
  Variable-width values qualify only through the row budget. Eligibility
  covers one table or one storage-key equality join; windows, unfiltered
  aggregates, subqueries, and sorts without a storage-key order match use
  general capacity. Tiny databases retain their existing eligibility rule.
  Cold or stale replicas also need general capacity. Classification is not
  a latency guarantee: reserved reads still share CPU, memory and storage.
  `--reserved-query-slots` / `PINTAIL_RESERVED_QUERY_SLOTS` sizes the reserve;
  zero disables it, and at least one general slot is retained.

- The supervisor is finite-cycle rather than a permanently attached stream, so
  a newly committed event may wait for the next five-second cycle.
- Default size-tier maintenance admits at most 8,000,000 input rows per
  compaction pass and closes an output segment at 4,000,000 rows or 128 MiB. A
  candidate above the admission limit remains as overlapping immutable segments
  resolved by streaming merge-on-read. The compaction-debt metric reports the
  next eligible plan, so it does not quantify an oversized deferred window.
  These storage limits are engine options rather than TOML/CLI settings in v1.
- A compaction pass is deferred, not queued, when free disk cannot cover the
  planned merge plus `compaction_disk_reserve_bytes` (64 MiB default). Nothing
  retries until the next flush makes the plan eligible again.
- Size-tier merges run on a background thread by default
  (`background_compaction`); the ingest path only spawns them and publishes
  their results, so a large merge no longer stalls replication for its
  duration. At most one merge is in flight per table, an explicit `compact()`
  defers while one runs, and a failed background merge is recorded and
  retried by the next eligible pass. A merge that has begun still cannot be
  cancelled mid-flight; its orphan chunks are swept at the next open.
- Segment consolidation of disjoint key ranges waits for the live segment count
  to reach `compaction_file_pressure` (16 by default), so below that threshold
  an append-only table accumulates one segment per memtable flush.
- **Availability model: one process is the whole analytics tier.** Pintail is
  crash-safe and self-recovering, but not highly available. A restart or host
  failure means analytics is unavailable until the process is serving again;
  there is no standby, no replica read path, and no failover. Recovery duration
  is exported as `pintail_startup_milliseconds`. A restart costs availability,
  never data: MySQL remains the system of record.
- RSS comes from the host `ps` process table; environments without a compatible
  `ps` report zero rather than guessing. Storage and segment metrics walk the
  local data directory and can be expensive for very large deployments.
- DLQ retry performs a table reconciliation before removal. A database-level
  DLQ entry cannot be reconstructed from one row and requires a database
  resnapshot.
- Object-store authorization remains the operator's responsibility. Prefix
  validation prevents accidental cross-prefix writes; it is not tenant
  isolation.
- Backups have no automatic retention policy. Incremental generations depend on
  their parent chain, so operators must retain every ancestor referenced by a
  manifest.
- Restore is side-by-side and detached. It does not recover or expose the
  encrypted source DSN and never overwrites an active replica.

## Release boundary

Pintail v1 is a single-node, read-only analytical replica. It does not provide
clustered query execution, synchronous high availability, source writes,
multi-tenant isolation, or spatial indexing. Those
boundaries are explicit rather than emulated with results that look plausible
but may be wrong.

- One source transaction may carry at most 65,535 row mutations in
  file-position mode - the per-transaction ordinal is encoded into the
  64-bit row version, and the file index and byte position leave it 16
  bits. A larger transaction quarantines its table to needs_resync; a
  per-table resync captures the data and recovers. GTID mode has no such
  limit.

- SEC_TO_TIME, MAKETIME, CONVERT_TZ and JSON_UNQUOTE/->> advertise
  MySQL's own column types (TIME, DATETIME, LONG_BLOB) as direct
  projections; wrapped in another expression they fall back to
  MYSQL_TYPE_VAR_STRING, because the wrapper's shape owns the result.

- Comparing two JSON values (json_col = json_col) and JSON arithmetic
  are unsupported; MySQL compares JSON semantically (1.0 equals 1),
  which the text carrier does not implement. JSON extracted to text via
  ->> or JSON_UNQUOTE compares fully.

## Local writable databases

- `UPDATE` and `DELETE` are not implemented on a local database. Rows can
  be created and inserted, and a row that is wrong can only be corrected by
  recreating the table (issue #7, phase 3).
- There are no explicit transactions: every statement is its own autocommit
  transaction (phase 4). `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`,
  `SAVEPOINT` and `SET autocommit=0` are refused on a local database
  (MySQL error 1149) rather than accepted as no-ops - a no-op `ROLLBACK`
  reported that a stored row had been discarded. A client that needs
  atomicity across statements has no way to get it here. Replicated
  databases still accept all of them: they write nothing, so the no-op
  claims nothing false.
- A local table's text must be declared in `utf8mb4`, `utf8mb3`, `ascii`,
  `latin1`, `latin2`, `tis620` or `binary`. A column, table default or collation naming any other
  character set is refused at `CREATE TABLE`, because values are stored as
  decoded characters and would answer byte lengths, hex and ordering in the
  wrong encoding.
- A keyless local table is append-only: rows live under a generated id,
  the same model the replica uses for a keyless source table, so a
  duplicate row is simply a second row and nothing can address one later.
- `UNIQUE` beyond the primary key, foreign keys, `CHECK` and secondary
  indexes are accepted at `CREATE TABLE` and not enforced: a duplicate the
  source would refuse is stored. `AUTO_INCREMENT` is accepted but never
  assigns a value; an `INSERT` that leaves the column out is refused. A
  column `DEFAULT` must be `NULL`, a number or a string; an expression default
  such as `CURRENT_TIMESTAMP` is refused at `CREATE TABLE`.
- `INSERT` takes literal values only; an expression such as `1 + 1` or
  `NOW()` is refused rather than evaluated.
- A local table cannot be joined against a replicated one. A query reaches
  exactly one database, and the two kinds are separate databases
  (deferred, not foreclosed - see `docs/decisions.md`).
