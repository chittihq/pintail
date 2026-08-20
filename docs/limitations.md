# Known limitations

This document records only what does **not** work: deliberate compatibility
boundaries, known divergences from MySQL, and gaps. A query rejected with an
explicit error is preferable to a plausible but incorrect result.

What Pintail *does* support, and how closely it matches MySQL 8.4, belongs in
`parity.md` — not here. The two documents are disjoint on purpose, so this one
stays readable as a list of things to fix.

## M2 query engine

### SQL surface

- Correlated subquery shapes outside the canonical single-table equality form
  are rejected during binding. Correlated `NOT IN` additionally requires both
  membership sides to be provably non-nullable: with a possible NULL, MySQL's
  three-valued `NOT IN` diverges from an anti join, so those shapes reject.
- Non-equality join conditions are rejected.
- Compound temporal `RANGE` interval qualifiers reject because sqlparser does
  not accept their MySQL spelling (#13, #25).
- A window frame with a bounded start recomputes its aggregate over the frame
  width rather than sliding incrementally, because `MIN`/`MAX` cannot be
  un-accumulated when a row leaves the window. Cost is proportional to the
  frame width, so a very wide bounded frame is expensive; a frame anchored at
  `UNBOUNDED PRECEDING` accumulates once and is linear.
- `WITH RECURSIVE` accepts only one recursive member, which must scan the CTE
  exactly once in its `FROM`, with no aggregates, windows, `DISTINCT`,
  `GROUP BY`, `ORDER BY` or `LIMIT` inside the member, and member column
  storage types matching the anchor's. Pintail bounds
  `cte_max_recursion_depth` to `1..=1000000`; MySQL's unbounded value `0` is
  rejected so a session cannot disable the recursive resource guard.
- `RIGHT JOIN` supports only the two-table form.
- Aliased parenthesized join groups reject because a group-wide namespace is
  not implemented.
- MySQL warning categories other than `GROUP_CONCAT` truncation are not yet
  retained in a general diagnostics area.
- The JSON modification family (`JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`,
  `JSON_MERGE*`, `JSON_REMOVE`) and `JSON_TABLE` are unimplemented and out of
  scope by decision. `JSON_TABLE` is a table-valued function, so it needs a new
  table source through the binder, planner and executor plus the `COLUMNS`
  clause — structural work, not a function addition (#8).
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
- Direct comparison, ordering, grouping, DISTINCT/set duplicate handling,
  window partition/order keys, `IN`/`BETWEEN`, and `MIN`/`MAX` over JSON reject
  explicitly. Pintail does not substitute text collation or UTF-8 hashing for
  MySQL's binary-JSON precedence and equality rules (#8).
- JSON paths support member and numeric-index steps. Wildcards, recursive
  descent, ranges, and `last`-relative indexes still reject (#8).
- `REGEXP_LIKE` accepts MySQL's optional `match_type`; the longer positional
  overloads of `REGEXP_INSTR`, `REGEXP_REPLACE`, and `REGEXP_SUBSTR`, plus
  `REGEXP_COUNT`, remain unimplemented (#8). Regex uses Rust's linear-time
  Unicode engine rather than ICU. The compatibility surface is literals,
  alternation, capturing/non-capturing groups without backreferences,
  quantifiers, anchors, dot, Unicode properties and character/POSIX classes;
  lookaround and backreferences reject instead of being reinterpreted. Other
  ICU syntax and same-spelling semantic edges are not claimed. Binary-string
  operands reject, patterns are limited to 64 KiB, compiled
  programs to 1 MiB, and literal programs are owned and reused by the compiled
  query. Their conservative memory bound and generated replacement output are
  charged to the per-query ceiling; dynamic patterns are deliberately uncached
  so no program can outlive the row that requested it.
- Byte-level transcoding among character sets other than UTF-8 and binary is
  unsupported and rejects explicitly.
- Explicit `COLLATE` names other than `utf8mb4_0900_ai_ci` reject. Cross-profile
  coercibility is not implemented, so mixed source profiles reject on every
  collation-sensitive operation (#10).
- The `information_schema` client-discovery interpreter rejects CTEs, set
  operations, window functions, derived tables, and metadata relations outside
  the ten served relations. `VIEWS`, `ROUTINES`, and `CHECK_CONSTRAINTS` are
  deliberately empty: the compact replica does not retain source view/routine
  definitions or CHECK expressions.
- `SHA1`, `SHA2`, `CRC32`, `UUID`, `INET_ATON`/`INET_NTOA`, `BIN`, `OCT`,
  `SOUNDEX` and the trigonometric family are unimplemented; none appeared in
  the BI corpus (#17).
- `JSON_DEPTH`, `JSON_QUOTE`, `JSON_PRETTY`, `JSON_OVERLAPS` and `MEMBER OF`
  are unimplemented. `JSON_STORAGE_SIZE`/`JSON_STORAGE_FREE` report bytes of
  MySQL's binary JSON format, which has no counterpart here, so any number
  they returned would be invented; they stay unimplemented rather than
  approximated. `JSON_SCHEMA_VALID`/`JSON_SCHEMA_VALIDATION_REPORT` need a
  JSON Schema implementation (#8).

### What an unsupported construct looks like

Rejection is always an explicit error, never a silently different answer. The
messages below are what a client receives, so a message seen in `mysql`, a BI
tool, or the HTTP API can be matched back to the boundary that produced it.

| Message | Raised when |
|---|---|
| `unsupported statement: …` | The statement kind has no binding (DDL against the replica, writes, administrative commands) |
| `unsupported query clause: …` | A clause on a supported statement is out of scope — the recursive-CTE restrictions in this section report here |
| `unsupported query body: …` | A set-operation shape outside MySQL's left-associative semantics |
| `unsupported table expression: …` | A `FROM` item that is not a table, derived table, or non-recursive CTE |
| `unsupported projection: …` | A select item the binder cannot resolve to a column or expression |
| `unsupported join operator: …` | A join kind outside inner/left/right/semi/anti |
| `unsupported join constraint: …` | A join condition that is not an `AND` of equality pairs |
| `unsupported expression: …` | A function, operator, or literal form with no implementation — the window-function gaps in this section report here |
| `hash join requires one equality between left and right input expressions` | A join condition binds but has no cross-input equality to hash on |
| `cross join requires known catalog row counts for every input` | An unqualified cross join whose inputs have no exact catalog cardinality |
| `cross join estimate N exceeds safety limit M` | An unqualified cross join above the one-million-row guard |
| `scalar subquery produced N rows` | A scalar subquery returned more than one row at execution time |
| `physical operator X is not implemented` | A logical plan reached a physical operator that does not exist yet |
| `query memory limit exceeded` | The per-query ceiling was reached by an operator that does not spill |

Two of these are capability boundaries rather than bugs: the hash-join message
means the join is expressible but not with an equality to hash on, and the
cross-join guard means the query would have been correct but large enough to be
worth refusing.

### MySQL semantic differences

- `ENUM` values carry their declaration index and order by it, matching
  MySQL. A value not present in the declaration - which a source can hold
  after the column was altered - has no index and stays plain text, so it
  orders lexically against the labelled values rather than being given an
  invented position.
- Locale-specific collation profiles, full per-expression coercibility, and
  collation-sensitive execution over mixed source profiles remain unsupported
  (#10).

- `NOW()`, `CURDATE()`, `CURTIME()`, and no-argument `UNIX_TIMESTAMP()` are pinned to one timestamp per statement, read at plan time from the session time zone where one is set and the host clock and timezone otherwise. The MySQL wire endpoint implements `SET time_zone` per connection; the HTTP endpoint has no equivalent session state, and the session zone does not affect `CONVERT_TZ` or stored temporal values.

- Date parsing accepts the canonical date and date-time forms implemented by the M2 evaluator. `DATE_ADD` and `DATE_SUB` accept one interval field at a time; compound intervals such as `INTERVAL '1-2' YEAR_MONTH` are not implemented (#13). Compound qualifiers are rejected early by the SQL parser rather than during engine binding: sqlparser 0.62 only accepts simple interval unit keywords, so a compound qualifier fails with `INTERVAL requires a unit after the literal value`; supporting them requires the parser to accept the qualifier first (an upstream change). `EXTRACT` covers `YEAR`, `MONTH`, `DAY`, `HOUR`, `MINUTE`, `SECOND`, `QUARTER` and `WEEK`; compound units reject explicitly.

- The all-zero `DATE`/`DATETIME` (`0000-00-00`) is preserved as a value, as
  MySQL does: it is returned by a `SELECT`, does not match `IS NULL`, and is
  counted by `COUNT(column)`. Genuinely invalid values such as February 31st
  still normalize to SQL `NULL` during snapshot and CDC ingestion, because
  they have no canonical form MySQL round-trips. Existing replicas keep
  whatever ingestion already wrote; only rows re-ingested after this change
  carry the zero date.
- A zero date cannot be evaluated by the temporal functions: `YEAR`,
  `DATE_ADD` and their relatives error on it where MySQL returns `0` or
  `NULL`. That is the deliberate trade for it being a value at all - an
  explicit error rather than three silently wrong answers from mapping it to
  `NULL`.
- `sql_mode` does not reinterpret mirrored values, and `ALLOW_INVALID_DATES`
  is refused rather than accepted and ignored, so a client cannot believe it
  has asked for the invalid ones back.

- `STR_TO_DATE` supports the calendar/date, clock, month/weekday name, day-of-year, fractional-second, and composite clock directives used by the reporting corpus. Literal formats containing an unimplemented MySQL-only directive (ordinal dates or week/year reconstruction) reject at bind time; dynamic unsupported formats return `NULL`. They are never forwarded to chrono under a different meaning. `DATE_FORMAT` implements MySQL's full directive inventory, including the four `WEEK` numbering modes behind `%U %u %V %v` and their paired years `%X %x`, and copies an unrecognized directive's bare character the way MySQL does.

- Pintail maps an empty scalar-subquery result to `NULL`. During oracle development, MySQL 8.4's constant `SELECT` with `LIMIT 0` produced a special-case result that did not follow this behavior; that MySQL-only corner is excluded from the common-workload corpus.

- Source `DECIMAL` columns above precision 38 are replicated as text with a
  probe warning and deliberately decline exact-numeric expression semantics.
  Numeric overflow returns an explicit error rather than supporting MySQL's
  wider, up-to-65-digit DECIMAL range.

- `REPEAT`, `SPACE`, `LPAD`, and `RPAD` cap their result at 4096 bytes and error beyond it; MySQL's ceiling is `max_allowed_packet`. `FORMAT` uses en_US grouping only (no locale argument).

### Planning and execution

- Concurrent query execution is bounded (`--max-concurrent-queries`,
  default four times the core count with a floor of sixteen). Past the
  bound a query waits up to two seconds for a slot and is then refused
  with `MySQL` 1040 on the wire or HTTP 503, so overload becomes
  backpressure rather than unbounded queueing. The bound is what keeps
  tail latency flat under load; the cost is that median latency rises
  once the queue engages, because admitted queries may wait for a slot.
  Measured in `tests/load/results.md`. Connections themselves are still
  accepted without limit, so a client that only holds sessions open is
  not bounded by this.
- The process-wide memory budget (`--total-query-memory-limit-bytes`)
  defaults to three quarters of host memory, and to unbounded when host
  memory cannot be read rather than guessing a ceiling. It
  reports exhaustion as `server memory limit exceeded` rather than
  `query`; spilling operators treat both alike and spill, so the budget
  degrades a query to disk before failing it. Only reservations tracked by
  `MemoryTracker` are charged: batch decode buffers and per-connection
  session state are outside it, so the budget bounds operator memory rather
  than the whole process resident set.

- Text keys, numeric/string coercions, out-of-range signedness conversions,
  synthetic append-row IDs, undeclared mappings and composite keys remain
  correct but deliberately skip physical range pruning.
- A snapshot containing only one relevant segment has no smaller storage
  morsels to parallelize.
- Views below 65,536 candidate rows use the simpler materialized merge path,
  which remains covered by the query memory ceiling.
- `GROUP_CONCAT`/`JSON_ARRAYAGG` aggregation, the single-column direct-path
  aggregation, and materialized query outputs do not spill and still fail at
  the memory ceiling. A grace-partitioned join errors when one join key's own
  rows exceed the ceiling. Spill is isolated per query and bounded by
  `query.spill_limit_bytes` plus the process-wide `global_spill_limit_bytes`;
  exhausting either limit fails the query before the write crosses the
  ceiling.
- Uncorrelated scalar and `EXISTS` subqueries stop after two and one rows,
  respectively. Large `IN (subquery)` membership still materializes in memory
  under the query ceiling rather than using an external membership index.
- Dependent correlated execution reruns its bounded inner plan for each outer
  row and does not cache repeated parameter tuples. A correlated subquery in a
  join `ON` predicate uses a materialized nested loop under the query ceiling;
  that fallback does not spill. Nullable correlated `NOT IN` shapes that cannot
  be proven safe still reject rather than risk a different answer.
- Cross joins require catalog cardinalities and reject estimates above one
  million rows.
- Aggregate pushdown removes only unreferenced predicate-free cross-join inputs
  with an exact catalog cardinality of one. Pintail has no relationship or
  uniqueness statistics that would justify broader rewrites safely.
- `EXPLAIN ANALYZE` scan counters accumulate work from all executions of a
  stable table in the statement, including uncorrelated subqueries.
- Grouped sub-cubes and predicate-covered blocks are not covered by the
  persistent per-segment SMA fold.

## Snapshot engine

- A missing `FLUSH TABLES WITH READ LOCK` privilege can be allowed explicitly,
  but worker start instants can then differ and the result reports the degraded
  guarantee.
- A source changed between resume attempts can leave a mixed-time snapshot
  until the mandatory post-snapshot CDC catch-up replays the overlap. On
  binlog-disabled sources, polling and reconciliation own that convergence.
- PK-less tables use a single-stream `LIMIT`/`OFFSET` scan and generated
  append-row IDs. Source changes between attempts can shift offsets; polling
  reconciliation is required because there is no stable source identity.
- PTSEG v1 uses existing physical carriers: narrow integers use 64-bit values,
  `Float32` uses the 64-bit float carrier, and decimal/temporal/JSON values use
  canonical UTF-8.
- `DECIMAL` precision above 38 maps to text with a probe warning. ENUM and SET
  snapshot values are textual. Virtual generated columns are skipped.
- Spatial columns are retained as binary WKB without MySQL's four-byte SRID
  prefix, but there is no spatial logical type, index, or query function. They
  export as bytes and cannot be used for spatial predicates.
- Progress row estimates use `information_schema.TABLES.TABLE_ROWS`, which is
  approximate for InnoDB.

## CDC engine

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
  an operator resnapshot, `auto_resync` schedules that rebuild, and `reject`
  refuses the source during probe. Rebuilding from one source snapshot restores
  exact duplicate multiplicity; Pintail deliberately does not infer candidate
  identities or use collision-prone row fingerprints.
- A source charset outside utf8mb4/utf8mb3, ASCII and latin1 (cp1252) is
  quarantined through the DLQ.
- Grouping by a case- or accent-insensitive column reports one of the equal
  spellings, not necessarily the one MySQL reports. Both engines agree on the
  grouping and on the counts; each returns the spelling its own scan reached
  first, and the scans do not share an order. MySQL does not define which it
  returns either.
- `GROUP BY` accepts only columns that are grouped or aggregated. MySQL also
  accepts a column functionally dependent on the grouped key - selecting
  `orders.placed_at` while grouping by `orders.id`, say - because the key
  determines it. That analysis does not exist here, so such a query is refused
  rather than answered. Held to a test, so the entry cannot outlive the
  limitation.
- A single comparison spanning two collations is refused. `WHERE a = b` where
  the two columns are `utf8mb4_general_ci` and `utf8mb4_0900_ai_ci` has no
  defined answer here: the collations disagree about trailing spaces and about
  every character above the BMP, and MySQL chooses between them by coercibility
  rules that do not exist here. A query using both collations in SEPARATE
  comparisons is fine - each resolves its own.
- Grouping keys must share one collation. `GROUP BY general_ci_col,
  ai_ci_col` is refused, because grouping folds a whole key tuple into one
  entry and there is nowhere to record that one column of the tuple compares by
  different rules than the next. Ordering has no such limit: each `ORDER BY`
  key sorts under its own collation.
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
- Versions reserve 16 bits for the intra-transaction mutation ordinal, GTID
  sequences must fit 48 bits, and file/position versions support a 16-bit
  numeric file suffix and 32-bit event offset. A source transaction above
  65,535 physical mutations fails explicitly.
- Automatic purge recovery is database-wide and attempted once per runner
  invocation, resetting every included target, because one global source
  coordinate cannot safely advance while a table retains an unfillable gap.

## DDL and polling

- Polling cannot reproduce intermediate states that exist entirely between
  cycles. Hard deletes on cursor tables remain visible until a scheduled key
  reconciliation, except when a secondary-UNIQUE collision triggers immediate
  targeted repair.
- Count/MAX tokens are diagnostic only, so Pintail still performs an inclusive
  cursor-boundary read, aggregate-chunk comparison, or append-generation check
  when the token is unchanged — at the cost of source-side check queries on
  every scheduled sync.
- Source-key reconciliation materializes the full source and replica keysets in
  memory, so very large tables need memory proportional to their key inventory
  until a bloom-assisted or partitioned anti-join is implemented.
- CDC-side cascade/SET NULL reconciliation materializes a table-sized
  comparison in memory.
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
- The optional secondary-UNIQUE read policy inherits the collation
  approximation above.
- ALTER operations other than pure ADD/DROP COLUMN — rename, type/key changes,
  index-only changes, default-only changes — conservatively mark that table
  `needs_resync`.
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
  implemented. DROP TABLE retains the replica as an orphan with no operator
  purge action.
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

- The `caching_sha2_password` full-authentication fallback (RSA key exchange or
  cleartext-over-TLS) is not implemented; only the fast-auth exchange is
  served, which requires the verifier stored at key creation. Keys created
  before metadata schema version 13 must be rotated before use with
  caching_sha2_password clients; keys from before schema version 6 also lack
  the mysql_native_password verifier.
- The endpoint is read-only. `SET sql_mode` accepts only modes that are
  genuinely inert on a read-only replica: write and DDL modes
  (`STRICT_*`, `NO_ZERO_*`, `NO_ENGINE_SUBSTITUTION`) are stored and
  echoed, while modes that would change how a statement parses or
  evaluates are refused rather than accepted and ignored. `ANSI_QUOTES`,
  `PIPES_AS_CONCAT`, `HIGH_NOT_PRECEDENCE`, `NO_BACKSLASH_ESCAPES`,
  `IGNORE_SPACE`, `REAL_AS_FLOAT`, `NO_UNSIGNED_SUBTRACTION`,
  `ALLOW_INVALID_DATES` and the combination modes (`ANSI`, `DB2`,
  `MAXDB`, `MSSQL`, `ORACLE`, `POSTGRESQL`) all reject: the parser is a
  fixed `MySQL` dialect, so honouring them is not possible and accepting
  them would answer a different question than the client asked. Multiple
  SQL statements in one command are not supported.
- Variable-width text, binary, and JSON expressions without a retained source
  declaration report a type-derived `column_length` fallback of 1024. Only a
  direct `GROUP_CONCAT` projection derives that field and its VARCHAR/BLOB
  threshold from `group_concat_max_len`; wrappers and derived projections do
  not retain that aggregate provenance.
- Certificate rotation requires a restart. The HTTP endpoint still expects a
  TLS-capable ingress when exposed across a network.
- Explicit `KILL QUERY` is unsupported.
- DBeaver and Metabase application-level smokes are not automated in CI.
## Operations and backup

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
- Compaction runs inline on the ingest path, so a merge at a large size tier
  stalls replication for its duration. A 5,000,000-row append-only load
  measured 583,000 rows/s with compaction inert and 343,000 rows/s once merges
  engaged. There is no background compaction thread, and no way to bound or
  defer a pass that has begun.
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
multi-tenant isolation, spatial querying, or background compaction. Those
boundaries are explicit rather than emulated with results that look plausible
but may be wrong.

- A `CASE`/`IF` branch value with a scale smaller than the unified DECIMAL
  result type renders at the unified scale: `CASE WHEN .. THEN 0 ELSE
  dec(12,2) END` answers `0.00` where MySQL 8.4 answers `0` (MySQL keeps
  the branch value's own scale; its `COALESCE` rescales like Pintail
  does). Pintail's decimal columns carry one canonical text scale per
  COLUMN, regenerated whenever a batch is repacked, so a per-VALUE scale
  does not survive execution. Numerically the answers are equal.

- One source transaction may carry at most 16,777,215 row mutations in GTID
  mode, and 65,535 in file-position mode - the per-transaction ordinal is
  encoded into the 64-bit row version (24 bits under GTID, 16 under
  file-position, where the file index and byte position leave no spare
  bits). A larger transaction quarantines its table to needs_resync; a
  per-table resync captures the data and recovers.

- STR_TO_DATE, CONVERT_TZ, SEC_TO_TIME and MAKETIME advertise
  MYSQL_TYPE_VAR_STRING on the wire where MySQL advertises a temporal
  type (format-dependent for STR_TO_DATE, DATETIME and TIME for the
  others). Values match byte-for-byte; drivers decode them as strings.
