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
- A `RANGE` frame accepts only offsetless bounds (`UNBOUNDED PRECEDING`,
  `CURRENT ROW`, `UNBOUNDED FOLLOWING`). `RANGE` with a numeric offset
  compares the ordering key's own values rather than counting rows, so it is
  not approximated with row offsets and rejects. `GROUPS` frames, which count
  peer groups, reject for the same reason. Windows still cannot combine with
  `DISTINCT` (#25). A named window may be referenced as `OVER w` and extended
  additively with clauses absent from its base definition. Chained named
  definitions (`WINDOW child AS parent`) still reject; resolving those needs
  cycle detection. `RANGE` with an offset compares the ordering key's own
  values rather than counting rows, so it is not approximated with row offsets.
- A window frame with a bounded start recomputes its aggregate over the frame
  width rather than sliding incrementally, because `MIN`/`MAX` cannot be
  un-accumulated when a row leaves the window. Cost is proportional to the
  frame width, so a very wide bounded frame is expensive; a frame anchored at
  `UNBOUNDED PRECEDING` accumulates once and is linear.
- A distinct union under a later `UNION ALL` rejects explicitly.
- `WITH RECURSIVE` accepts only one recursive member, which must scan the CTE
  exactly once in its `FROM`, with no aggregates, windows, `DISTINCT`,
  `GROUP BY`, `ORDER BY` or `LIMIT` inside the member, and member column
  storage types matching the anchor's. `cte_max_recursion_depth` is not
  configurable; a non-converging recursion aborts with an explicit error.
- `RIGHT JOIN` supports only the two-table form.
- Parenthesized root INNER/CROSS join groups bind by flattening their left-deep
  chain. Aliased groups, nested right inputs, and parenthesized groups that
  contain outer/semi/anti joins reject because the current bound plan cannot
  preserve their join tree (#16).
- `GROUP_CONCAT`'s `group_concat_max_len` session variable is not configurable
  and the truncation warning is not raised.
- Aggregates are limited to `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`,
  `GROUP_CONCAT` and `JSON_ARRAYAGG`. `ANY_VALUE`, the `STDDEV`/`VARIANCE`
  families and `BIT_AND`/`BIT_OR`/`BIT_XOR` are missing (#17).
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
- `JSON_OBJECT`/`JSON_ARRAY`/`JSON_ARRAYAGG` encode DECIMAL and temporal
  values as JSON strings where MySQL emits numbers or datetime scalars, because
  there is no JSON column type in the executor.
- JSON paths support member and numeric-index steps. Wildcards, recursive
  descent, ranges, and `last`-relative indexes still reject (#8).
- `REGEXP_LIKE` accepts MySQL's optional `match_type`; the longer positional
  overloads of `REGEXP_INSTR`, `REGEXP_REPLACE`, and `REGEXP_SUBSTR`, plus
  `REGEXP_COUNT`, remain unimplemented (#8).
- `CONVERT(value USING charset)` does not perform byte-level transcoding among
  MySQL character sets.
- `CAST(value AS TIME)` and `CAST(value AS JSON)` reject. MySQL's `TIME` spans
  `-838:59:59`..`838:59:59`, which the executor's datetime carrier cannot
  represent, so a partial implementation would answer `NULL` where MySQL
  answers a value; and `CAST AS JSON` must reject invalid JSON text rather
  than pass it through. `CAST AS YEAR` also rejects (#17).
- `information_schema` does not support joins, aggregates beyond `COUNT(*)`, or
  metadata tables outside the served set.
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

- `ENUM` values compare and sort as their text, not as MySQL's
  declaration-index order. `CAST(col AS CHAR)` on the MySQL side produces
  matching orderings.
- Text comparison, grouping, hashing, `LIKE`, and ordering use a case-insensitive Unicode-lowercase approximation by default. Setting `PINTAIL_COLLATION=utf8mb4_0900_ai_ci` opts every text comparison into an accent-insensitive approximation of MySQL's default collation (NFD with combining marks stripped, then lowercased) — closer to `utf8mb4_0900_ai_ci` for Latin scripts, but still not the UCA weight tables, locale tailoring, coercibility rules, or pad-space behavior. The flag reads once at process start and cannot change per session. Binary values remain bytewise.

- `NOW()`, `CURDATE()`, `CURTIME()`, and no-argument `UNIX_TIMESTAMP()` are pinned to one timestamp per statement, read at plan time from the session time zone where one is set and the host clock and timezone otherwise. The MySQL wire endpoint implements `SET time_zone` per connection; the HTTP endpoint has no equivalent session state, and the session zone does not affect `CONVERT_TZ` or stored temporal values.

- Date parsing accepts the canonical date and date-time forms implemented by the M2 evaluator. `DATE_ADD` and `DATE_SUB` accept one interval field at a time; compound intervals such as `INTERVAL '1-2' YEAR_MONTH` are not implemented (#13). Compound qualifiers are rejected early by the SQL parser rather than during engine binding: sqlparser 0.62 only accepts simple interval unit keywords, so a compound qualifier fails with `INTERVAL requires a unit after the literal value`; supporting them requires the parser to accept the qualifier first (an upstream change). `EXTRACT` covers `YEAR`, `MONTH`, `DAY`, `HOUR`, `MINUTE`, `SECOND`, `QUARTER` and `WEEK`; compound units reject explicitly.

- `STR_TO_DATE` translates the MySQL format string into the underlying parser's dialect, mapping `%c %e %M %k %l %i %s %f %%` and forwarding the rest. Several letters mean something different there, so a directive outside that set parses against the wrong field rather than raising — the same defect `DATE_FORMAT` carried until it was rewritten to render each directive itself. Rendering cannot be reused here: parsing needs a real parser, not a formatter run backwards. `DATE_FORMAT` now implements MySQL's full directive inventory, including the four `WEEK` numbering modes behind `%U %u %V %v` and their paired years `%X %x`, and copies an unrecognized directive's bare character the way MySQL does.

- Pintail maps an empty scalar-subquery result to `NULL`. During oracle development, MySQL 8.4's constant `SELECT` with `LIMIT 0` produced a special-case result that did not follow this behavior; that MySQL-only corner is excluded from the common-workload corpus.

- Integer and floating arithmetic use Pintail's current `Int64`, `UInt64`, and `Float64` execution types. `DECIMAL` values are stored losslessly, and the operations MySQL keeps exact over exact numerics are exact here too: division (`/`) and `AVG` produce a DECIMAL widened by four fraction digits with half-away-from-zero rounding, `SUM` accumulates scaled integers, and CASE/IF/COALESCE branches that mix decimals with integers unify to a decimal instead of truncating; `+`/`-`/`*` over decimal columns, casts, and literals compute exactly on scaled units, and `CAST(x AS DECIMAL(p, s))` rounds half away from zero. Remaining gap: chained expressions whose intermediates are division results (`a / b / c`, `(a / b) * c`) round each step to its own result scale while MySQL carries extra unrounded digits between steps. Numeric overflow returns an error.

- `REPEAT`, `SPACE`, `LPAD`, and `RPAD` cap their result at 4096 bytes and error beyond it; MySQL's ceiling is `max_allowed_packet`. `FORMAT` uses en_US grouping only (no locale argument).

### Planning and execution

- Text keys, numeric/string coercions, out-of-range signedness conversions,
  synthetic append-row IDs, undeclared mappings and composite keys remain
  correct but deliberately skip physical range pruning.
- A snapshot containing only one relevant segment has no smaller storage
  morsels to parallelize.
- Views below 65,536 candidate rows use the simpler materialized merge path,
  which remains covered by the query memory ceiling.
- `DISTINCT` state, `GROUP_CONCAT`/`JSON_ARRAYAGG` aggregation, the
  single-column direct-path aggregation, and materialized query outputs do not
  spill and still fail at the memory ceiling. A grace-partitioned join errors
  when one join key's own rows exceed the ceiling. Spill files carry no disk
  quota.
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
  `needs_resync`.
- A source charset outside utf8mb4/utf8mb3, ASCII and latin1 (cp1252) is
  quarantined through the DLQ.
- `binlog_row_metadata` may be MINIMAL or absent (MySQL 5.7, MariaDB): column
  identity is then ordinal against the probed schema, enum/set labels and
  charsets come from probed declarations, and unsigned integers are
  reinterpreted at their declared width because MINIMAL row events omit the
  SIGNEDNESS field. `binlog_format=ROW` and `binlog_row_image=FULL` are hard
  requirements; a non-FULL row image demotes the source to polling.
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
- If several schema changes occur while Pintail is offline and the final source
  schema no longer represents an event's intermediate shape, Pintail
  quarantines the incompatible table rather than reconstructing historical
  layouts from SQL text. A table resnapshot is then required.
- Auto-inclusion uses case-insensitive exact allow/deny names and requires a
  writable target root; glob patterns and dashboard rule editing are not
  implemented. DROP TABLE retains the replica as an orphan with no operator
  purge action.

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
- The endpoint is read-only. `SET sql_mode` is stored and echoed with no
  semantic effect. Multiple SQL statements in one command are not supported.
- Certificate rotation requires a restart. The HTTP endpoint still expects a
  TLS-capable ingress when exposed across a network.
- DBeaver and Metabase application-level smokes are not automated on this
  workstation.
- Binary result columns carry `BINARY_FLAG`, but `opensrv-mysql` 0.7 hardcodes
  column charset 33 (utf8) in result metadata, so clients that detect binary
  columns via charset 63 — mysql2 and most connector libraries — decode raw
  binary bytes as text. The bytes on the wire are the exact stored value.

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
