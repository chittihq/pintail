# Changelog

All notable changes to Pintail are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Removed

- The optional x86-64-v3 second binary and its launcher
  (`PINTAIL_X86_64_V3`, `scripts/pintail-launch.sh`).

### Added

- The audit log is pruned: events older than `PINTAIL_AUDIT_RETENTION_DAYS`
  (default 90; `0` keeps every event) are deleted hourly and once at
  startup, 10,000 per transaction, so audit writes and requests are never
  held up for long. A value that is not a whole number of days logs a
  warning and keeps the default. The setting appears as `audit_retention`
  on the `pintail limits:` startup line, and `GET /api/storage` reports it
  with the last pass under `metadata.audit_retention`. Metadata migration
  25 adds the index the pruning needs.
- Dead letters are pruned the same way: letters older than
  `PINTAIL_DLQ_RETENTION_DAYS` (default 30; `0` keeps every letter) are
  deleted on the same hourly pass, reported as `dlq_retention` on the
  startup line and under `metadata.dlq_retention`. The dead-letter list, its
  counts and the `pintail_dead_letters` gauge stay consistent; discarding
  or retrying a pruned letter answers 404.

### Changed

- The published image's server is profile-guided: built instrumented,
  trained on a workload that needs no source server, and rebuilt with the
  profile, for each platform's generic target. On the 20M-row benchmark it
  is about 6.5% faster across Q1-Q8 and 11% faster on a key lookup, with
  identical answers; its startup line reports `build_variant=pgo`.
  `--build-arg PINTAIL_PGO=0` builds a plain binary, and
  `docker-compose.dev.yml` builds plain by default.
- Temporal functions over a column with few distinct values (DATE_FORMAT,
  the week and day functions, DATE_ADD/DATE_SUB and interval arithmetic,
  EXTRACT and the date parts, TIME formatting) are evaluated once per
  distinct value instead of once per row, and a constant temporal
  subexpression no longer drops the expression around it to the row path.
  The calendar families of the differential corpus run 4.4-20x faster, all
  within 1.5x of MySQL; invalid and zero dates, fractional seconds and the
  order of warnings are unchanged.
- A named session time zone is prepared once per statement and a column's
  distinct timestamps are converted once, not per row (the America/New_York
  corpus family runs 19% faster). TIME columns compared with text share
  one parsed carrier per value, and a floating comparison is finished in
  the vector path (about 29% faster on that family).
- A `TIMESTAMP` column kept as stored text (one holding the zero
  `TIMESTAMP`), read in a named session time zone such as
  `America/New_York`, converts its text with a cursor over the zone's
  transitions instead of the general conversion per row. Comparisons with
  a constant run about twelve times faster, and the corpus family for a
  named zone is level with MySQL.
- Row constructor `IN`/`NOT IN` against an uncorrelated subquery of up to
  64 rows reads the members once and compares each row with them in a
  batch, instead of answering a subquery per row; NULL answers are
  unchanged (the corpus family runs about 4-5x faster). Correlated
  subqueries answered row by row no longer copy each subquery's bound tree
  per outer row (the batched dependent-subquery family runs 2.5x faster).
- A text column that becomes dictionary-coded reserves its codes for the
  whole chunk up front instead of growing them by reallocation; filtered
  scans over such columns run up to 40% fewer instructions.
- A row membership test over values that cannot be NULL drops its
  undecided-row bookkeeping, and a replayed join rejects unequal keys
  before resolving its subqueries. A filtered count reading only its
  predicate column runs 11% faster on the 20M-row benchmark.
- Reading a format 7 block with one- or two-byte dictionary indexes no
  longer widens them on the general path, and a block that stores no null
  bitmap skips counting one: text scans of format 7 data take 14% fewer
  instructions and wide scans no longer read slower than format 6.

### Fixed

- A GROUP BY or DISTINCT over two case-insensitive text columns could show
  a group with the spelling of a later row instead of its first row in
  primary-key order, when the table was folded in parallel, when the query
  paused near its memory ceiling, or when the groups moved to the hashed
  fold partway through. Counts and totals were always correct. Introduced
  in 0.1.7.
- A parallel fold that cut an oversized slice smaller placed the rest of
  that slice after later slices; anything that depends on scan order now
  sees the rows in key order. Introduced in 0.1.7.
- String functions that build more than they read (REPEAT, SPACE,
  LPAD/RPAD, INSERT, CONCAT, CONCAT_WS, REPLACE, TO_BASE64, and the JSON
  constructors and modifiers) answer NULL with warning 1301 once the result
  would pass `max_allowed_packet`, sized as MySQL 8.4 sizes it, instead of
  erroring past 4 KiB or reserving memory for results that cannot exist.
  `SET SESSION max_allowed_packet` answers error 1621.
- `max_execution_time` stops a long LIKE, JSON_SEARCH, regular-expression
  search, JSON_CONTAINS or JSON_OVERLAPS within a single value, and KILL
  QUERY also stops the column LIKE kernel and those functions.
- DISTINCT, GROUP BY, COUNT(DISTINCT) and unions inside derived tables
  over a TIMESTAMP in a daylight-saving session zone follow the sql_mode as
  MySQL does: under NO_ZERO_DATE, NO_ZERO_IN_DATE or ALLOW_INVALID_DATES
  (the default mode included) the two instants of the repeated hour are one
  value, and without those flags they stay two. WITH ROLLUP keeps them
  apart and orders by instant.
- A DATE or DATETIME that no calendar has (a day past its month's end, a
  zero month or day) is written as `0000-00-00` when grouped,
  deduplicated, counted distinct or unioned under a sql_mode that rejects
  it, as MySQL does; column statistics that prove a column holds only real
  dates skip the check.
- TIMESTAMP grouping, DISTINCT and COUNT(DISTINCT) keep instants apart
  where MySQL reads the column through a covering source index, which
  Pintail learns from the source's index definitions. Joins and IN between
  TIMESTAMP columns compare session-zone readings, as MySQL's hash joins
  do, and instants when a joined column leads a source index. A top-level
  UNION over TIMESTAMP columns removes duplicates and orders by instant in
  modes without the date-validation flags. Differences against MySQL on a
  6,090-query matrix of zones and modes fell from 1,159 to 202; the rest
  are listed in docs/limitations.md.
- UNIX_TIMESTAMP of a TIMESTAMP column returns its stored instant in a
  daylight-saving zone instead of folding the repeated hour onto the
  earlier one.
- Zero and partial DATE/DATETIME values pass through COALESCE, IFNULL, IF,
  CASE, GREATEST, LEAST, NULLIF and an explicit CAST instead of becoming
  NULL; a TIMESTAMP column's zero still casts to NULL under NO_ZERO_DATE.
- A UNION of calendar columns with different precisions, or of DATE with
  DATETIME, is accepted and typed at the finer precision instead of
  raising a syntax error.
- A dead-letter retry whose letter was removed while its reconcile ran
  answered 404 after the table had already been reconciled; it now
  succeeds.
- `EXISTS` / `NOT EXISTS` over an ungrouped aggregate whose `HAVING`
  aggregates nothing (`HAVING 1`, or a condition on the outer row only)
  answered false on empty input and raised a cardinality error on several
  rows; a `HAVING` naming the subquery's own select alias read an outer
  column of that name instead. Introduced in 0.1.7.
- `ORDER BY <primary key> DESC LIMIT n [OFFSET m]` returned the smallest
  keys on tables read with unique-key visibility (polling replicas, and
  change-capture tables with a secondary UNIQUE key that need
  reconciliation). Introduced in 0.1.7.
- The block cache no longer releases the memory charge of blocks running
  scans still hold, so memory pressure cannot report as free memory that is
  still in use. Deleting segment files no longer leaves stale keys in the
  cache's eviction queue, which grew without bound on a long-running
  server.
- The sparse-index cache counts the text and binary bytes of primary keys,
  so tables with long text keys stay within its 16 MiB bound.
- GROUP BY on date parts no longer folds in parallel with per-worker totals
  that were never charged to the query's memory limit; when they do not
  fit, the query takes the ordinary aggregation path.
- A batched IN/EXISTS subquery that fails part-way gives back what its
  discarded results were charged, so the per-row fallback no longer fails
  with a spurious memory-limit error.
- The query audit queue is bounded at 32 MiB. When the metadata store falls
  behind, HTTP queries wait for room instead of the server buffering every
  statement's text without limit. A batch refused because the store is
  locked is retried whole a few times and then reported lost, instead of
  each event waiting out its own five-second busy timeout.
- A statement that reads no table and uses LIKE, JSON_SEARCH or a function
  whose result can be many times its input (REPLACE, HEX, QUOTE,
  TO_BASE64, JSON_QUOTE) runs on a worker thread, so it no longer stalls
  the other connections on the thread it arrived on. KILL QUERY stops a
  long LIKE match partway through.

## [0.1.7] - 2026-10-03

Everything in 0.1.7-rc1 through rc3, plus the startup resync advice below,
gated with the full stable chain: fmt, typecheck, unit, parser corpus,
oracle, MTR, MTR replayed through change capture, E2E on MySQL 8.4 and 8.0,
migrations, browser, compose, BI clients, the 20M-row benchmark, TPC-H and
acceptance, then freshness and acceptance again on the banked tree; and a
100-cycle crash and restart run under live writes with no parity failures.

Benchmark, TPC-H and acceptance evidence for this release was measured on
16-vCPU hosts (the harness caps each engine at 8 CPUs and 8 GB).

**Upgrading:** a data directory this release writes to holds segment format
7, which earlier releases cannot open; take a backup before upgrading if a
rollback may be needed. Tables with a signed `MEDIUMINT` or `BINARY(n)`
column that applied streamed changes under an earlier release are named at
startup and should be resynced.

### Added

- At startup, a warning names each replicated table that may hold values
  change capture stored wrong before 0.1.7-rc3: a negative signed
  `MEDIUMINT` stored 16777216 too high, or a `BINARY(n)` value missing its
  trailing zero bytes. A table is named only when its schema has one of
  those column types, its copy came from an older binary, and it has
  applied streamed changes since that copy. The warning names the
  database, table and columns and points to the table's Resync action or
  `POST /api/databases/{id}/tables/{name}/resync`; the table's snapshot
  status carries the same as `resync_advised`, and a resync clears it.
  Nothing is resynced automatically. A table copied by 0.1.7-rc3 itself is
  named once, since that release did not record which binary copied it.
  `PINTAIL_LOG` also accepts `warn`.

## [0.1.7-rc3] - 2026-10-03

Aggregates folded on the worker that decodes the data, with CPU per query
roughly halved on grouped and join shapes and twice the throughput under
concurrent clients; AVX2 decoding kernels, a narrower segment format and a
shared block cache; small statements and bounded scans answered in
microseconds; change capture applied about twice as fast; background
merges that converge under writes; and replication fixes for negative
MEDIUMINT values, BINARY padding and JSON documents holding DECIMAL or
temporal values.

### Added

- The server logs which build it is and which execution paths it runs:
  two lines after `pintail limits:` (`pintail optimizations:` and
  `pintail paths:`) give the CPU and its instruction sets, the vector
  kernels' dispatch level, the build's target level and variant (standard,
  pgo, pgo+bolt), both pool widths and where they came from, each
  switchable path on or off, and a `non_default` list of every setting
  that moves the process off its defaults. `GET /api/storage` returns the
  same as `optimizations`.
- Profile-guided builds train on a workload that needs no source database
  (`scripts/pgo-build.sh`, `benchmark/pgo-train.ts`), and the image can
  carry a second, x86-64-v3 binary behind a launcher that picks by the
  CPU's features (`PINTAIL_X86_64_V3=1` at build time, off by default).
  `docs/release-builds.md` records what each build setting measured.

### Changed

- Segment files are written as format version 7: dictionary indexes are
  stored one or two bytes wide, and a block with no NULL stores no null
  bitmap (segment files about a tenth smaller on the 20M-row benchmark).
  Versions 1-6 remain readable; existing segments are rewritten only as
  compaction or a recopy replaces them. **Downgrade:** a binary older than
  this release refuses any version 7 segment, so a data directory this
  release has flushed to, compacted or recopied cannot be opened by an
  older release; restore a backup taken before the upgrade, or
  re-snapshot.
- Blocks are read in positioned runs rather than one read each, and one
  block cache serves key lookups and scans. `PINTAIL_BLOCK_CACHE_MB` sets
  its budget (0 turns it off; the default is 1/128 of available memory,
  between 32 MiB and 1 GiB). It is charged to the shared memory budget and
  gives its memory back when a query needs it.
- Bit-packed integer columns decode through an AVX2 kernel where the CPU
  has one (four values a step instead of one), and the dictionary codes of
  a GROUP BY over text keys with at most eight values translate with a
  register permutation. One binary still runs on every x86-64 CPU;
  `PINTAIL_SIMD=off` runs every kernel's portable fallback for diagnosis.
- A session in a named time zone (`America/New_York`) reads a `TIMESTAMP`
  column a batch at a time, as a session at a fixed offset already did:
  grouping by `DATE()`, `HOUR()` or `DATE_FORMAT()` of it no longer
  converts every row through its text, and a range filter on it looks the
  zone up only for rows within a day of the range's ends.
- Work that ran on a statement's own thread between rounds of a scan - a
  computed grouping key such as `DATE(col)`, text interning, per-batch
  memory bounds - runs on the workers or once per batch. A statement's
  audit row is written after its response is sent, so a crash can lose
  that row; it never delayed or failed the statement's data.
- Change apply decodes row images with a plan compiled once per table
  map and moves a batch's rows into the memtable instead of copying them:
  about 2.3 times the rows per second on update-heavy catch-up, and half
  the CPU per row.
- HTTP statements with a session token no longer write an audit row per
  request thread; one writer commits them in batches, and a query's audit
  row carries the time it was queued. Session authority, a query's
  database check and a wire login's reads are read once per metadata
  write instead of per request, so an edit to users, memberships, keys or
  databases made by another process is seen within five seconds; one made
  by the server itself is seen by the next request.
- A scan under `LIMIT` reads the rows the limit can take: with or without
  predicates, with `OFFSET`, and from the end for `ORDER BY <whole
  integer key> DESC`. `SELECT * ... LIMIT 10` on a 100,000-row table
  decoded 600,000 values and now decodes 60.
- A join whose WHERE is an OR of conditions that each repeat the join
  equality joins on that equality instead of pairing every row of both
  tables.
- Merging for fewer files stops when no merge would leave fewer, where it
  rewrote the same files every cycle on an idle replica. A cluster no key
  range of which can be folded is merged whole. Background merges are
  bounded in width, run at lower priority and report their state in
  `/api/storage`; `PINTAIL_MERGE_THREADS` and
  `PINTAIL_MERGE_WRITE_BYTES_PER_SEC` tune them.
- A grouped COUNT with a SUM keeps each group's row count and 64-bit sum
  in one cell, widening in place before a sum could leave 64 bits. The
  integer-range fold checks keys as it computes slots instead of reading
  every window's bounds first, and each worker keeps its own slots. COUNT
  of an unsigned integer column no longer sends a dense-key aggregate to
  the scatter. The fused join resolves keys in 64 bits.
- A repeated statement runs from a kept plan (`PINTAIL_PLAN_CACHE=0`
  turns it off); a kept key lookup or short scan runs on its connection's
  thread when nothing else is being answered (`PINTAIL_SMALL_READS`); the
  query log line is written by a background writer; and the server proves
  a replica current from in-process counters instead of asking the file
  system. On one connection a repeated key lookup fell from about 58 to
  about 21 microseconds.
- A statement that reads no table runs on its connection's task, a
  command is read in one read, and a key lookup decodes a block once and
  keeps it. `PINTAIL_INLINE_STATEMENTS=0` restores the worker hand-off.
- A LIMIT stops a key-lookup join, an `IN (subquery)` membership test and
  a key-order scan once its rows are found.
- More correlated subqueries are answered for a batch of outer rows at once
  instead of once per row: `EXISTS` and `NOT EXISTS` over a join, `IN` and
  `NOT IN` (with their NULL answers computed from the same members), a
  scalar subquery over a join that is not an aggregate, a row constructor
  against a subquery, a subquery in the select list of another, and a
  subquery in an outer join's ON condition that reads the join's left side.
  `EXISTS` over an aggregate with no GROUP BY is answered without running
  it, as its HAVING condition where it has one.
- An aggregate over a scan folds each slice on the worker that decoded it
  (grouped, a join's probe side, ungrouped, `COUNT(*)`), instead of handing
  decoded batches to a second pool. CPU per query roughly halves on grouped
  and join shapes, and throughput at 8-16 clients rises 73-127%.
  `PINTAIL_DISABLE_FUSED_FOLD` turns it off; the startup report shows it as
  `fused_fold`.
- A statement's profile (`PINTAIL_PROFILE`, `EXPLAIN ANALYZE`) lists what its
  correlated subqueries did - set-at-a-time executions, hash indexes built,
  per-row executions - and the reason each one left to the per-row path was
  left there.

### Fixed

- A negative value in a signed `MEDIUMINT` column arrived through change
  capture 2^24 too high (-1 became 16777215) once the row had changed at
  the source. Rows copied by a snapshot were right; recopying a table
  repairs values already stored wrong.
- A lookup on a table dropped and created again, or recopied, with the
  same columns and row count could be answered from the earlier table's
  rows: an equality, `IN`, `ANY`/`ALL` or correlated lookup then missed
  rows the table holds. The lookup index's cache identified a segment
  file by its path and shape; it now also carries the file's length and
  modification time.
- `ALTER TABLE ... RENAME` is followed as the rename it is. It was treated
  as a recopy, and a table renamed away and back stayed "not ready".
- A conditional (`IFNULL`, `COALESCE`, `IF`, `CASE`, a `LEAD`/`LAG`
  default) mixing a JSON branch with another type answers as text, as
  MySQL does; it answered 0 for the document.
- A subquery's `GROUP BY` or `HAVING` reads the subquery's own alias
  before an outer column of the same name. Derived tables accept a
  column list.
- A statement waiting for a table being recopied could keep seeing "still
  copying" after the copy had finished, until an unrelated metadata write.
- A correlated subquery whose aggregate sits under a function, such as
  `COALESCE(SUM(x), 0)`, was classified as volatile and executed once per
  outer row with no sharing between rows. It is now memoized and batched
  like the bare aggregate.
- A `BINARY(n)` value written after the initial copy lost its trailing zero
  bytes, so equality, keys and joins on the column missed it.
- A JSON document holding a DECIMAL, date/time or binary value made its
  table resync. Such documents now replicate and print as MySQL prints
  them, with the DECIMAL's scale kept (`1.50`).
- CHAR values copied from a source running `PAD_CHAR_TO_FULL_LENGTH` kept
  their pad spaces.
- `GROUP BY ... WITH ROLLUP` blanked constants and still-grouped expression
  keys in subtotal rows, rolled up every item over an aliased key's column,
  and lost the names of `GROUPING()` columns.
- An unqualified outer column whose name also exists in the table of a
  select-list subquery was refused as ambiguous.
- Metadata writes could fail with "database is locked" while replication
  was writing: a transaction now takes the write lock when it begins.

## [0.1.7-rc2] - 2026-10-02

Replication that survives very large transactions, kills and schema
changes without recopying tables; tables with text or composite keys read
in place after writes; correlated subqueries answered for a batch of rows
at once; column-at-a-time aggregation across more shapes; closer MySQL
parity for dates, times, sums and case mapping; and workspace and TLS
hardening.

### Security

- A node told to require TLS on the MySQL wire port (`PINTAIL_WIRE_REQUIRE_TLS`)
  with no certificate configured now refuses to start when it cannot prepare
  or load its own certificate. It used to start without one and then accept
  plaintext connections, since the listener had nothing to refuse them with.
  With TLS optional the node still starts and serves without it.
- The wire listener now holds the TLS requirement on its own, apart from the
  certificate. A listener that requires TLS and has no certificate refuses to
  run, and a plaintext login on a required-TLS listener is closed whether or
  not a certificate is present.
- The live event streams (`/api/events`, `/api/ws`) deliver a dashboard
  session only the events of databases its own workspace owns. Every session
  used to receive every workspace's events: database identifiers, table names
  and replication messages.
  An open stream now ends within seconds of its account being disabled, its
  member being removed from the workspace, or its API key being revoked; it
  used to run until the client hung up. Events that belong to no database
  reach node administrators only.
- Node-wide settings (the Google sign-in client and the wire certificate's
  hostnames) now require a node administrator: an administrator of the node's
  first workspace. Any account could create a workspace, become its
  administrator, and change them. `/api/session` reports `node_admin`, and the
  dashboard shows those settings only to a node administrator.
- A database's replication mode can be changed only from the workspace that
  owns it. The mode endpoint wrote the change before checking the caller's
  workspace, so an operator of one workspace could pause or re-mode another
  workspace's database by its identifier.

### Fixed

- A transaction large enough to write a log record over 128 MiB, followed
  by a kill, left a record that recovery refused: every table of the
  database went to the error state on every later cycle. The writer no
  longer writes such a record, a large batch is stored as segments
  published in one step, and recovery replays any complete, checksummed
  record.
- A kill between storing rows and committing the checkpoint made the
  restart recopy every table. Replayed changes a table already holds are
  now recognised and skipped.
- A merge deleted its input files while a running scan still read them
  ("open segment" errors under sustained writes). A finished background
  merge was published only by the next flush, so a table under continuous
  writes could collect thousands of segment files; merges now publish
  when they finish and idle tables are compacted.
- ADD, DROP and RENAME COLUMN are applied as of their own statement. They
  were applied from the source's current schema, so a column changed again
  seconds later forced a full recopy of the table.
- Reset mirror and resnapshot could wait indefinitely behind the
  replication loop. Operator actions are admitted ahead of it, a reset
  survives a crash, the catch-up after a snapshot's copy is bounded, and
  the dashboard says what an action is waiting on.
- A statement naming a table that is being recopied after a schema change
  waits for it (`PINTAIL_TABLE_RECOPY_WAIT_MS`) instead of failing.
- Aggregations under a memory ceiling: a range or dense fold, a
  small-group fold whose keys keep coming, and a fused join whose groups
  outgrow the ceiling now hand over to the spilling path instead of
  failing or exceeding it.
- MySQL parity, each confirmed against MySQL 8.4: SUM and AVG of doubles
  add in row order; variance follows MySQL's recurrence; doubles past 1e15
  print as MySQL prints them; keys merge under their own collation; ENUM
  and SET columns keep their declaration order on rows built from the
  memtable; a MIN/MAX no longer returns a stale extreme.
- Date and time parity: dates with a zero month or day, text dates past a
  month's end under ALLOW_INVALID_DATES, intervals and weeks at the
  calendar's edges, zero and out-of-range TIMESTAMPs in every session
  zone, text read as a TIME, a TIME column compared with text, TIMEDIFF of
  a zero date under NO_ZERO_DATE, and PERIOD_ADD/PERIOD_DIFF.

### Performance

- Aggregates fold a column at a time: ungrouped and few-group aggregates,
  packed lanes over integer, date-part and text keys, composite and
  sparse integer keys, COUNT(DISTINCT) on packed keys, decimal moments,
  text MIN/MAX and bit aggregates. Selection runs a 64-row word at a
  time, and the new `pintail-simd` crate supplies safe vector kernels
  with runtime dispatch.
- Scans skip blocks whose stored min/max rule out a range filter, keep a
  scattered filter's exact rows, decode filter-first where a filter
  rejects rows, and unpack bit-packed integers sixty-four at a time.
- Joins: a LEFT join fuses into the grouped aggregate above it, a dense
  integer build goes straight into a flat table, and spread-out integer
  keys hash into one flat table (a fused join over a 2M-row sparse-key
  build fell from about 1.4 s to about 0.19 s in the in-process harness).
- A correlated scalar subquery that could not become a join is answered
  once for a batch of outer rows instead of once per row, and select-list
  subqueries under a LIMIT run for the kept rows only. A paged report with
  five such subqueries fell from seconds to a few hundred milliseconds.
- Tables with a text, binary or composite primary key are read in place
  after writes. They fell back to merging the whole table row by row; a
  report page over such tables went from about 90 s to under 1 s, and a
  point lookup from about 0.7 s to about 1 ms.
- Text columns with few distinct values: values are numbered once per
  segment, and a scan skips blocks holding no value the predicate
  accepts, for `<>`, `NOT IN`, `LIKE`, `IS NULL` and functions of the
  column. GROUP BY on a 200-value column is about 5× faster.
- The side index answers text-key lookups from predicates and join keys,
  and its cache is sized from the process's memory
  (`PINTAIL_SECONDARY_INDEX_CACHE_MB` still overrides).
- Written tables: changes apply to decoded chunks, the memtable is read
  as arrays built once, newer segments are read where they are stored,
  and the key index over them is a merge tree extended after each flush.
- Replication applies source transactions in batches under one
  checkpoint and stores a spilled transaction in bounded pieces: peak
  memory for a 1M-row transaction fell from about 3.3 GB to under 0.5 GB.
  Opening the tables at each cycle fell from seconds to tens of
  milliseconds on a database with many tables.

### Changed

- SUM of an integer column is a DECIMAL, as in MySQL (it was an integer
  that failed past 64 bits). The total is exact at any size. A client
  that reads DECIMAL as text - the HTTP API's JSON, and drivers that do
  not convert it - now sees `"123"` where it saw `123`.
- UPPER and LOWER map one character to one character by the collation's
  own table, as MySQL does, instead of the full Unicode mapping. Comparing
  two columns whose collations tie is error 1267, and a UNION of them
  error 1271, where both were answered before.
- `PINTAIL_LAYER_INDEX_MB` sizes the key index over a table's newer
  segments; the fixed 256 MB cache it replaces is gone.
- A select-list subquery under a LIMIT is not run for rows the LIMIT
  discards, so an error it would have raised on a discarded row is not
  raised.

## [0.1.7-rc1] - 2026-10-01

Two of 0.1.6's known regressions fixed, faster grouped aggregation, a
side index that now answers text lookups and limited sorts, and a planner
that orders joins from per-column statistics instead of fixed guesses.

### Fixed

- Scans that test a dense text column on every row are faster than in
  0.1.5 again (about 24 ms to 14 ms on the uniform 20M-row probe): a
  dictionary block's codes are appended with one sized extend instead of
  a per-row loop.
- A column that a scan both filters on and returns - the key of a join's
  first input, or a date column under a range - was decoded twice. It is
  decoded once, which removes the extra blocks the TPC-H Q05 profile
  showed (a mixed-selectivity probe fell from 129 ms to 74 ms).

### Performance

- Grouped aggregation adds COUNT, SUM, AVG and MIN/MAX over decimals into
  plain per-group totals and reads grouping keys and values straight from
  packed storage; COUNT(DISTINCT) partial results merge by set union. On
  the 20M-row benchmark with the result memo off, Q6 (top 10 spenders) is
  about 1.6× faster and Q7 (regional analytics) about 1.9×.
- The side index answers equality and IN filters on text columns under
  the comparison's collation, keeps unflushed rows it rules out out of the
  scan, and lets `ORDER BY <integer column> LIMIT k` read only the rows
  that can come first.
- Each segment keeps a small distinct-value sketch per column. Join
  ordering and the choice of which side of a hash join to read first use
  these estimates (equality and IN from distinct counts, ranges from
  min/max), so a join run whose last table is filtered to a sliver is
  reordered to read it first, and a small side's keys filter the large
  side. Single-table queries never build the statistics.

### Changed

- The manifest format is version 4 (it carries the sketches). Earlier
  manifests still read and fall back to the old estimates.

### Known issues

- The Q2 (filtered count) and Q5 (monthly revenue) slowdown first reported
  here did not reproduce. Measured back to back against 0.1.6 on one box,
  0.1.7-rc1 is level on both (Q2 minimum 22 to 25 ms against 25 to 26, Q5
  48 to 52 ms against 49 to 53) and keeps its Q3, Q6 and Q7 gains. The
  earlier figures compared runs from two different hosts.
- The 0.1.6 slowdown of Q8 (join users + orders) did not reproduce on
  native or in-process runs; on this release's containerized run Q8 is
  within 5% of 0.1.6.

## [0.1.6] - 2026-09-30

Everything in 0.1.6-rc1 through rc3, gated with the full stable chain: fmt,
typecheck, unit, parser corpus, oracle, MTR, E2E on MySQL 8.4 and 8.0,
migrations, browser, compose, BI clients, the 20M-row benchmark, TPC-H,
acceptance, then freshness and acceptance again on the banked tree.

The release was measured against a real reporting workload replayed on
both engines with every answer diffed against MySQL: across 300 queries
Pintail answered the same rows (231 identical, 66 differing only in the
order of an unordered result, and 3 plan-dependent cases already known),
raised no errors, and finished the set in 11.5 s against MySQL's 93.9 s.

Benchmark evidence for this release was taken on 8-vCPU hosts and is not
comparable with the figures banked for 0.1.5, which came from different
hardware. The release-against-release studies ran both versions on the same
host, one after the other: TPC-H SF1 Q03 is 2.0× faster than 0.1.5, Q05
1.48× and Q10 1.42×, and Q05 holds a 290 ms median at the default spill
limits. Every answer in every run matched MySQL exactly.

### Known regressions

- On the 20M-row analytical benchmark with the result memo off, Q8 (join
  users and orders) is 17% slower than 0.1.5 by median and 13% by minimum;
  Q1 (full count) is about 0.3 ms slower on a 2 ms query. Answers are
  unaffected.
- Scans that test a dense text column on every row are about 23% slower
  than in 0.1.5 on the uniform 20M probes; mixed-selectivity and numeric
  filters are 7% and 13% slower. The same blocks are decoded in both
  versions, so the cost is per row, not extra I/O.
- The TPC-H Q05 profile decodes 2,132 fact blocks where 0.1.5 decoded
  1,684 on the same replica. Its time still improved; the cause is being
  investigated for the next release.

## [0.1.6-rc3] - 2026-09-30

Listing a source's tables no longer scans it, a side index answers point
and join-key lookups inside segments, and the planner reads pinned keys
and constant lists before it chooses a join order.

### Fixed

- Listing a source's upstream tables, adding tables from it, and the
  supervisor's periodic drift check ran `COUNT(*)` on every source table:
  up to 30 seconds of index scan per table and 300 per probe, repeated on
  each visit and each reconciliation. They now read the source's row
  estimates from `information_schema.TABLES` and issue no count; the
  counts they report are marked as estimates. Registering a database and
  the re-probe before a snapshot or repair still count exactly.

### Performance

- Segments can carry a side index over an integer column: per-value row
  postings that answer an equality, an `IN` list or a join's key set by
  reading only the rows they name. It is on by default, built on first use
  and persisted in the segment file (segment format 6; formats 1-5 still
  read). A probe naming more than a quarter of a segment reads it whole
  instead.
- An inner join run with three or more tables reads a relation pinned by
  its primary key first, so its keys filter the rest; row estimates count
  a relation's own filters.
- A small probe key set filters a build side beneath a join or an
  integer-keyed DISTINCT on that side, down to the table the key comes
  from.
- `column IN (constants)` on an integer or date column bounds the scan by
  the list's span, so segments outside it are skipped.

### Changed

- Segments written by this release use format 6 and are not readable by
  earlier releases.

## [0.1.6-rc2] - 2026-09-30

Measured against a real reporting workload replayed on both engines, with
every answer diffed against MySQL: joins, scans and sorts got faster on the
report shapes that trailed, several wrong-answer and restart faults were
fixed, and the control-plane metadata is now checked and backed up.

### Fixed

- A LEFT join whose table name was reused inside a derived table could be
  treated as an inner join and lose its unmatched rows.
- The settled-answer memo keyed on only part of a query's filters and join
  conditions, so two queries differing in one condition could share an
  answer. It now keys on every one of them.
- Calendar date parts (year, month, day) no longer share the packed group
  key of time units, which merged groups that differ.
- Filter-first scan predicates compile under the query's table alias.
- A narrowed key-range scan keeps its overlay key, so rows changed since the
  last flush are not lost from it.
- SIGTERM always completes: shutdown is bounded even while event streams
  are open.
- A first snapshot interrupted by a restart resumes as a snapshot rather
  than as a table repair, and one that copied every table but never handed
  off is handed off at the next start. Tables are restored at restart only
  for a database that handed off.
- A table left half-copied by a failed job is quarantined instead of being
  served.
- A resumed snapshot copy retires the chunks it reads again.
- Comparisons across column collations resolve by charset, then binary.
- Dates a lenient source stores are kept instead of being read as NULL.
- Hex literals read as numbers where a number is wanted, including in an
  IN list and under negation; negated text and truth values read as
  numbers.
- A decorrelated subquery joins after every comma-list item its condition
  reads.
- Columns leading a non-unique index report `MUL`.
- Spatial results carry MySQL's column metadata; `PI()` declares six
  decimals.
- A query waits on a full shared memory budget instead of failing.
- The metadata file is migrated when it is behind the schema, not only the
  first file opened, and is never reopened outside SQLite while live.

### Added

- The server takes exclusive ownership of its data directory at boot.
- The metadata file is integrity-checked, copied and pruned, and watched
  for changes.
- The table catalog is reconciled against the source; the dashboard lists
  upstream tables and adds them to the mirror.
- DECIMAL up to 65 digits replicates as an exact decimal.
- Spatial functions over replicated geometry.
- `GROUP BY ... WITH ROLLUP`, row constructor comparisons, row membership
  in a subquery, compound intervals from expressions, TIME as a duration,
  recursive CTE members, week and quarter intervals, regex position,
  occurrence and match type, SOUNDEX, trigonometry, microsecond EXTRACT
  units and BIT results.
- Text compared with JSON; MIN/MAX and window keys order by the JSON rules.
- Truncation and invalid-date warnings reach the diagnostics area.
- HTTP queries take a time zone and a timestamp.

### Performance

- Joins: a LEFT join turns inner when the WHERE rejects its null rows; an
  EXISTS semi join runs below the joins it does not read; a LEFT join's
  keyed bridge joins first; correlated EXISTS, IN and nullable NOT IN
  become semi and anti joins; inequality joins search a sorted input and
  semi/anti joins read its span in place; an ON clause's one-input
  conjuncts run in that input's scan; a join's probe keys filter the build
  scan and narrow it to the key span; a resident hash join probes a round
  of batches at a time on the pool; an oversized probe side is peeked
  before building.
- Scans: DATE and DATETIME ranges, including ones written through a cast
  or against `DATE(literal)`, bound the scan; wide columns are tested only
  on the rows narrow tests keep; scattered updates layer over their base
  segments; compacted segments stay on the layered scan path.
- Sorts and limits: each top-k batch is cut at its own k-th key; key-ordered
  groups stream under a limit; a short sorted prefix reads its other columns
  by key; a LEFT JOIN's preserved table is cut to the limit first.
- Aggregates: a large relation aggregates below its inner joins; a LEFT
  branch keyed by grouping columns folds first; aggregate rounds size to
  the memory the query has left; DISTINCT groups before sorting.
- Storage: segment format v5 stores large text blocks as independently
  compressed frames, so reading a few rows of a wide text column no longer
  decodes its whole block. Segments written by this release are not
  readable by earlier releases.

## [0.1.6-rc1] - 2026-09-27

Replication that survives a source upgrade, plus two query faults: a tight
memory ceiling that failed a query before it could spill, and an ORDER BY
that sorted by the wrong table's column.

### Fixed

- A source whose binary logs were renumbered, as a major-version upgrade or
  a restore onto a new instance does, no longer forces every table to be
  copied again. A GTID stream resumes from its transaction set alone,
  without naming the checkpoint's log file, which the source can no longer
  find.
- A source rebuilt under a new server identity numbers its transactions
  from one again, and each change would have been applied and then lost to
  the row it replaced. The stream now refuses a transaction numbered at or
  below the rows already stored and recopies the database.
- A full recopy of a database is logged at error level, so it reaches
  error reporting instead of passing as routine output. Dashboard events
  now log at the level they were published with.
- Under a tight query memory limit, a small table whose recent changes were
  not yet flushed could take the whole budget when its scan opened, and a
  join or sort in the same query failed instead of spilling. Such a scan now
  takes at most half the budget and streams past it.
- `ORDER BY b.id` over `SELECT a.id ... CROSS JOIN b` sorted by `a.id`, the
  selected column sharing the name. A qualified name now always means its
  own table's column.

### Added

- With `PINTAIL_PROFILE=1`, a query that fails logs its operator tree beside
  the error, so the operators holding the memory are visible, not only the
  one that asked last.

## [0.1.5] - 2026-09-26

Everything in 0.1.5-rc1 through rc9, gated with the full stable chain: fmt,
typecheck, unit, parser corpus, oracle, MTR, E2E on MySQL 8.4 and 8.0,
recovery, browser, compose, BI clients, the 20M-row benchmark, TPC-H,
freshness and acceptance on the banked tree.

Measured against 0.1.4 on TPC-H SF1, every answer byte-exact against MySQL:
Q05 31.7 s to 0.32 s with no spill, Q03 3.5 s to 0.27 s, Q10 2.5 s to
0.33 s, Q01 5.2 s to 3.6 s. Q05 holds a one-second p95 (347 ms) at the default
spill limits.

### Fixed

- A table renamed while its copy was cut short no longer sits in
  `snapshotting` forever, or in `needs_resync` on every repair cooldown after
  the next restart. A forced copy retires every tracked table its fresh probe
  no longer lists, and a per-table repair that finds its table gone does the
  same: the table is retained as a dropped table is, never streamed or
  served, and removable from the dashboard.

### Known regressions

- On the 20M-row analytical benchmark with the result memo off, Q3 (group by
  status) and Q4 (region by status) are slower than in 0.1.4: 129 to 173 ms and
  155 to 194 ms by median, with minimums up as well. Answers are unaffected.
  0.1.4 measured an improvement that the rc6 build no longer had; the cause is
  being bisected for 0.1.6.

## [0.1.5-rc9] - 2026-09-25

What production error reports asked for: the SQL shapes a deployed rc4 refused
or failed on - a correlated subquery inside an aggregate, the two-argument
TIMESTAMP, the UTC clock functions and a driver's read-only probe - plus a
way to let go of a table the source has dropped, and a cached-answer fault
that served one GROUP_CONCAT spelling for another.

### Added

- `TIMESTAMP(expr)` and `TIMESTAMP(expr, time)`, which combines a date and a time
  into one DATETIME.
- `UTC_TIMESTAMP()`, `UTC_DATE()` and `UTC_TIME()`, and the standard spellings
  `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `LOCALTIME` and `LOCALTIMESTAMP` (with
  or without parentheses) and `SYSDATE()`.
- `@@transaction_read_only` and `@@tx_read_only`, which drivers read to tell
  a read-only connection apart.
- A table the source has dropped can be removed from the dashboard (Remove,
  on the database page) or with `DELETE /api/databases/{id}/tables/{name}`.
  Its mirrored rows, schema history and replication state are deleted. The
  source is re-probed first, and a table it still has is refused.

### Fixed

- A correlated subquery inside an aggregate's argument, such as
  `SUM(x AND EXISTS (SELECT ... WHERE t.k = outer.k))`, failed the statement
  as an invalid physical plan. It is now evaluated
  per row ahead of the aggregate.
- Two `GROUP_CONCAT`s of one column over an unchanging table that differed
  only in their own ORDER BY or separator were answered from one cached
  result, so the second came back in the first one's order or separator.

## [0.1.5-rc8] - 2026-09-25

Change capture that follows every table a DDL touches: rc7 replaced the
forced database snapshot that repaired a quarantined table with a
one-table repair, and that snapshot had been hiding four capture faults.
Found by the first runs of the nightly replica gate.

### Fixed

- A table whose CREATE the stream could not parse (a ZEROFILL, INVISIBLE or
  PARTITION clause is enough) was never copied, and every read of it failed
  as an unknown table. It is now recorded as awaiting its first copy and
  copied from the source's own catalogue.
- A table dropped and re-created that way was copied but never followed
  again: later changes to it did not reach the replica.
- A column dropped and added back under its name read the dropped column's
  values, and a column added with a default left the rows already copied
  reading NULL. Such a change now recopies the table.
- A table quarantined by a second DDL within five minutes of its last
  repair stayed unreadable until the five minutes passed; only a repeat of
  the same cause is held back now.
- A comparison with a datetime literal that carries a time-zone offset
  (`'2015-01-01 10:10:10+03:30'`) ignored the offset.
- A binary string that is not UTF-8 failed in a numeric context instead of
  reading as the number its leading bytes spell, as MySQL does.
- `GROUP BY` a name two joined tables both hold refused as ambiguous where
  MySQL takes the select alias of that name.
- A negative-zero FLOAT printed as `0`, and DOUBLE arithmetic over numbers
  with a declared scale printed unfixed (`0` for `0.0`).

## [0.1.5-rc7] - 2026-09-25

Correctness under memory pressure and replays, a one-table blast radius for
locked and keyless tables, a streaming aggregate that spills over any input,
and the top-spenders shape back to its earlier speed.

### Fixed

- A range scan over part of a segment too large for its memory budget
  returned rows outside the range: the fallback that reads the segment in
  row slices decoded every row, unbounded by key. It now slices only the
  rows the range selects.
- A scan with a value predicate could answer with an older version of a
  row: a segment skipped on its statistics shadowed an older version the
  memtable still held after a replay, and the skip let that version stand.
  Such a segment is no longer skipped.
- A table another session held locked past the copy's bounded wait failed
  the whole snapshot, and every table in the database answered with an
  error until the next retry. The locked table is now flagged for a resync
  on its own and the rest of the copy completes.
- Under the `auto_resync` keyless policy, repairing one quarantined keyless
  table recopied the whole database and left every table not ready for the
  copy. It now recopies that one table.
- A grouped count over a text key with many distinct values failed at a
  tight memory ceiling instead of spilling, when its input was a join or a
  subquery: the table of distinct keys was never released, and the rows
  waiting to be grouped were applied too late to leave room. It now spills
  and completes.
- A negative `TIME(1)` or `TIME(2)` value below -625 hours with a fractional
  part was captured wrong through change capture: the binlog decoder read
  the fraction unsigned. It now reads it signed, as the server writes it.

### Performance

- A grouped `ROUND(SUM(x), 2)` over many groups parsed every rounded result
  back from text to re-render the same text, since the exact decimal
  rounding fix. That step is skipped when it cannot change the value: a
  100,000-group top-10 aggregate measures 34 ms again, from 46.

## [0.1.5-rc6] - 2026-09-24

Read parity with MySQL (character sets, temporal reading, comparison
typing), a one-table blast radius for replication failures, faster
aggregation and grouping, and a gate that runs in a fraction of the time.

### Fixed

- Base-conversion results retain their raw digits under wide connection
  encodings, including byte consumers, concatenation and case conversion.

- Binary-to-UTF-8 conversion keeps the well-formed prefix before malformed
  bytes, including an empty result when the first byte is invalid.

- `CONCAT` and `CONCAT_WS` retain declared BIT-column byte widths through
  direct and derived projections instead of formatting the numeric carrier.

- Chained `BETWEEN` expressions bind the upper comparison using MySQL
  precedence while preserving explicit parentheses and negation.

- Calendar casts capture SQL-mode rules for zero components and invalid
  dates; accepted temporal values retain their fields in date consumers.

- Wide-character connection collations retain their encoding and comparison
  profile, including a collation assignment following `SET NAMES`.

- Scalar bitwise operators and `BIT_COUNT` preserve binary byte strings,
  fixed-width shifts and numeric hex-literal semantics.

- `utf8mb4_0900_as_cs` preserves accents and case in comparisons, grouping,
  ordering and searches, including connection collation and regex defaults.

- Binary `BIT_AND`, `BIT_OR` and `BIT_XOR` retain byte values and declared
  identities across groups, windows, spill and merges, with MySQL length
  errors. Unintroduced hex and bit literals keep numeric aggregate semantics.

- Decimal `DIV` reads internal division precision in runtime evaluation and
  constant folding; explicit text casts remain display boundaries.

- Result labels use the server metadata character repertoire independently
  of values. Connection collation changes also update the literal charset.

- `SET sql_mode = DEFAULT` restores the default modes, including zero-date
  validation, instead of storing the keyword as a mode name.

- Typed `TIME` casts to calendar types and calendar intervals anchor to
  the captured session date. Query sharing separates different dates.

- Clock `EXTRACT` fields preserve negative duration signs and fold day
  prefixes into hours, while calendar inputs retain their day component.

- Hour, minute and second intervals on typed `TIME` values preserve signed
  durations and fractional precision, returning NULL on range overflow.

- `DAYNAME` retains its weekday number in arithmetic, numeric comparisons
  and numeric aggregates while explicit casts still convert the label.

- Temporal casts and date/time consumers retain parsed partial dates and
  fractional precision. Day-only clock formats form durations; early
  `FROM_DAYS` inputs and year-zero calendar labels match MySQL.

- `STR_TO_DATE` handles partial input, fractions, ordinal dates, week years
  and character-class directives, with captured zero-date modes and correct
  date/time result types for literal and dynamic formats.

- `SET timestamp` evaluates numeric SQL expressions in the current
  session before capturing the statement clock.

- Qualified identifier result labels stop at the identifier under
  `IGNORE_SPACE`, preserving the distinct unqualified-label behavior.

- Decimal `ROUND` and `TRUNCATE` retain exact arithmetic and the input
  scale when their precision argument is computed at runtime.

- Mixed binary/text `IF`, `CASE` and `COALESCE` branches retain byte
  comparison semantics and the selected text branch’s encoding.

- Unicode string search retains accents, matches complete case-folded
  characters, and returns positions in the original subject.

- String search and `FIELD` capture the subject collation; binary patterns
  retain text-subject search and padding semantics.

- Empty-needle search boundaries, trailing escapes in `LIKE`, and signed
  and unsigned overflow limits in `CONV`.

- FLOAT casts preserve single-precision values, decimal guard digits survive
  floating casts, and FLOAT text/binary results retain their distinct
  precision. String and temporal consumers honor FLOAT formatting.

- Session-fixed statement clocks and TIME-to-YEAR casts, including year
  boundaries across time zones. Temporal casts accept omitted seconds,
  enforce YEAR's range, and read JSON values and compact times correctly.

- UCS-2, UTF-16 and UTF-32 expression encoding for introducers, `CONVERT`,
  connection-generated strings and byte-reading functions, with encoding
  captured in query plans and isolated cached answers.

- Unaliased result labels preserve regular-expression source text, omit
  trailing comments, retain `IGNORE_SPACE` whitespace, and honor the
  255-byte name limit. Prefixed adjacent strings concatenate correctly.

- Session default week modes in `WEEK` and `EXTRACT(WEEK ...)`, with
  explicit modes and `YEARWEEK` retaining their independent behavior.

- Session calendar locales for `DAYNAME`, `MONTHNAME` and `DATE_FORMAT`,
  including locale-specific abbreviations and isolated cached answers.

- String replacement and insertion boundaries, and binary-preserving
  `LEFT`, `RIGHT`, `REPLACE` and string `INSERT` results.
- `GROUP_CONCAT` argument-position ordering and numeric DISTINCT ordering.
- Compact runtime TIME casts and precision clamping when casting TIME
  columns to DECIMAL.
- Session time zones and fractional seconds in Unix timestamp conversions,
  including negative inputs and upper-range rounding.

- A source ALTER that had not streamed yet could leave a table's store
  refusing to open with a schema fingerprint mismatch. A table never altered
  took its shape from the stored probe, which a re-probe, forced snapshot or
  sibling resync rewrites with the source's current columns. Its first
  shape is now recorded in schema history when its store first opens.
- One table's storage refusing its rows, a TRUNCATE its store could not
  take, or a failed first copy of a newly created table stopped replication
  for every table and failed the same way each cycle. That table is now
  quarantined for the automatic resync while the rest keep streaming.
- A grouped `SUM`, `AVG`, `MIN` or `MAX` over a projected decimal column
  holding a CASE with an integer branch (`CASE ... THEN price ELSE 0 END`,
  through a derived table or view) answered NULL for every group.
- `GROUP BY` on several keys, one of them under a binary collation, merged
  keys that differ only in letter case.
- `SELECT 1, @@version` - a connection variable beside any other item -
  returned the variable's column alone.
- `CAST(x AS SIGNED)` and `AS UNSIGNED` truncated a fractional number where
  MySQL rounds it (`CAST(1.5 AS SIGNED)` is 2), and refused an operand that
  did not fit where MySQL saturates it.
- Zero-part dates read from numbers and from text with any punctuation
  (`CAST(0 AS DATE)`, `CAST('12:00:00-12.34.56' AS DATETIME)`) answered NULL
  even where the session allows zero parts; they now follow the session's
  `NO_ZERO_DATE`, `NO_ZERO_IN_DATE` and `ALLOW_INVALID_DATES`.
- `EXTRACT(HOUR FROM '1-2-3')` read a short-part date as a duration.
- An `IN` list mixing strings and numbers, or a `DATETIME` among `TIME`
  items, is compared item by item as MySQL does, instead of in one type for
  the whole list.
- A table-free subquery reading a column two levels out, such as
  `HAVING (SELECT c)` under an outer `GROUP BY c`, failed to bind.
- A UTF-8 introducer over bytes that are not UTF-8 is read lossily rather
  than refused.
- An ordered `SELECT @v := ...` assigns the variable from the rows it sends.


### Added

- `default_week_format` is honored: a one-argument `WEEK()` uses the
  session's mode, 0 to 7.
- `lc_time_names` accepts MySQL's locales for day and month names.

### Performance

- Aggregates over a computed argument (`SUM(CASE ...)`, `SUM(a * b)`,
  `AVG(x + 1)`) project the argument a batch at a time and take the parallel
  aggregation paths: up to three times faster over 2M rows.
- `GROUP BY` on a text key or several keys found each new group by scanning
  every group so far, which was quadratic in the group count. A collation-
  keyed index answers it with one lookup.
- A filtered scan whose row ranges are exact no longer evaluates its
  predicates a second time above the scan: a selective text-range filter is
  about 18% faster.

### Changed

- MTR replay artifacts are retained per invocation with source and binary
  provenance, statement identities, and bounded diagnostic samples.

### Verification

- The MTR harness keeps the better of a run and its replay, and the MySQL
  and MariaDB suites no longer overwrite each other's diffs.
- A nightly workflow replays the MTR suites, replica mode included, which
  no gate ran before.
- The rc gate generates the dashboard once and builds under one setting, so
  stages no longer rebuild and relink the binary between them.
- The rc gate runs in about 13 minutes instead of about 43: the unit stage
  runs in parallel (timing-sensitive tests one at a time, with retries), each
  crate's integration tests build as one binary, MTR files replay eight at a
  time with the two suites side by side, and a second Docker host takes the
  mtr, migrations and MySQL 8.0 stages once the oracle has passed.
- The MTR oracle keeps every table generation a file creates (6 GB of tmpfs,
  was 2), follows a file's global `sql_mode` onto Pintail's session, and
  restores its server state only after the files that change it.

## [0.1.5-rc5] - 2026-09-24

Fixes from reviewing the verification program, and one replication stall.
Twenty-three commits since rc4.

### Fixed

- One table whose store could not open stopped replication for the whole
  database. A store written for another shape of its table - same schema
  version, different fingerprint - failed every cycle, and the failure was
  written onto every table, so all of them showed the same fingerprint error
  and none of them streamed. That table is now quarantined on its own, the
  rest keep streaming, and the automatic resync recopies it.
- Change capture stopped for good when every tracked table had been dropped
  and re-created; it now keeps running and reads the CREATE events that
  replace the retained rows.
- A table whose name differs only in case from another source table is
  quarantined, not just skipped, so it cannot rejoin the stream later with
  rows missing from the gap.
- A keyless table is no longer copied without the global read lock. Without
  it, events between the captured position and the copy were replayed on
  top of the copy, and a keyless table has no key to absorb the duplicates.
- A crash between a transaction's rows and its commit record could leave a
  table that refused every later open. Recovery now frees the sequences of
  the transaction that never committed.
- `AVG` and division honour the session's `div_precision_increment` in the
  cached aggregate and in the column metadata a driver reads; both assumed
  four digits.
- `CAST('101112' AS TIME)` and `TIME('101112')` read the digits as packed
  `HHMMSS` instead of a date; `DIV` with an unsigned operand and a
  non-integer one no longer overflows; a hex literal converts to a number
  correctly.
- `SET @v = ...` keeps backslash escapes and `:=` inside literals, and a
  `SET` list applies every assignment.
- A local `CREATE TABLE` reads unquoted `ENUM`/`SET` numbers as positions,
  and three other definition details from the parsed statement rather than
  its text.
- `CAST(x AS BINARY(n))` wider than `max_allowed_packet` answers `NULL` with
  warning 1301 instead of allocating the declared width for every row.
- Parallel execution workers get the same 8 MiB stack as the calling thread,
  so how deep a query may recurse no longer depends on which thread ran it.
- A connection the disconnect watch closes now logs which of EOF, a
  readiness failure or a read error it saw.

### Verification

- The MySQL and MariaDB suite replay reads the suites' SQL correctly,
  forwards their session statements, refuses a baseline banked against a
  different oracle, and lets a partial run speak only for the files it ran.
- The CDC matrix runs every leg it claims; the farm masks varying numbers
  rather than identifying ones; the preflight sweep only removes abandoned
  containers.
- The oracle no longer compares a spelling that a case-insensitive `UNION`
  or `INTERSECT` may legitimately return either way.

## [0.1.5-rc4] - 2026-09-13

Verification. MySQL's and MariaDB's own regression suites now run as a gate
stage, and most of this release is what replaying them found. Seventy-nine
commits since rc3.

### Added

- MySQL's `mysql-test` suite, and MariaDB's, replayed statement by statement
  against Pintail and a live MySQL 8.4 as the new `mtr` gate stage - 633
  files, 23,118 SELECTs, compared byte for byte. The suites are fetched from
  upstream at a pinned commit rather than vendored, so no upstream test file
  enters this repository. A second mode replays the same statements through
  change capture: every `INSERT`, `UPDATE` and `ALTER` runs on a
  binlog-enabled source and each SELECT is compared once the mirror catches
  up, which turns MySQL's own suite into a replication test.
- A bug atlas drawn from the fix history of MySQL, MariaDB and ClickHouse,
  ranked by bug class, and generated gates named per class.
- A seeded simulation of change capture checked against a reference model,
  a live matrix across MySQL 8.4/8.0/5.7 and MariaDB 11.4/10.6, disk-fault
  injection for the store, and randomized checks of the vectorized kernels
  against row-by-row evaluation.
- Session variables a client sets on connect: `sql_select_limit`,
  `div_precision_increment`, user variables, and `SET time_zone` back to the
  global zone. Local tables accept hexadecimal literals, `TRUE`/`FALSE`, and
  a column's literal default.
- Observability: `PINTAIL_QUERY_TRACE_JSON` writes the per-statement phases
  as a Chrome trace for `ui.perfetto.dev`; a `console` feature serves the
  tokio task view for `tokio-console`; and every wire connection now records
  why it closed.

### Fixed

- Wrong answers the replay found: temporals read as numbers, dates written
  packed or punctuated and two-digit years, exact integer casts, `TRUNCATE`
  and `DIV` on large integers, `BINARY(n)`, `GREATEST`/`LEAST` across mixed
  types, text beyond the `TIME` range, character-set introducers such as
  `_ucs2 X'0420'`, legacy utf8 comparison, a negated `DECIMAL` zero printed
  with a sign, and `<=>`, simple `CASE` and row `IN` typed as `=` types them.
- Local tables stored `ENUM`, `SET`, `JSON` and `DECIMAL` values as written
  rather than as MySQL stores them, and gave text the wrong collation.
- Change capture no longer freezes: a copy and a row count each bound their
  wait for a table another session has locked, the global read lock no longer
  stalls the source, a binlog packet larger than `max_allowed_packet` is
  read, and two source tables whose names differ only in case no longer stop
  the database replicating.
- The store checksums its segment header and column descriptors, and refuses
  a table whose manifest is lost after a flush.
- A join frees its build table when its probe is exhausted rather than when
  the query is dropped, so an aggregate or sort above it is not charged for a
  hash table nobody can read.
- The wire idle deadline applies to waiting for a command, not to running
  one, so a long query is no longer disconnected without an error packet.

### Changed

- The `rc` profile runs twelve stages and takes about forty minutes; `mtr` is
  twenty of that. A release candidate claims correctness, and this is the
  stage most likely to find a regression, because nobody here wrote it.
- The differential oracle is 1,907 cases, with ten multi-table join
  topologies added - three- and four-table chains, a bushy join, an outer
  join above an inner one, and the `RIGHT JOIN` after an inner join whose
  `ON` carries a subquery.
- Segment format version 4 adds a descriptor digest. Readers accept every
  published version.

## [0.1.5-rc3] - 2026-09-13

The columnar execution program, and the schema-migration decisions that go
with it. Two hundred and sixty commits since rc2.

### Added

- `DECIMAL` results compute to 65 digits, MySQL's own limit, instead of
  overflowing the 128-bit intermediate a narrower engine can hold.
- Cross and theta joins plan whatever their estimated size, rather than
  refusing above an estimate. **Breaking**: a query that was refused for its
  estimate now runs, and a genuinely large one will take the time it takes.
- `NO_UNSIGNED_SUBTRACTION` is implemented, so a subtraction below zero on
  unsigned operands answers as MySQL does under that mode.
- A session starts in the source's global time zone, so an unqualified
  `TIMESTAMP` reads the way it reads on the source.
- Statement tracing records where each statement's time goes, and the store
  publishes a generation after every change to a table's files, which is what
  lets a reader prove a replica current without asking.
- A result is written to the client while it is produced, rather than after
  it is complete.

### Performance

The theme of this release: expressions and operators work a batch at a time
over packed columns, instead of a value at a time through `Value`.

- The expression tree has batch kernels for comparison, exact arithmetic and
  NULL tests; `NOT` and unary minus and plus; `IF`, `CASE`, `COALESCE` and
  `NULLIF`; scalar functions generally; date parts and interval arithmetic;
  `DATE`, `LAST_DAY`, `DATEDIFF` and `TIMESTAMPDIFF`; and `UPPER`, `LOWER`,
  `LENGTH`, `CHAR_LENGTH` and `LIKE` read in place. A kernel can take another
  kernel's answer as its argument, so a nested expression stays packed
  throughout.
- Those kernels are now reachable from a `WHERE` clause, a join residual, a
  disjunction, and integer and temporal `IN` lists - the tree existed before
  this and the filter could not reach it, which is where most of the gap was.
- Decimals and temporals compare and format from their packed units rather
  than round-tripping through text, including `DATE_FORMAT`, `TIME`
  comparison numbers, and rounding to a place left of the point.
- Sorting orders row references over the input's own batches; a top-k keeps
  its rows as columns; window functions evaluate over batches in about one
  pass per partition; a hash join probes a batch at a time and keeps its
  build rows in their batches. A text sort key is prepared once per row, and
  a text join key is held as its collation weight bytes.
- Access paths: scans and joins pinned to a few keys are read by key, a
  key-ordered join stops at its limit, a sort the scan's key order already
  satisfies is left out, `TIMESTAMP` filters prune in a fixed-offset session,
  and a derived table's unread columns are dropped.
- The wire path streams a large result as execution produces it, encodes on
  the worker that ran the statement, keeps rows as batches and encodes
  straight from them, and prepares each statement once for both admission and
  execution.

### Fixed

- Decide whether a schema change can be adopted onto a running mirror from the
  source's own column declaration rather than from Pintail's mapped type. An
  `ALTER TABLE` reaches the replica as one statement and no row events, so a
  migration that rewrites the rows the source already holds leaves the replica
  holding the pre-`ALTER` values with nothing later to correct them - and a
  whole class of those migrations keeps the mapped type identical while doing
  it. A narrowing integer, a shrinking `VARCHAR`/`CHAR`/`TEXT`/`VARBINARY`, a
  narrowing `BIT`, a `VARCHAR` or `VARBINARY` becoming the fixed-width `CHAR`
  or `BINARY`, a `FLOAT` and a `DOUBLE` exchanged either way, `DATETIME` becoming
  `TIMESTAMP` (which zeroes every value outside the epoch window), a dropped
  or renamed `ENUM` member, a reordered `SET`, a tightened nullability and a
  rewritten generated expression now mark the table `needs_resync` instead of
  evolving in place; the resync recopies the rewritten values. Reordering an
  `ENUM`, appending to an `ENUM` or `SET`, widening a string or its character
  set, changing a collation, and the existing integer and decimal widenings
  still evolve in place.
- A declaration this version cannot read is refused rather than adopted. An
  `ENUM` or `SET` whose member list will not parse, a type respelled under the
  same data type, and a generated expression recorded before expressions were
  read each used to return "nothing to report", which the caller read as safe.
- `ORDER BY ... LIMIT` answered a different set of rows between runs of the
  same query over the same data, because the row top-k chose among tied rows
  with an unstable selection and no tiebreak.
- A `RIGHT JOIN` following an inner join kept a subquery in its `ON`, where it
  previously answered too few rows, and an `EXISTS` decorrelates correctly
  when an outer relation shares its alias.
- Decimal semantics: scale widens in place for a text-stored column, an `IF`
  or `CASE` branch renders at its own scale, a nested expression that outgrows
  `i128` widens, and a wide total stays out of every aggregate state.
- MySQL fidelity: `DOUBLE` prints as MySQL prints it, literals collate under
  the connection's collation, trailing no-break spaces group apart under
  `unicode_ci`, prepared-statement results are described as MySQL describes
  them, the diagnostics area survives for `SHOW WARNINGS`, and multi-row
  subqueries and bad JSON paths answer MySQL's error codes.
- Resource and robustness: a join hands back its reservations when the plan
  stops early, carries a cancellation out instead of dropping it, refuses a
  residual batch before building it, and partitions a build that leaves no
  room to continue; a sort charges its prepared text keys before building
  them; one unreadable table no longer reloads a whole replica or makes a
  database unreadable; and CDC advances past a DDL this engine cannot parse.

### Tests

- A schema-migration differential gate (`tests/e2e/migrations.ts`, banked to
  `tests/e2e/results-migrations.md`) runs migration families against a
  live mirror and asks three questions of each: do the rows nobody wrote to
  after the migration still match the source, do the writes that follow it
  land, and does the table still match after a restart. Checking only the rows
  written after a migration passes every one of these cases while the table is
  wrong. `docs/schema-migrations.md` records what MySQL 8.4 was measured to do
  to stored values in each family. The stage runs in the `rc` profile.
- The differential oracle corpus grew to 1,907 cases, all byte-exact against
  MySQL 8.4, with ten multi-table join topologies added: three- and four-table
  chains, a bushy join, an outer join above an inner one, and the
  `RIGHT JOIN`-after-an-inner-join shape that was answering too few rows.

## [0.1.5-rc2] - 2026-09-10

A TPC-H-derived Q05 join plan fifty times faster, storage scans that reuse
what they already decoded, and a MySQL differential oracle of 1,895 cases
that now gates, with the string, binary, TIME, ENUM and decimal fixes it
drove.

### Performance

- Avoid dimension fanout in cyclic inner joins, propagate complete integer join
  membership, fold literal date intervals into scan bounds, and copy only needed
  packed scalar payloads. With settled-result memoization disabled, the unchanged
  synthetic SF1 Q05 query improved from a 47.96-second median to 0.91 seconds
  (0.99-second p95 over fifteen runs), with exact MySQL answers and no spill.
  The candidate uses a 4 GiB query-memory ceiling and default spill limits;
  the baseline needed a larger spill allowance to finish. This is a measured
  workload result, not a one-second guarantee for arbitrary joins. A paired
  20M shared-host benchmark had one query median 8.2% slower; regression-free
  performance is not established.

- Cache immutable block offsets and retain decoded predicate columns when the
  output projection is identical. Dense text-predicate scans with that
  projection decode half as many blocks. Idle-host 20-million-row probes found
  1.26–1.51× SQL improvements on scan-bound shapes, with no material gain for wide aggregates. The
  benefit is workload-specific.

### Fixed

- `EXISTS` and `NOT EXISTS` over an ungrouped aggregate subquery - `EXISTS
  (SELECT COUNT(*) FROM ... WHERE ...)` - held only where input rows
  existed; the aggregate yields its one row regardless, as MySQL answers.
- Two temporal operands of different types compared their texts: a DATE
  never equalled the DATETIME at its midnight, nor a DATETIME the same
  instant at another fractional precision. They now compare as instants in
  comparisons, `<=>`, IN, BETWEEN, row IN and join keys, and `<=>` reads a
  temporal literal as the other comparisons do.
- BETWEEN with a NULL bound answered NULL where its other comparison
  decides: `1 BETWEEN 2 AND NULL` is false, and `NOT BETWEEN ... AND NULL`
  keeps the rows below the lower bound.
- A DECIMAL compared with text compares as a double, the text read by its
  numeric prefix or exponent: `total > '100.5x'` and `> '2e1'` compared
  strings.
- JSON integers past 2^53 no longer collide in comparison, grouping and
  DISTINCT keys; `1` still equals `1.0`.
- `x op ANY` and `x op ALL` over text values take their extremes as numbers
  when `x` is a number, as MySQL does over a table column.
- An exact number compared with a string compares as a double however the
  string is spelled, so `'9007199254740992'` and `'9007199254740992x'` agree.
- IN and BETWEEN compare their whole list under one type: a DECIMAL sharing
  it with text or a float compares as a double, where DECIMAL members had
  met each other as text; a text subject meets DECIMAL bounds the same way.
- A DATE member of an `IN (SELECT ...)` or `= ANY` list of DATETIME values,
  decorrelated or not, matches the instant at its midnight.
- SUBSTRING with a negative start reaching past the first character answers
  an empty string instead of the whole string, and LPAD or RPAD that must
  pad with an empty pad string answers an empty string instead of NULL.
- Binary strings go through SUBSTRING, TRIM, LPAD, RPAD, CONCAT_WS and LIKE
  byte by byte, and return bytes, where bytes that were not UTF-8 raised an
  error.
- DATE_ADD and DATE_SUB keep a DATETIME's fractional seconds.
- TIME values compare as times, not as text, so `-100:00:00` sorts below
  `-00:00:01`; `TIME + 0` and numeric casts read the `HHMMSS.ffffff`
  number, and ADDTIME and SUBTIME clamp at the TIME range.
- An ENUM in a numeric context - `status = 3`, `status + 0`,
  `CAST(status AS UNSIGNED)` - reads its declaration index, not its label.
- COALESCE, IF and CASE over signed and unsigned BIGINT branches stay exact
  instead of passing through a double, and JSON_LENGTH is a signed integer,
  so `COALESCE(JSON_LENGTH(doc), -1)` stays an integer.
- JSON_TYPE names a non-negative integer past 2^32 - 1 `UNSIGNED INTEGER`.
- Equality between DECIMALs whose common type would need more than 38
  digits compares by value instead of failing with a numeric overflow.
- `x op ALL` and `x op ANY` accept a UNION subquery.
- A query with a window function may sort by a column it does not select.

### Tests

- The MySQL differential oracle grew to 1,895 cases: typed result
  comparison, a boundary fixture across integer, decimal, float, string,
  binary and temporal limits, reviewed and seed-minimized regressions, and
  a replay across storage layouts. Cases that diverge through a documented
  limitation sit on a reviewed known-failure ledger that warns while they
  fail and fails the run once they pass.

## [0.1.5-rc1] - 2026-09-10

Outer-join ON subqueries answered in every shape, WHERE clauses matched to
MySQL at their edges, every row of a large result returned over the wire,
and range filters that keep pruning on a table taking updates.

### Added

- `IS [NOT] TRUE`, `IS [NOT] FALSE` and `IS [NOT] UNKNOWN`; row-constructor
  comparisons (`(a, b) = (1, 2)`, and `<>`, `<`, `<=`, `>`, `>=` decided by
  the first pair that differs); `x op ANY (subquery)`, `SOME` and `ALL`,
  answered from the subquery's count, non-NULL count and extremes so
  three-valued logic and an empty subquery come out as `MySQL` has them;
  and a JSON value compared with a number, which compares as JSON.

- A range filter keeps pruning segments after a table takes updates. Value
  pruning let a segment go only when it overlapped no other segment at all,
  so the first flush of updated rows over a table's base switched pruning
  off for every base segment, and a filter on a timestamp column read the
  whole table - on a replicated table taking updates, always. A segment now
  prunes when every segment overlapping it is newer: each row it holds is
  current and fails the filter, or stale behind a newer version that
  decides for itself. A segment overlapping an older one is still read, so
  no stale version comes back.

- A correlated `IN` or `EXISTS` in a LEFT or RIGHT join's ON condition that
  reaches the join's preserved side is answered instead of refused. The
  subquery's rows do not depend on the outer row - only which of them a
  row asks for does - so the joined side is widened by the subquery's
  DISTINCT rows on the equality it names, and the correlation becomes one
  more join key. DISTINCT keeps the answer exact: a joined row meets at most
  one subquery row per preserved row, so no match is duplicated and an
  unmatched row stays null-extended. It runs as hash joins with no per-row
  subquery executions.
- The shapes that widening does not take - `NOT IN`, `NOT EXISTS`,
  inequality correlations, subqueries with their own joins or grouping -
  are answered on the dependent join path instead of refused. Each
  candidate pair of rows resolves the subquery with both rows in scope,
  memoized per distinct correlation value, and the ON condition's plain
  equalities bucket the candidates: a left row meets only the right rows
  its keys reach, where the path used to test every pair.

### Fixed

- A DECIMAL compared with a string literal or inside BETWEEN was compared
  as its text carrier: `balance > '100.5'` let `99.00` through, and
  `BETWEEN -500 AND -0.01` sorted -12.50 below -500. A plain-number string
  literal is read as a number against an exact number, and a DECIMAL BETWEEN
  is bound as the two exact comparisons MySQL defines it to be, so every
  execution path compares by value.
- `NULL NOT IN` an empty list is true and `NULL IN` one false; the list
  evaluator answered NULL, so an outer join scoped by a membership subquery
  dropped rows whose list came back empty.
- Result metadata follows MySQL 8.4: `TIMESTAMP_FLAG` only on a TIMESTAMP
  that initializes or updates itself, and an integer of up to eleven digits
  declared INT when a grouping or a materialized derived table stores it,
  while wider integers and merged derived tables keep their types.
- A MySQL-wire result stopped at 10,000 rows without an error. The client
  protocol has no way to say a result was cut, so a report over 43,000 rows
  arrived as a complete-looking 10,000. The wire now returns every row,
  bounded by the per-query memory ceiling as before, and
  `PINTAIL_MAX_RESULT_ROWS` sets an optional row ceiling that refuses a
  larger result instead of truncating it. The HTTP query API keeps its
  preview cap, which it reports as truncated.
- A DATETIME or DATE compared with a literal written any way but its
  canonical text answered wrongly, silently: `created_at = '2024-03-01'`
  matched nothing, a date-only upper bound in `BETWEEN` or `IN` dropped the
  rows at that midnight, `> 20240301` and `>= '2024-3-1'` compared as
  strings, and `'...56.000'` missed its own value. The literal is now read
  as `MySQL` reads it and rewritten into the column's canonical form before
  comparing; an impossible date like `'2024-02-30'` is refused, as `MySQL`
  refuses it.
- `NOT EXISTS` or `IN` decorrelated into a semi- or anti-join left the
  inner table's columns visible to the outer query, so an outer column the
  inner table shared by name - `SELECT id ... WHERE NOT EXISTS (SELECT 1
  FROM users u ...)` - was reported ambiguous.
- MySQL 8.4 with default optimizer switches drops a subquery's own filters
  when it materializes a correlated `IN` or `EXISTS` from an outer join's ON
  condition, so it matches rows the subquery excludes. The refusal shipped
  in 0.1.3-rc1 was built on that answer: the differential suite compared
  against it and read Pintail's correct count as too low. Pintail's answer
  is what MySQL itself returns with `semijoin=off`, which is now the
  reference the suites compare these shapes against.

### Fixed

- Browser exceptions and Nuxt errors are reported to the configured Sentry
  project with deployment release tags. Reporting excludes component props,
  request details and navigation breadcrumbs; source maps can be uploaded
  during the dashboard build.
- The SQL console renders 100 result rows per page instead of mounting the
  entire result set. Large results no longer create thousands of offscreen
  components, and changing pages keeps every returned row accessible.
- Chart tooltip HTML rendering releases detached Vue components and no
  longer caches an unlimited history of payloads.

## [0.1.4] - 2026-09-10

Everything in 0.1.3-rc1, gated with the full stable chain, plus the two
replication fixes below.

### Fixed

- A source's double-quoted identifiers are read as identifiers. A source
  running with `ANSI_QUOTES` writes them that way, and the DDL lexer read
  them as string literals, so an ordinary `CREATE TABLE` did not parse. The
  source's SQL mode does not travel with the statement, so it is parsed as
  written and, on failure, parsed again with double quotes delimiting
  identifiers. Nothing loses a valid reading to that retry: the first parse
  rejects a double-quoted token outright, so any statement carrying one has
  already failed by the time the retry runs.

- A DDL nobody can parse no longer stops a database replicating. The
  replication pass returned on an unreadable statement and the next pass
  resumed at the same offset and failed identically, so one such statement
  froze every table in that database at one binlog position indefinitely.
  The tables the statement NAMES are now quarantined for resync and the
  stream moves on: a DDL that alters a table cannot avoid naming it, so
  nothing that changed is missed, while a name appearing in a comment costs
  a resync rather than a wrong answer.

- The dashboard reports browser errors through its runtime Sentry
  configuration, so a failure in the page reaches the same place a failure
  in the server does.

- The dashboard bounds how much of a SQL result it renders, and releases
  its chart tooltip components, so a large answer or a long session no
  longer grows the page without limit.

## [0.1.3-rc1] - 2026-09-10

### Fixed

- A subquery in an OUTER join's ON condition correlated to the join's LEFT
  side is refused rather than answered. Dependent resolution exists only at
  Filter level, so in a join condition the subquery ran without the outer
  context it needs and the join matched too few rows: measured against
  MySQL 8.4, three matches reported as one and two reported as none, with
  no error to notice. Refusing is not the fix - the rewrite that would lift
  the restriction has to widen the join's right input, because a semi-join
  under it cannot see the left side - but a wrong count is worse than a
  rejection. Correlating to the join's RIGHT side alone answers correctly
  and is unaffected, as is an uncorrelated subquery.

- A failed query no longer holds a worker for as long as the client allows.
  The refused shape above resolved once per correlation value on a single
  thread, so a report that carried it consumed a core until the client's
  deadline elapsed; under several concurrent readers that starves the
  queries that would have answered.

- The resumed replication position is reported when something about it
  changes - a new binlog file, a different target count, a table newly
  blocked or paused - rather than on every supervised pass. A pass is
  non-blocking and runs every few seconds per database, so the line printed
  hundreds of times an hour and buried the log it was meant to clarify.

- Telemetry reports the release a deployment is running. Every event
  carried the workspace crate version, which does not track the released
  version, so an issue named a build nobody deployed. A deployment's
  PINTAIL_BUILD_VERSION is used when PINTAIL_RELEASE is unset.

- A deployment can set its spill limits. The engine reads a per-query and a
  process-wide spill quota and the compose file named neither, so setting
  them had no effect. A query that exceeds its quota fails rather than
  falling back to a slower plan, and the per-query default of one gibibyte
  is small for a multi-table join.

### Performance

- A correlated `IN` or `EXISTS` in an INNER join's ON condition decorrelates
  into a semi-join. An INNER join's ON filters the rows WHERE filters, so
  the same predicate asked the same question from either place, but only
  WHERE reached the rewrites: the ON placement resolved once per distinct
  correlation value instead. Measured on a three-table fixture, that
  placement went from one inner execution per correlation value to none,
  answering identically.

## [0.1.2] - 2026-09-09

The release candidate's contents plus the entries below, gated with the
full stable chain: the rc gates, the bench family, TPC-H and acceptance.

### Added

- `utf8mb4_unicode_ci` is compared, grouped, ordered and joined rather
  than refused. A schema created before `MySQL` 8 names this collation
  explicitly, and every collation-sensitive operation on a column
  carrying it used to fail to bind, so a report reading one returned an
  error instead of rows. The weights are generated from a real `MySQL`
  rather than transcribed: a character weighs a sequence, an ignorable
  mark weighs nothing, an expansion weighs several, and whole blocks
  derive their weights from the code point by UCA's rule. `utf8mb3_
  unicode_ci` resolves to the same profile; `utf8mb4_unicode_520_ci` is
  UCA 5.2.0 and still rejects.

### Fixed

- Both PAD SPACE comparators padded where they had trimmed. Trailing
  spaces are insignificant either way, but `MySQL` pads the shorter
  operand, so `'a'` is GREATER than `'a<tab>'` - the pad puts a space
  against the tab and a space outweighs it - where trimming made `'a'` a
  prefix and smaller. Every string ending below the space weight ordered
  wrongly under `utf8mb4_general_ci` and `utf8mb4_bin`.

- A failed query records why it failed. The wire logged the word "error"
  and a statement shape; the reason went to the client and nowhere else,
  and telemetry forwards only error-level events, so a query failing
  against a deployment raised nothing an operator could see. It now logs
  at error level with the reason, without the elapsed time that would
  file every repeat of one broken query as a new issue.

- The fuzz lockfile is refreshed and the parser corpus is gated.

## [0.1.2-rc12] - 2026-09-09

### Performance

- A snapshot's copy workers run as tasks rather than sharing one, and a
  composite-key page seeks by an expanded prefix comparison instead of a
  row-value tuple. The tuple form made the source scan its index from the
  beginning on every page, so successive pages re-read rows already copied
  and the cost grew with the square of the page count; the expanded form
  reads only the page. Measured on one million synthetic rows over a
  loopback source: four tables copied 2.90x faster, and a composite-key
  copy 3.48x faster at the page size that forces a hundred pages. The
  arms are independent and do not multiply.

- A grouped aggregate folds one segment at a time and keeps the folds of
  segments the memtable has not touched, so a dashboard query re-reads the
  fraction of a table that changed rather than all of it. Each span's
  chunks aggregate in parallel.

- Small responses no longer wait on a delayed acknowledgement. A result
  ending in a short packet - an OK terminator, a one-row answer - sat in
  the send buffer until an earlier segment was acknowledged, inflating the
  round trip by an interval the query never spent working.

- Two overlapping segments are now enough to plan a compaction. The
  planner returned before overlap was ever considered unless the table
  held at least the fan-in's worth of segments, four by default, so a
  table that had flushed once - one base and one small tail covering the
  rows that changed - stayed on the merging scan path until two more
  flushes arrived, however often it was read. Measured on two million rows
  with one percent changed: the scan went from 1430 ms to 19 ms, and the
  rewrite that bought that repays after 1.1 scans. The size tier still
  refuses to rewrite a base for a tail a hundredth its size when the only
  prize is fewer files; overlap is admitted because the prize is the scan.

### Fixed

- Decimal `AVG` retains its quotient behind its declared-scale display,
  so enclosing rounding functions and decimal casts no longer round an
  already-rounded average. Spill and batch boundaries preserve the extra
  precision without exposing extra digits to clients. The deterministic
  regression, full RC profile, and three e2e passes on each MySQL version
  pass, closing G14.

- One path by which `AVG` over a `DECIMAL` column could answer a unit in
  the last place away from `MySQL` is closed. The two-pass lane is chosen
  from the batch column's storage type, and the arm for a `Float64` column
  returned the float accumulator without asking whether the planner had
  typed the aggregate as an exact decimal - the arm beside it, for a
  decimal column, does ask. Since `f64` addition is not associative, an
  average that fell through moved with how the rows were split across
  workers. A branch predating the fix failed the same check in all twelve
  end-to-end phases and passed once the fix was merged in, with nothing
  else changed.

- A segment writer that cannot store a value as the fixed-width units it
  chose for that column refuses the segment, naming the column, instead of
  asserting inside the thread that asked for it. The assertion fired once
  during an acceptance snapshot and aborted the snapshot worker with
  nothing to say about which column or value was involved; it did not
  recur in twenty-seven further runs over the same data. The writer probes
  every value of a column immediately before writing it, so a value that
  fails here contradicts a check that just passed - a segment to refuse,
  and a report to act on rather than a crash to reconstruct.

- Settled aggregate and segment-fold caches now include each table
  opening's identity. Dropping and recreating a table at the same path
  can no longer reuse cached results from its predecessor.

- A grouped aggregate no longer drops a segment's rows when the fold
  cannot open a ranged read over it. The declined read was treated as an
  empty segment, so its rows stopped being counted and a `GROUP BY` came
  back missing whole groups, with no error to notice. A span that cannot
  be read this way now abandons the fold, and the general path reads the
  table.

- A predicate naming one alias of a self-joined table now reaches that
  alias's scan. Two instances of one table answered to the same key, so
  the planner could not attribute a single-relation predicate to either
  and left it above the join - reading the filtered side whole and
  building the hash table from every row rather than the matching ones,
  which is how a join that fits comfortably otherwise exhausts a query's
  memory ceiling. The rule that carries a constant across a join equality
  was blocked for the same reason and now applies.

- A short query proves its replica current before answering, restoring a
  revalidation that had been removed.

- The binlog decoder is pinned to a fork rather than vendored, and no
  longer panics on a transaction payload event whose field identifier or
  compression type falls outside the values it knows.

- The release image stops copying a vendor directory that no longer
  exists.

## [0.1.2-rc11] - 2026-09-07

### Known issues

- `AVG` over a `DECIMAL` column can answer one unit in the last place away
  from `MySQL`, rarely and not repeatably; `SUM(...) / COUNT(*)` over the
  same rows stays exact. Seen twice in gate runs and not reproducible on
  demand, so this release ships with it open rather than claiming a fix.
  `docs/limitations.md` records what is and is not known about it.

### Performance

- The overlay picks its superseded-row mask by a threshold that now comes
  from the algorithm the engine runs. The threshold had been read off a
  measurement of a different mask - one built block by block rather than
  by looking each changed key up against the whole key column - which put
  the crossover four times further out than it is. Every table between one
  and five percent changed was taking the slower of the two paths.
- Several clients asking the same question at the same time now cost one
  execution instead of one each. The first request executes and the rest
  wait on it, then every one of them receives those rows; nothing is
  retained afterwards, so the next request executes again. Only a `SELECT`
  whose answer cannot move between two runs is offered, and it is offered
  only to requests that match on the loaded replica, the statement text,
  the row ceiling and every session setting an execution reads - so a
  commit, a local write or a schema change puts the same text on a
  different key rather than answering it from before the change. A failure
  is never shared: an error, a cancellation or a panic sends everyone
  waiting to execute for themselves. Measured on sixteen simultaneous
  copies of one grouped aggregate: 1.7x faster on an idle host and 3.6x
  on a host with four cores to share, sixteen executions becoming one in
  both. `PINTAIL_DISABLE_SHARED_QUERIES` turns it off.

- The benchmark's resource sampler now reads one long-lived `docker stats`
  stream per container instead of a fresh `docker stats --no-stream` call
  every 250 ms, which on the shared remote docker host regularly took
  longer than the query it was sampling and reported 0% CPU. The README's
  generated benchmark table now shows the memo-off "engine speed" table
  first and the memo-hit table second, so the headline comparison is the
  one where both engines execute.
- The HTTP query path no longer builds a fresh query engine and looks up
  the API key against metadata on every request: one engine is held for
  the process's life and cloned per call, and a validated API key is
  cached for 30 seconds (cleared immediately on disable or delete). Rows
  serialize straight from the engine's values into the response instead
  of through an intermediate JSON tree first. The benchmark can now time
  Pintail over its MySQL wire protocol beside the HTTP call, so the engine
  is measured the way a BI tool actually reaches it.
- A join's build side now finalizes itself into a hash-free, direct-index
  table in place when its keys are a plain integer set in a narrow range,
  instead of that table existing only inside the fused join-aggregate:
  every reader of the build side benefits, and the fused join-aggregate
  additionally resolves which output group each build row folds into once
  per distinct key rather than once per probe row.
- `COUNT(DISTINCT)` over an integer column now dedups through a bitmap
  once a group's distinct values pass a count threshold and fit a span
  cap, instead of always hashing into a set; the bitmap grows with
  headroom as the column's real range becomes apparent, the same way a
  growing `Vec` or `HashSet` amortizes its own resizing.

### Fixed

- Resuming a paused table could leave it silently stale. A replication
  cycle decides what to pass over from the paused set it read when it
  began, so a table resumed part-way through still lost the changes that
  followed, and the write that would have flagged it for a recopy found
  the table already running and did nothing. The checkpoint then advanced
  past the lost transaction and the table looked healthy. Every table a
  cycle passed changes over for is re-checked when the cycle ends, and
  one that resumed under a dropped change is flagged for the recopy.
- Automatic recovery from a purged source position no longer lifts a
  table's pause. It cleared every block to rebuild, which let the stream
  apply changes to a table an operator was holding still and then flagged
  that table for a second recopy.
- A spilling aggregate handed back memory its input scan still owned. It
  refunded everything charged since it started rather than what its own
  group map held, so the scan's retained batches were released twice and
  the query and process budgets undercounted live memory, admitting work
  past their ceilings.
- A query that spilled ordered `ENUM` values by their label instead of
  their declared position, so it answered differently from the same query
  that stayed in memory. `MySQL` orders the type by declaration, which is
  what the in-memory comparison already did; the spill format wrote an
  enum as plain text and dropped the ordinal it sorts by. Spilled records
  now carry it. This reached every spilling operator, and the window and
  collection-aggregate paths added in this release made it reachable from
  more shapes.

## [0.1.2-rc10] - 2026-09-07

### Fixed

Result metadata retains declared decimal scale, temporal precision and
unsignedness before optimization changes the execution carrier. Exact integer
rounding stays integral, and prepared result bytes follow the advertised type.
Spatial columns and text-carried temporal functions preserve their wire types;
connection settings retain the value types expected during driver startup.
Version-dependent YEAR and GROUP_CONCAT metadata follows the recorded source
version for replicas.

- Short reads on a warm replica no longer lose reserved execution capacity
  just because the whole database is large. Admission bounds the query's
  physical inputs and operators; point lookups, small filtered aggregates,
  and bounded listings can use the reserve. `--reserved-query-slots` and
  `PINTAIL_RESERVED_QUERY_SLOTS` let operators size that capacity.

- A malformed transaction-payload header could panic while replication read
  its next event. Oversized header field IDs return a decoding error with
  the event position, without advancing the checkpoint.

- The MySQL client dependency is updated to fix a race in its statement
  cache. The workspace and fuzz harness use the same client version.
### Added

Restored databases report the installed backup timestamp and `data_age_seconds`
in the API, with a restored-data age gauge in metrics. The timestamp survives
restarts; old restores with no recorded timestamp report an unknown age.

The RC validation profile runs Metabase schema sync and saved time-grain and
filter questions, plus JDBC metadata discovery and prepared-result checks.
Connection probes return all requested session variables, and complex
metadata projections use the SQL executor.

Session parsing honors `ANSI_QUOTES`, `PIPES_AS_CONCAT` and
`NO_BACKSLASH_ESCAPES`, including the mode captured by prepared statements.

- Clients can negotiate multi-statement text requests. Statements execute
  in order, with a result for each and the protocol's more-results flag
  until the last. A failure stops the batch at that statement; quoted and
  commented semicolons do not split a request.

- Compound interval literals are accepted by date arithmetic, including
  year-month and day-through-second qualifiers. Signs and omitted leading
  fields follow the source's parsing rules; malformed extra fields yield
  NULL. Time-grain queries can use these qualifiers directly.

- One table can be paused while the rest of its database keeps
  replicating: `POST /api/databases/{id}/tables/{name}/pause` and
  `/resume`, with a Pause table / Resume table action and a paused badge
  on the database page. A paused table's row events are passed over by
  the CDC stream (the position still advances) and polling leaves it
  alone. Skipped changes are not kept: resuming a table the stream
  passed changes over for flags it for a recopy, which the supervisor's
  automatic resync carries out for keyed tables; a table nothing changed
  under simply moves again. A paused table is never auto-resynced,
  cascade-reconciled or polled while paused.

### Performance

- Multi-column integer scan predicates reuse their decoded columns and
  retain only qualifying rows. Increasing integer columns decode packed
  deltas directly, avoiding temporary cells and a second conversion pass;
  adding a predicate no longer multiplies the scan's working set.

- Low-cardinality text and bounded integer GROUP BY keys fold packed
  integer sums and counts directly into worker-local dense slots. Column
  lookup and lane dispatch move out of the row loop; larger key domains
  return to the existing partitioned aggregate without changing results.

- Large `IN` subquery sets spill into query-owned membership partitions
  instead of becoming an oversized literal list. Probes preserve NULL and
  `NOT IN` outcomes, comparison collation, and exact-decimal coercions;
  sets that fit retain the memory path.

- A grace join no longer rejects a key whose build rows exceed the memory
  ceiling after repeated partitioning. It replays the build rows from disk
  for each probe and serves matches in bounded chunks, keeping unmatched
  and scalar-row decisions across the complete replay.

- A correlated subquery in a join `ON` predicate no longer requires both
  inputs and the complete output to fit in memory. The replayed side and
  accumulated output spill independently, while the left input is read in
  batches and the predicate memo yields space before starving inner queries.

- Group maps containing `GROUP_CONCAT` and `JSON_ARRAYAGG` spill their
  unfinished fragments instead of failing when the map fills the query
  ceiling. Ordered concatenation retains its element keys across runs,
  DISTINCT retains its original values, and truncation applies after merge.

- Window output larger than the query memory ceiling is sorted on disk and
  evaluated one partition at a time, then served in chunks. Independent
  window expressions retain their input row identity through each sort; a
  single partition still has to fit within the ceiling.

### Fixed

- The container image could link a stale workspace crate. Its build keeps
  incremental state in a cache that outlives the tree it was built from,
  and a source file older than that cache's artifacts read as unchanged,
  so a build of one tree after a newer one compiled against the previous
  crate. Workspace sources are stamped at build time; the dependency
  cache is unaffected.

## [0.1.2-rc9] - 2026-09-07

### Added

- `RENAME TABLE` within the mirrored schema is followed in CDC mode. The
  table's store directory and every metadata row keyed by its name (the
  table row, schema history, chunk journal, polling state and checksums,
  dead letters, sync runs, the snapshot fence) move at the binlog position,
  the stored probe report is refreshed so the replica lists the new name at
  once, the stream routes later row events under the new name to the same
  store, and nothing is recopied. A rename into another schema is treated
  as a drop. Before, a rename quarantined the table for a resync and the
  new name was copied afresh on the next probe.

### Fixed

- A grouped aggregate whose group map spilled held its merged result
  whole, so a result larger than the query's memory ceiling failed on its
  own output after spilling correctly. The merged groups are now served a
  chunk at a time, each chunk sized to a quarter of what remains, and a
  spilled result is never memoized.

## [0.1.2-rc8] - 2026-09-07

### Fixed

- The pieces of an overlay slice halved for memory share the slice's
  allowance: what the pieces already decoded retain comes off what the next
  may take, and a single block that does not fit reports the request that
  failed instead of an empty memory error.
- The insert-only aggregate delta never found its settled base entry: the
  two spelled the memo key differently, so a grouped aggregate after an
  insert-only batch always recomputed. The keys now agree.

## [0.1.2-rc7] - 2026-09-07

### Performance

- A segment the memtable overlaps is decoded directly, with the rows the
  memtable supersedes masked out by the key column and the memtable's live
  rows added, instead of merged row by row. Under live replication a table
  keeps its latest updates in the memtable for as long as the memtable takes
  to fill, and one such row used to send the whole segment through the
  row-wise merge: on a 300K-row segment with forty scattered updates, a
  count took 52 ms instead of 0.7 ms, a five-key filter 128 ms instead of
  61, a three-key lookup 93 ms instead of 36. With the overlay those read
  4 ms, 65 ms and 39 ms; with two thousand scattered updates 7 ms, 68 ms
  and 42 ms. The overlay applies to a single-column integer key when every
  memtable row in the segment's span is at least as new as the segment; a
  composite or text key, a stale replay, a partially scanned segment or a
  segment retaining versions keeps the merge. Reconciliation, which walks
  the stream by key, keeps the merge as well.

### Fixed

- A table whose copy a restart interrupted was served from its partial
  store: the interrupted copy is flagged for resync with the copy still
  owed, and the flag alone read as a whole store. The copy-pending flag now
  decides, so a table flagged for a resync it has not started (a
  quarantine) keeps serving and an interrupted copy is refused as not ready.
- A wire response body at or past the writer's 64 KiB buffer streams
  straight from the caller's slice, and a buffer a large response grew
  shrinks back, so a pooled connection no longer keeps its largest
  response's capacity.
- `PINTAIL_QUERY_QUEUE_WAIT_SECONDS` set to a finite value no duration can
  hold (1e30) is refused at configuration instead of panicking.

## [0.1.2-rc6] - 2026-09-07

### Performance

- The scan decodes direct segments in block-aligned slices of 131,072 rows,
  up to four per scan thread per round, within half of the query's
  remaining memory ceiling (one slice at a time under 64 MiB). A round
  used to take one whole segment per thread bounded only by the whole
  ceiling, so ten threads over compaction-sized segments either held most
  of the ceiling or halved their width; rows in flight are now bounded by
  width times a slice whatever the segment size. The filter-first decode
  works per slice. A 200K-group GROUP BY over ten million rows fell about
  6%; a scan-bound five-group text key costs about 10% more, the price of
  the scan holding half the ceiling instead of all of it.

### Added

- `PINTAIL_QUERY_QUEUE_WAIT_SECONDS` (also `--query-queue-wait-seconds` and
  `query.queue_wait_seconds` in the config file) sets how long a query at
  the concurrency ceiling waits for a slot before it is refused with 1040.
  The default stays at 2 seconds; fractions are accepted and zero refuses
  at once. A dashboard that fires a burst of reports can trade errors for
  latency without raising the ceiling. The startup limits line reports it.

### Performance

- Wire responses reach the socket as one buffered write per response. The
  packet writer issued two unbuffered writes per packet (header, body), so
  every row of a result set cost its own system calls and segments.
  Measured on loopback against a release build: a 200-row result 2.7×
  faster, a 100K-row result 4× faster, a one-row query about 20% faster.

### Fixed

- A table whose copy from its source has not completed is refused as not
  ready (HTTP 503, wire 1040-class unknown error naming the table) instead
  of answering from an empty or partial store. While a resnapshot ran, the
  engine served the table's rows as they arrived, so a report joining it
  returned silently short results with no error; the other tables of the
  database, and metadata queries, keep answering. A table flagged for a
  resync it has not started, a table under replication and a local table
  hold complete stores and serve as before.
- A key lookup reads the key blocks its range touches, not the whole key
  column. The row-header pass walked every block of the primary-key column
  (loading and checksumming each) to find the ones a range fell in, then
  every block of the version and tombstone columns, and reserved header
  memory for every row of the segment; on a compaction-sized segment that
  was tens of megabytes read and reserved per lookup. It now seeks by the
  footer's column directory and sparse key index straight to the touched
  run of blocks in each of the three system columns, passes over the blocks
  before it, stops after it, and reserves for the run. Merge-on-read over
  overlapping segments seeks each segment to the range's lower bound the
  same way and stops at its upper bound instead of draining every segment
  to its end.
- A segment reader skips the blocks it will not decode. The readers behind
  key lookups, ranged projections and late materialization walked every
  column of a segment through the file and loaded and checksummed each
  block payload before deciding it was not wanted, so a narrow read over a
  table with wide text or JSON columns paid for the whole segment. On a copy
  of a production-shaped table (a few hundred megabytes across two
  segments) a ten-row key lookup took 56 ms on a laptop and about half a
  second on the deployment's host, the same for any projection. Blocks a
  reader has already ruled out are now passed over with a seek after their
  length and row count are read: the same lookup takes 1.4 ms and a
  three-column filtered scan of the table fell from 57 ms to 1.5 ms. Block
  checksums still cover every payload a scan decodes.
- A warm replica stays warm across audit records, API-key touches and other
  metadata bookkeeping. The replica stamp compared the metadata store's file
  and WAL, which every authenticated request moves, so a cached replica was
  judged stale on every request and a short query eligible for reserved
  admission fell back to the general queue. The stamp's metadata half is now
  a signature of the rows a replica load reads (database, tables, schema
  history), and the reserved path's size cap counts the tables' files alone
  (issue #34).

## [0.1.2-rc5] - 2026-09-06

### Added

- Generated recovery sequences for the store: random interleavings of
  versioned writes and tombstones, flushes, compactions, reclaims,
  checkpoints, ADD COLUMN, at-least-once replays and one process abort,
  checked against an in-memory model after the crash, after replaying the
  tail into the restarted table, after the rest of the sequence and after
  a clean reopen. A failing sequence is shrunk to the shortest one that
  still fails and printed for replay. A `failpoints` build adds the same
  sequences with the abort inside a WAL write.

- `EXPLAIN ANALYZE` prints a per-operator profile after the plan: each
  plan node's total and self time, time to first batch, batches, rows and
  peak query reservation. `PINTAIL_PROFILE=1`, a development switch, logs
  the same block for every query the server runs.
- The `compose` validation stage runs one functional check inside the
  shipped `docker-compose.yml` on the docker host: the image built from
  the tree, the stack up through the compose file, the startup limits
  line checked for the descriptor limit and the concurrency the
  environment asked for, and a spilling aggregation through the
  container compared with MySQL. It is part of the rc and stable
  profiles.
- A production-shaped report suite: an invented eleven-table schema at
  600K rows and six report shapes, each run at four ceilings with every
  answer required to agree.

### Performance

- The hash join's build-side key filter tests a typed integer key column
  through a bitmap of the probe's keys instead of building a `Value` and a
  hash key per build row, and the probe is read ahead to filter the build
  only when the build is estimated at least four times the probe's size.
  Together these return the executor instruction gate's 4,096-row join to
  3% below its baseline from 20% above it.
- Backups and restores stream their segment transfers, four objects at a
  time. Segments above 8 MiB upload as multipart with two parts in flight
  and a whole-object digest computed as the parts are read; restores write
  each object to disk as it arrives while checking its size and SHA-256.
  On a 10 GiB synthetic dataset over loopback MinIO a full backup fell from
  about 48 s to 26 s and a restore from 22 s to 14 s, with peak client
  memory at 256 MiB segments down from about 265 MiB to under 100 MiB.
- Initial snapshots run their workers as spawned tasks, each chunk's row
  conversion and segment write on a blocking-permitted thread, instead of
  polling every worker from one future so that only one converted or wrote
  at a time. Composite-key pages seek with ordered prefix predicates
  (`a > ? OR (a = ? AND b > ?)`) rather than a row comparison MySQL cannot
  range-scan. On a one-million-row synthetic source: four tables 6.4 s to
  2.2 s, a composite-key table in 10,000-row pages 23.6 s to 6.8 s.
- Parallel aggregate rounds run as row-range morsels: the general,
  fused-join and two-pass paths cut a round's batches into bounded row
  ranges the pool takes dynamically, so a round of one or two batches - a
  small table, the tail of a scan, a round cut short by the memory
  ceiling - runs on every thread instead of one or two. The general path
  runs its morsels in waves whose memory bound fits half the ceiling, and
  the fused join bounds a round by its plan's groups rather than per probe
  row. On a ten-million-row table with ten threads an expression-keyed
  GROUP BY fell from about 410 to 270 ms, a 200K-group GROUP BY from 7.5 to
  5.5 s, and a join-and-group that failed under the shipped ceiling
  answers in 85 ms; on a 150K-row table the general paths halve.
- A comparison between an unsigned column and a signed integer literal,
  or the reverse, stays on the packed kernel. The binder types a small
  literal as signed, so `id >= 1` on an unsigned key evaluated row by row
  over every row of a predicate that excluded nothing; a filtered count
  over ten million rows fell from about 170 ms to 55 ms.
- A literal on one side of a join equality now reaches the other side's
  scan. `WHERE a.k = 5` with `ON b.k = a.k` derives `b.k = 5` onto the
  scan of `b`, so both sides prune segments and blocks instead of one.
  Inner joins carry constants both ways, a left join only into its
  null-supplying side; semi, anti and scalar-subquery joins, mismatched
  types or collations, ENUM keys, self-joins and anything beneath a
  LIMIT, window, aggregate, DISTINCT, derived table or set operation are
  left alone. On an eleven-table grouped report over 850K rows the
  derived predicates took 35 % off the run time. Forty-five oracle cases
  pin the pass and its boundaries byte-exact against MySQL.

### Changed

- The analytical benchmark runs against ClickHouse 26.8 LTS (was 25.8), and
  the keyword and function compatibility matrix reads that image's
  inventory. Its concurrency sweep is a mixed workload, Q2 through Q8
  round-robin per call with a per-query breakdown in results.json, beside
  the full-table count as its own row. The README states the 4 GiB
  per-query ceiling the harness has always run with, and that the
  RMT+FINAL column is charged without a live update tail and is therefore
  a lower bound on ClickHouse's merge-on-read cost (issue #31 tracks the
  live-tail phase). A smoke-scale run caches its MySQL baseline separately
  instead of overwriting the full-scale ledger.

### Fixed

- A decoded segment sliced into batches retained the whole segment's
  allocation once per batch: `Vec::split_off` leaves the head holding the
  original capacity, so a 1M-row segment cut into sixteen batches held
  about eight times its data for as long as those batches lived, and a
  plain GROUP BY over ten such segments asked 1.28 GB on its first pull
  under the 512 MiB default ceiling. Prefixes are now right-sized and the
  adopted string column is pre-sized from its arena.
- A spilling aggregation or sort holds a bounded number of open files
  however many runs it spilled: one while writing runs, seventeen while a
  merge pass runs, sixteen in the final merge. Runs close as soon as they
  are written and reopen on demand, and more runs than the fan-in are
  first reduced in passes that copy records without combining them, so a
  partial aggregate state is combined exactly once and in the order the
  runs were written. Before this every run kept a file open until the
  final merge, and a grouped report that spilled a few hundred times
  exhausted a container's 1024-descriptor limit with
  `aggregate spill create: Too many open files`. `EXPLAIN ANALYZE` now
  reports the peak number of open spill files.
- The streaming two-pass aggregate, which takes single-column GROUP BY
  with plain counts, sums, extremes and integer DISTINCT counts, spills
  its group maps to sorted runs under memory pressure instead of holding
  its whole state; a per-entity DISTINCT count over a large table now
  completes under a ceiling smaller than its state.
- A grouped aggregation over one integer column with aggregates outside
  the two-pass lanes ran on a path that never spilled and failed at the
  ceiling; it now spills like the others. The buffered aggregate counted
  its round's batches twice against a tight ceiling, refusing the first
  round of a query it had already paid for. A join cut its output batches
  where the columnar copy still fits instead of refusing them once
  buffered.
- `COUNT(DISTINCT)` over a text column counted a value once per spill run
  or parallel partial when the same group met the same value in more than
  one: merged distinct keys are collation sort keys, and re-normalizing a
  sort key produced a different key. Merged keys are now absorbed as
  they are.
- A grace hash join buffers each partition's rows and appends them in
  bursts, so its thirty-two partitions hold one descriptor between them
  instead of one each, and it releases its files as soon as every
  partition is served.
- A hash join whose build side is refused by the process-wide memory
  budget now partitions to disk like one refused by its own ceiling,
  instead of failing the query with `server memory limit exceeded`. Under
  load the budget is a backpressure valve: queries slow down rather than
  fail.
- The server raises its own soft open-file limit toward 65,536 at startup,
  capped by the inherited hard limit and never lowering it;
  `PINTAIL_KEEP_OPEN_FILE_LIMIT=1` opts out.
- A derived table whose `ORDER BY` names a column outside its select
  list, such as `(SELECT a FROM t ORDER BY b LIMIT 3)`, failed to plan
  with an internal layout error. The hidden sort key is no longer exposed
  as a column of the derived table.
- A grace hash join that overflowed while a partition was being served
  could not split that partition again and failed with
  `grace run read twice`. Spill runs now reopen from disk on every read,
  and every run closes its writer once probe routing finishes, so the
  join holds far fewer open descriptors while it serves.
- The server logs its effective limits at startup: admission slots,
  per-query and shared memory, process memory, descriptor soft and hard
  limits, and the spill directory and its ceilings.
- The compose file passes `PINTAIL_MAX_CONCURRENT_QUERIES` through and
  raises the container's descriptor limit; the installer's generated
  compose file does the same and warns when an existing one lacks it.

## [0.1.2-rc4] - 2026-09-06

### Performance

- Date-function predicates now prune and vectorize. The optimizer rewrites
  `DATE(c)`, `CAST(c AS DATE)` and `YEAR(c)` compared with a literal
  (=, <>, <, <=, >, >=, BETWEEN, NOT BETWEEN, IN, NOT IN) into half-open
  ranges on the wrapped DATETIME or DATE column, so segment and block
  pruning and the vectorized filter mask apply. On a 10M-row table
  `DATE(created_at) BETWEEN` fell from 15 s to the plain range's
  milliseconds. Time-bearing literals other than exact midnight, impossible
  dates and text columns keep their original evaluation; every rewritten
  shape is checked byte-exact against MySQL in the fixed oracle corpus.

### Fixed

- `DATE(c) = 'YYYY-MM-DD 00:00:00'` and `<>` with a midnight datetime
  literal now match MySQL, which promotes the date to midnight before
  comparing; the previous string comparison never matched.

### Added

- Query admission reserves up to two slots for simple queries over small,
  revalidated cached replicas whose entire database has at most 1,024 physical
  rows and 4 MiB of stamped files, while preserving the configured total limit.
  Larger replicas use the general pool, which cannot consume the reserve.

- An independent auditor command runs the benchmark from an isolated clean
  checkout with fresh MySQL timings and portable provenance artifacts.

- The MySQL function ledger now separates linked historical differential tests,
  implementation-only coverage and reviewed missing functions. Coverage can
  link both the E2E corpus and a banked, complete fixed MySQL oracle run.

- A push/PR instruction-count gate checks four answer-verified queries.
- Docker builds support opt-in PGO with `PINTAIL_PGO=1`. On the four-query
  training workload, PGO reduced instructions a further 16.83% after tuning
  the release profile; this is not a server-wide latency measurement.
- Release builds now use Thin LTO and one codegen unit, reducing instructions
  by 5.91% on that workload while retaining portable CPU settings and unwinding.

- Local predicate-partition and equivalent-query checks run in push CI.
- Fuzz targets exercise wire messages, binlog event decoding and stored
  records; malformed stored collection counts are rejected before allocation.
- A memory watchdog samples once per second and cooperatively cancels the
  largest tracked query when process or query-budget usage reaches 90%.

- A recovery suite with test-only failpoints and 38 isolated scenarios across
  eight fault areas plus a baseline. It checks exact rows, keyless duplicate
  counts, column metadata, repair state and continued writes after a second
  restart. Stable validation runs and banks the suite separately from E2E.

### Fixed

- Memory-pressure cancellation now allows five seconds for a victim to release
  memory before choosing another, preventing cancellation on every sample.

- Benchmark retries refresh ClickHouse's published port after container
  restarts and retain readable crash diagnostics in the private run log.

- Closing a table now waits for background compaction before releasing its
  writer lock, preventing reopen cleanup from racing temporary segment writes.
  Resnapshot also discards pending compaction results so old rows cannot be
  published into the reset table.

- Polling detects changed source columns at scheduled reconciliation and
  repairs them through the existing resnapshot policy while healthy tables
  keep polling. Reconciliation also repairs values whose cursor was unchanged or moved backwards.
- Interrupted copies retain their pending work across errors and restarts,
  including keyless tables under quarantine policy (metadata migration 21).
- A source connection failure during automatic CDC resnapshot now flags
  incomplete copies for repair immediately, preventing a later CDC cycle
  from treating a reset but unfilled table as healthy.
- Automatic CDC resnapshot attempts report the unavailable source-position
  error in server diagnostics.
- Successful full resnapshots clear dead letters for repaired tables before
  restarting replication; failed table copies retain their diagnostics.

## [0.1.2-rc3] - 2026-09-05

### Fixed

- A `MySQL` table with a `VIRTUAL` generated column no longer loops through
  quarantine and resync under `binlog_row_metadata=MINIMAL`. The probe
  dropped such columns on the belief that row images omit them; `MySQL`
  writes them, so every row event was one column wider than the schema,
  every event failed, and the automatic resync recopied the table with the
  same schema every five minutes. The column now stays in the schema (and
  replicates, and appears in `SHOW CREATE TABLE`) on `MySQL` 5.7 and 8,
  under FULL and MINIMAL metadata alike. `MariaDB` leaves the value out of
  UPDATE after-images, so there the column is skipped with a warning that
  says so; the probe now records every column's source position and the
  decoder places image values by it, so a skipped column no longer makes
  every wider image undecodable under MINIMAL metadata. A virtual column
  joining a schema mid-stream - through a logged ALTER or a missed one - is
  recopied rather than evolved in place, so existing rows carry its values.
- A snapshot resumed after a restart advances past the chunks its journal
  already holds instead of re-reading them from the source, which on a
  large replica re-read the whole source before reaching the one table that
  needed copying.
- A restart during a table resync no longer turns into a whole-database
  snapshot. Tables keep a copy-complete marker independent of the walk
  state; on boot, fully copied tables a restart left pending, mid-walk or
  in error go straight back to streaming, the interrupted table resumes as
  a one-table resync, a table a failed job left in error without a complete
  copy is handed to the automatic resync, and the database leaves its error
  state. A snapshot on a database that already replicates copies
  only tables without a complete copy and leaves the rest live.
- One table failing to copy no longer fails the whole snapshot job and
  marks every other table error: the table is flagged for resync, the run
  copies the rest, and the handoff proceeds with a `snapshot.partial`
  event naming what was skipped.
- A `MariaDB` stream no longer risks a corrupted binlog file name at every
  connection. The artificial rotate event that opens a stream carries the
  requested position rather than zero, so it was taken for a real rotation,
  and its unstripped checksum bytes became part of the file name whenever
  they happened to be printable. The artificial flag now decides.
- Every snapshot table logs its start and end at info level, with the
  chunks it skipped on resume; replication errors carry their full cause
  chain, so a metadata-file failure names the OS error behind it.

## [0.1.2-rc2] - 2026-09-05

### Fixed

- A table with a secondary UNIQUE key on a natively replicated (CDC)
  database no longer fails large scans with the query memory limit. The
  read policy that hides the older side of a unique-value collision has to
  hold the table's whole projection in memory to find one, and the wire
  engine applied it to every table with such a key; a one-row `COUNT` over
  a large mirrored table then failed on the row that no longer fit. The
  policy now applies only where a collision can exist - polling databases,
  and CDC tables flagged for periodic reconciliation - and every other
  table streams its scan as before.
- A local (writable) database refuses `BEGIN`, `START TRANSACTION`,
  `COMMIT`, `ROLLBACK`, `SAVEPOINT` and `SET autocommit=0` with MySQL
  error 1149 instead of accepting them as compatibility no-ops. The
  no-op told a client that a `ROLLBACK` had undone an `INSERT` whose row
  was durably stored - a wrong answer the client had no way to detect.
  Every statement on a local database is still its own autocommit
  transaction until explicit transactions land. Replicated databases are
  unchanged: they write nothing, so the no-op that lets drivers and BI
  tools open a transaction before a `SELECT` claims nothing false.

### Changed

- A correlated subquery on the dependent path - the shapes the binder
  cannot rewrite into a join - now executes its inner query once per
  distinct outer tuple instead of once per outer row. A statement-local
  memo, keyed on the outer values substituted into the inner query and
  charged to the query's memory ceiling, shares the answer across rows
  that ask the same question; a refused charge drops the memo and the
  query finishes exactly as before. Inner queries using `RAND()` or
  `UUID()` are never memoized, NULL is its own key, text keys compare
  bytewise (never a wrong hit under a case-insensitive collation), errors
  are not cached, and `IF`/`COALESCE` branches not taken are still never
  run. Measured 18-200× on scalar and `EXISTS` shapes at a thousand
  distinct keys over twenty thousand rows, 21-1600× on correlated `IN`
  (`benchmark/evidence/dependent-subquery-memo.md`).
- `bun run scripts/validate.ts` runs explicit profiles: `development`
  (fmt, typecheck, unit), `rc` (plus oracle, e2e, e2e-mysql80, browser)
  and `stable` (plus bench and accept). Every run writes its own report
  directory recording HEAD, toolchain, requested and skipped stages, and
  a `--stages=` subset reports `PASS (SUBSET)` with the skipped stages
  named; `validate-out/latest` and `latest-complete` point at the newest
  run and the newest complete one.

### Added

- The wire listener bounds what a client can hold open without running a
  query. `--wire-max-connections` (default 1000, MySQL's own) counts every
  accepted connection, authenticated or not, and answers the one beyond it
  with MySQL's "Too many connections" (1040) in place of the greeting.
  `--wire-max-prepared-statements` (default 1024 per session, plus a
  16 MiB ceiling on retained statement text) refuses the PREPARE beyond it
  with MySQL's 1461, before the statement is parsed. Both are also
  settable as `PINTAIL_WIRE_MAX_CONNECTIONS` /
  `PINTAIL_WIRE_MAX_PREPARED_STATEMENTS` and under `[wire]` in the config
  file; zero disables a bound. `/metrics` gains
  `pintail_wire_connections_active`, `pintail_wire_connections_limit`,
  `pintail_wire_connections_refused_total` and
  `pintail_wire_prepared_statements_refused_total`.
- The encoded, wire-ready copy of a result set - built after execution
  releases the query's memory tracker - is now held to the same per-query
  ceiling, so a result whose encoded form alone exceeds the limit is
  refused as a memory-limit error rather than being the one allocation
  the ceiling never saw.
- The overview's "Storage engine" card is now disk usage: the volume
  behind the data directory, and the system volume alongside it when the
  data directory is mounted elsewhere. Backed by `GET /api/storage`.

## [0.1.2-rc1] - 2026-09-04

A candidate for the `GROUP BY` refusal that took a customer's analytics
endpoint down. Pintail required every selected column to be grouped or
aggregated; MySQL requires it to hold ONE value per group and proves that
through the table's key, which is why grouping by a foreign key and
selecting the joined dimension's name is ordinary SQL everywhere else.
The candidate also carries the deployment work that keeps a published
port off the public internet, which a host firewall cannot do on its own.

### Fixed

- A column the grouping keys functionally determine is answered rather
  than refused with `ER_WRONG_FIELD_WITH_GROUP` (1055). The proof follows
  MySQL's own `ONLY_FULL_GROUP_BY` rules: a table's key fixes the rest of
  its row, and an equality in `WHERE`, in an inner `ON`, or in an outer
  join's `ON` against the outer side carries that onto the joined table's
  key. So `GROUP BY orders.id` may select `orders.placed_at`, and
  `GROUP BY enrollment.payment_type_id` may select the LEFT JOINed
  `payment_type.name` - the shape every dashboard and BI tool writes, and
  the one whose refusal broke a customer's analytics endpoint. A
  determined column reads as `ANY_VALUE`, so a group whose outer join
  matched for some rows and not others stays one group with one set of
  counts, as MySQL answers it.

### Added

- `PINTAIL_BIND` sets the host address both published ports bind to,
  default `0.0.0.0` as before. A deployment that must stay on a private
  network could previously only be kept off the public internet by editing
  the compose file: Docker publishes ports through its own NAT rules,
  which are consulted before the filter rules `ufw` and `firewalld`
  manage, so a port on `0.0.0.0` answers the internet while the firewall
  claims to deny it. The installer carries the variable and reaches the
  service at that address. Two published addresses are two entries per
  port; where the readers sit on a private cloud network and a VPN,
  binding the cloud address and advertising its subnet as a VPN route
  reaches both from one published address. Selective rules belong in the
  `DOCKER-USER` chain; both are documented beside the ports.

## [0.1.1] - 2026-09-03

Stops the server growing without bound on a database whose tables cascade.
0.1.0 had left the allocator holding everything it freed; the candidates
fixed that and then removed what was allocating it, the scheduled cascade
reconciliation, which re-read whole child tables from the source and held
them in memory. This release carries both candidates and the work that
made the repair fast as well as bounded: at twenty million rows a cascade
of ten parents is repaired in half a minute inside a two-hundred-megabyte
peak, where the same repair on 0.1.0 peaked at a gigabyte over its
baseline on a table a tenth the size.

Includes everything in 0.1.1-rc1 and 0.1.1-rc2.

### Fixed

- Creating a database honours the `poll_interval_seconds` and
  `reconcile_interval_seconds` it was given; both were accepted and then
  replaced with the defaults, so only an update could set them.
- A scan under a memory budget reads a segment the budget cannot hold
  whole in block-aligned row slices instead of refusing it; a compacted
  twenty-million-row table had stopped the cascade repair every interval.
- Reconciliation repairs go through the plain ingest, tombstones carry
  placeholder values, and candidates are verified five thousand a query.
  At twenty million rows the operator's full compare converges in about
  three minutes where it had not converged in half an hour.

### Added

- `tests/e2e/results-scale.md` records the reconciliation measured at ten
  times the gate's size, both passes, with what it implies for tables in
  the hundred-gigabyte range.

## [0.1.1-rc2] - 2026-09-02

A second candidate for the memory fix. rc1 stopped the allocator from
hoarding freed memory; this one removes the thing that was allocating it,
the scheduled cascade reconciliation, which re-read whole child tables
from the source and held them in memory. The e2e gate now measures that
repair over a two-million-row child and fails on the old behaviour.

### Fixed

- Cascade reconciliation no longer reads the child table from the source.
  A child row an invisible `ON DELETE`/`ON UPDATE` cascade can have touched
  is one whose parent the replica no longer holds, so the scheduled pass
  streams the child replica's key and referencing columns, looks each
  parent up in the parent replica, and verifies only those candidates
  against the source. A staging node had spent minutes and gigabytes every
  ten minutes re-reading a two-million-row child for a handful of parent
  deletes; the same repair now touches the source for the affected rows
  alone, and its memory is one streamed chunk regardless of table size.
- The full compare an operator requests, and the fallback for cascading
  keys that do not reference a replicated parent's primary key, no longer
  holds the table's key set: replica keys are verified against the source
  in batches, so its memory is bounded for tables of any size.
- A grouped aggregation whose group map had filled the query ceiling could
  fail on a small reservation the partial-group build made; the build now
  spills before that point and retries once on a full budget.
- A polling database can resnapshot one table without a binlog fence:
  the fence guards a CDC stream against replaying rows the snapshot just
  copied, and a polling source may write no binlog at all.
- The all-zero `DATE` and `DATETIME` cross the wire's binary protocol as a
  zero-length temporal, as MySQL sends them; a prepared-statement read of
  such a row had failed with "input is out of range".

### Changed

- The e2e gate gains a `reconcile-memory` phase: a cascade delete over a
  two-million-row child, the repair sampled for memory from the deletes,
  and a bound on its peak over the baseline.

## [0.1.1-rc1] - 2026-09-02

A release candidate for the memory fix: the server no longer hoards freed
memory, which on a staging node had grown to seven gigabytes for half a
gigabyte of data. It also carries the first batch of MySQL-fidelity work
measured against MySQL's own regression suite, and the stress evidence
for both. Gate: unit, oracle, end-to-end on MySQL 8.4 and 8.0, browser.

### Fixed

- Unaliased output columns are named by their source text the way MySQL
  names them: `floor(5.5)`, `round(5.64,1)`, a bare string literal by its
  value. They were named from the parser's rendering (`FLOOR(5.5)`,
  `round(5.64, 1)`), which MySQL's own regression suite flagged 315 times.
- MySQL literal forms bind: double-quoted and `N'...'` strings, `X'..'`,
  `0x..` and `b'..'` binary literals, `DATE '..'` / `TIME '..'` / `TIMESTAMP '..'`
  typed strings, charset introducers, integer literals past BIGINT UNSIGNED
  (read as DECIMAL, as MySQL does) and `FROM DUAL`.
- `INSERT(str, pos, len, newstr)` and `TIME(expr)`.
- `ADDTIME`, `SUBTIME`, `TIMEDIFF`, `PI()`, `RAND(seed)` with MySQL's generator,
  and `HAVING` without `GROUP BY` filtering by the select list.
- `ROUND`, `TRUNCATE` and `FORMAT` read their digit count as a saturating
  64-bit integer instead of overflowing; `FORMAT` rounds the decimal text
  half away from zero (`FORMAT(4.55, 1)` is `4.6`); `UNIX_TIMESTAMP` and
  `FROM_UNIXTIME` honour MySQL 8.0.28's 3001-01-18 ceiling; an unparseable
  date in a scalar function is NULL, as MySQL answers, not an error;
  `GREATEST`/`LEAST` over mixed signed and unsigned integers stay exact;
  `COLLATE` accepts the legacy `*_bin`, `*_general_ci` and `*_swedish_ci` names.
- Temporal columns of a local table store MySQL's canonical text at the
  column's precision, and local DDL keeps `TIME(n)`/`DATETIME(n)` precision.
- Bit operators `|`, `&`, `^`, `<<`, `>>`, evaluated over BIGINT UNSIGNED as
  MySQL does.
- Local databases accept tables without a primary key, keeping every row
  under a generated id as the replica does for keyless source tables, and
  the column declarations fixtures carry: COMMENT, UNIQUE, ON UPDATE,
  DEFAULT NULL, CHARACTER SET, COLLATE and AUTO_INCREMENT.
- The server no longer hoards memory it has freed. A staging node held
  3.9 GB resident plus 3 GB of swap for 527 MB of data: glibc malloc keeps
  each thread's freed memory in that thread's arena at the high-water mark
  of whatever query once ran there, and analytical queries landing on fresh
  blocking threads filled fifty such arenas. The binary now uses jemalloc,
  which returns freed pages on a decay timer. In the memory soak the shipped
  image sat at 2.9 to 3.4 GB with a gigabyte in swap; this build oscillates
  between 0.2 and 1.5 GB and never swaps.

### Added

- `tests/memsoak`: a memory soak of the actual Linux image on the docker
  host - hundreds of tables, a fast supervisor, a source writer and wire
  query clients - judged on the per-minute memory floor after warm-up and
  on swap. The first memory measurement that runs where the release runs.
- `tests/mtr`: MySQL's own regression suite replayed against Pintail. The
  query-shaped files of `mysql-test/t` are fetched at run time, their
  fixtures built into per-file local databases, and every SELECT compared
  with live MySQL byte-for-byte.

## [0.1.0] - 2026-09-02

Makes the server fit a small container under a lot of concurrent load, and
adds the stress evidence that proves it: a memory-pressure phase in the
end-to-end gate and a constrained profile in the load harness.

### Changed

- One replica cache for the whole process. Every wire connection and every
  HTTP request used to load and hold its own copy of a database's tables -
  a manifest read, a WAL replay into a fresh memtable and a segment
  verification per table per connection, charged to nothing - and any
  change to any file, including the metadata the supervisor writes every
  cycle, threw the whole database away and reopened every table. The cache
  is now shared by every engine in the process, a change reopens only the
  table whose files or schema moved, resident memtable bytes are reserved
  from the process memory budget and released on eviction, and the number of
  resident databases is bounded (`PINTAIL_REPLICA_CACHE_DATABASES`, default
  32) with least-recently-used eviction. A replica the budget cannot hold is
  served once and refused a slot rather than counted as free.
- Wire connections are authenticated off the runtime thread, and a key's
  connection bookkeeping - `last_used_at` and the `wire.connect` audit row -
  is written once per key per minute instead of per connection. Every
  connection used to make two `SQLite` writes on the worker that accepted
  it, queued behind the replication applier's own writes; under a
  connection storm every worker was parked there and the dashboard and
  HTTP queries stalled with them.
- A replica is reloaded once when its files move, however many queries
  notice at the same time: the rest wait for the reload and answer from it
  instead of each replaying the same WAL tail into its own memtable.
- HTTP queries run on a blocking thread. `POST /api/query`, table preview
  and table count executed the statement inline on the runtime worker that
  received the request - admission wait, WAL replay and execution included -
  so a few dozen HTTP query clients parked every worker inside a query and
  the wire connections and the dashboard, which need those workers only to
  move bytes, waited on them: in the constrained load profile wire queries
  the server finished in 341ms took clients a p99 of 65 seconds.

Measured on the constrained profile at 128 clients reconnecting per query
with a CDC writer, dashboards and HTTP queries alongside, before and
after: wire p50 5.1s → 2.0s, wire p99 147s → 2.5s, peak RSS 2,354 MB →
727 MB, dashboard p99 10.5s → 27ms, and the admission window's refusals
now arrive in the client as the designed 1040 instead of as latency. The
e2e memory-pressure phase reads wire p99 826ms, health p99 18ms and peak
RSS 392 MB on a 256 MB budget.

### Added

- `tests/load` grew a `constrained` profile (`LOAD_PROFILE=constrained`):
  a 512 MB process budget, sixteen admission slots, a connection per query,
  and a CDC writer, dashboard pollers and HTTP query clients running
  alongside every level. It fails the run if peak RSS passes 1 GB, the
  replica does not catch up with the writer, or any wire failure is
  something other than the two designed refusals. Every setting is also an
  environment variable.
- End-to-end `memory-pressure` phase: a 256 MB budget, a 32 MB per-query
  ceiling and eight admission slots against forty-eight reconnecting wire
  clients, eight HTTP query clients, six dashboards and a CDC writer at
  once. The server has to survive, refuse only by design, keep answering
  health, stay under 1 GB, catch up afterwards and answer plain queries
  once the storm passes.

## [0.0.5-rc4] - 2026-09-02

Recovers a database whose snapshot a restart interrupted, and adds the
first stress phases to the end-to-end gate: a dashboard activity feed over
150,000 rows of control-plane history, twenty-five dashboards polling for
twenty seconds while rows are written at the source, and a SIGKILL landed
with a copy provably in flight that must recover with nobody touching it.
The stranding phase went red before its fix and green after.

### Fixed

- A restart during a database's snapshot no longer strands it. Quarantining
  the tables caught mid-copy was only half of recovery: the database itself
  was left in `created`, `probed` or `snapshotting`, none of which the
  supervisor schedules, so nothing ever ran its cycles, reached the automatic
  table repair, or copied the tables it had not got to. A production instance
  sat that way for over a day with 108 tables quarantined and 134 never
  copied. The copy now resumes at boot on the same job slot an operator's
  click would take - forced if the interrupted copy was a forced one - and
  says so in the activity feed, or says why it could not. Pinned by a new
  e2e phase that kills pintail with a copy provably in flight and then
  touches nothing.

## [0.0.5-rc3] - 2026-09-02

Corrects a caching defect introduced in 0.0.5-rc2 that pinned browsers to
a stale build, and makes every request measurable end to end.

### Fixed

- `_nuxt/builds/latest.json` is no longer served as immutable. It keeps a
  stable filename inside an otherwise content-hashed tree, and Nuxt reads
  it to notice a new deployment, so caching it for a year pinned every
  browser to the build it first saw. Introduced in 0.0.5-rc2 by the asset
  caching itself and caught by an independent review before it reached a
  stable release.
- The `ETag` on embedded assets is now honoured: a request carrying a
  matching `If-None-Match` gets a `304` instead of the whole body again.
  It was previously emitted but never evaluated, so revalidating clients -
  which is every client for the non-immutable HTML shells - re-downloaded
  everything.

### Changed

- Every HTTP request is logged with two timings, and static assets are
  logged at all. The access log covered only `/api`, so the ~90 asset
  requests a dashboard load makes were invisible, and it stopped the clock
  when the handler returned rather than when the client had the bytes. A
  request the handler answered in 3ms could take 40 seconds to arrive and
  the log would show 3ms - which is exactly how a real slowdown stayed
  hidden. Each line now carries `handled=` (time to produce), `sent=`
  (time until the last byte was delivered), the byte count, and whether
  the client took the whole response or gave up.

## [0.0.5-rc2] - 2026-09-02

Fixes a dashboard that became unusable on a long-running deployment.
Diagnosed on a live instance carrying 632,000 replication-cycle rows,
where the activity feed took over two minutes to answer while
replication itself stayed healthy.

### Fixed

- The dashboard's responses are compressed. Nothing was compressed at
  all - a 21KB HTML shell and every JavaScript bundle went out raw, even
  when the client advertised gzip - which on a high-latency link is most
  of the page load. Assets and API JSON now compress.
- The dashboard's embedded assets are cacheable. Every response carried
  only `content-type` and `content-length`, so a browser refetched all of
  Nuxt's content-hashed chunks on every visit - a captured trace of one
  page load showed 72 `_nuxt/*` requests, none of them cacheable, none
  compressed. Hashed bundles now answer
  `cache-control: public, max-age=31536000, immutable` (a new build is a
  new URL, so the old one can be held forever) and the HTML shells answer
  `no-cache` so a deploy is never served from a stale cache. Every asset
  also carries an `ETag`, which lets a CDN in front hold the immutable
  ones instead of returning to the origin at all.
- The dashboard no longer slows down as a deployment ages. A replication
  cycle writes one `sync_runs` row every supervisor cadence - 17,280 a day
  at the 5-second default - and nothing prunes it, but the activity and
  dead-letter feeds read those tables newest-first with no index on the
  sort column. Every dashboard load therefore scanned and sorted the whole
  history: measured at 145ms over 300,000 rows (about seventeen days of
  uptime) and growing linearly from there, which is why a long-running
  instance answered slowly while replication itself stayed healthy.
  Migration 19 indexes both tables, and the workspace-scoped feeds are
  split into one statement per shape so the planner walks the index in
  order instead of sorting every matching row. The same reads now answer
  in under 1.5ms over the same 300,000 rows.

## [0.0.5-rc1] - 2026-08-25

First release candidate of the 0.0.5 line. Adds writable local databases
(issue #7 phase 2) alongside the read-only replica, and closes a decimal
rounding boundary the new high-volume MySQL corpus found.

### Added

- Local (Pintail-owned, writable) databases, phase 2 of issue #7: a
  database kind that has no source, accepts `CREATE TABLE` and `INSERT`
  with primary-key enforcement, and is refused by every replication path
  (probe, snapshot, resnapshot, reconciliation, dead-letter retry, and
  supervisor scheduling). Replicated databases keep the read-only
  rejection for every mutating statement. A locally declared column is
  typed through the probe's own mapping, so a local table answers queries
  under the same rules as a mirrored one. Writes arrive over the MySQL
  wire and answer with an OK packet carrying their affected-row count,
  rejections carry MySQL's own codes (1050, 1062, 1048, 1146, 1054), and
  `POST /api/databases/local` creates one. `UPDATE`, `DELETE` and explicit
  transactions are not implemented (issue #7, phases 3-4).

### Fixed

- Negative `ROUND`/`TRUNCATE` digit counts over exact computed decimals
  remain on the exact-decimal path. SQL parses `-2` as unary minus over an
  integer literal; treating that expression as a dynamic digit count sent
  `ROUND(50.00 + 0.00, -2)` through nearest-even floating-point rounding
  and returned `0` instead of MySQL's half-away-from-zero `100`. Negative
  digits now round the original scaled units once, so `ROUND(949.86, -2)`
  returns `900` rather than double-rounding through `950` to `1000`.

### Changed

- The differential oracle grew from 731 to 1,081 fixed cases and now gates
  on MySQL 8.0 as well as 8.4, and its generated corpus covers 16 typed
  query families. High-volume sweeps totalling 102,500 generated
  statements ran with zero invalid or skipped SQL; both rounding defects
  above were found by that corpus rather than by hand
  (`tests/sqllogic/fuzz-results.md`).

## [0.0.4] - 2026-08-21

First stable cut of the 0.0.4 line, gated by the full release chain:
unit, oracle (874 differential cases, 400-case fuzzer, metamorphic
pack), e2e on MySQL 8.4 and 8.0 under binlog_row_metadata=MINIMAL,
browser, the 20M-row analytical benchmark, TPC-H, and acceptance on the
banked tree. Carries everything in the rc1-rc11 series plus the fixes
below.


### Added

- `PINTAIL_SNAPSHOT_WORKERS` caps snapshot/resnapshot copy parallelism
  (default 4, clamped 1-16) for hosts where the copy workers would
  otherwise saturate CPU or disk and slow the dashboard and query paths
  sharing the process.

### Fixed

- A JSON-extracted string's `utf8mb4_bin` collation now survives derived
  table and CTE boundaries: `SELECT DISTINCT s FROM (SELECT meta->>'$.k'
  AS s ...) d` kept case variants apart in MySQL but folded them in
  Pintail, because the inner projection's output column recorded the
  session default collation instead of the JSON producer's.
- `ROUND`, `TRUNCATE`, `CEILING`, and `FLOOR` over a computed decimal
  operand now read the operand's internal digits the way MySQL does (a
  scale-4 division carries 9 truncated fractional digits for its parent)
  instead of its display value, and cap their result scale at the
  operand's declared scale: `ROUND(28100/508, 2)` is `55.31` from the
  internal `55.314960629`, where rounding the displayed `55.3150` had
  double-rounded to `55.32`.

## [0.0.4-rc11] - 2026-08-21

The test-diversity release: a differential grammar fuzzer and a
dockerless metamorphic pack join the oracle gate, the e2e corpus grows
from 95 to 159 unique queries (BI-tool shapes, star-schema joins, SET
and geometry byte contracts, an errno/SQLSTATE rejection matrix, a
verified contention storm), the gate runs under MySQL's default
binlog_row_metadata=MINIMAL, and a second-major mysql:8.0 leg becomes a
release stage with its own environment-stamped ledger. The widened net
caught and fixed six engine bugs on first contact, including a
MINIMAL-metadata replication freeze that production sources at default
settings could hit.

### Added

- Erroring queries answer with MySQL's errno and SQLSTATE instead of a
  blanket 1064 parse error: unknown database (1049), unknown table
  (1146/42S02), unknown column or relation qualifier (1054/42S22),
  ambiguous column (1052/23000), ungrouped column (1055/42000), a group
  function outside an aggregation scope (1111/HY000), and a row-wise
  numeric overflow (1690/22003).
- The supervisor automatically recopies a keyed table that CDC quarantined
  as unplaceable (a MINIMAL-metadata stream more than one hidden ALTER
  behind), through the operator resync flow with a per-table cooldown.
  Keyless tables stay with `keyless_policy`; a successful recopy purges the
  table's superseded dead-letter rows only after its state transition lands.
- The e2e gate runs under `binlog_row_metadata=MINIMAL` (MySQL's default)
  by default, records the source image, server version, and metadata mode
  in its banked ledger, and gains a second-major leg: the `e2e-mysql80`
  validate stage runs the full gate against mysql:8.0 on a fresh container
  and banks its own ledger.

### Fixed

- Constant predicates fold the way MySQL folds them: a constant-false WHERE
  returns the empty set instead of "physical input is missing <column>",
  and a constant-true disjunct absorbs the whole OR before row evaluation,
  so a doomed sibling expression (an unsigned subtraction underflow) is
  never evaluated.
- Date-part extractions (YEAR/WEEKDAY/QUARTER/...) type and evaluate as
  SIGNED integers like MySQL's own metadata, so `INTERVAL -WEEKDAY(x) DAY`
  no longer raises a spurious overflow.
- `LENGTH`/`CHAR_LENGTH` of a binary value count raw bytes (geometry WKB
  including the SRID prefix) instead of demanding the bytes be UTF-8 text.
- The empty SET value no longer inherits a reconstructed ENUM label slot's
  ordinal: `GROUP BY` over a SET column sorted the empty group wrongly once
  memtable rows entered the scan.

## [0.0.4-rc10] - 2026-08-21

The fast-gate release: the e2e differential gate drops from 16.4 to
6.5 measured minutes, and the faster loop immediately caught one
production race and three wire-metadata divergences that the slower
cadence had been hiding. First rc gated under the new policy:
correctness stages only (fmt, unit, oracle, e2e, browser); the bench
family runs for stable releases.

### Fixed

- An operator's polling-to-cdc mode switch could be silently reverted
  by any replication work in flight across it: the poll/CDC checkpoint
  commits and the probe's effective-mode write all updated the record
  unguarded, and once reverted, the correctly-guarded healing writes
  could never repair it - the database polled forever under a record
  claiming cdc. All three writers now judge the mode at write time
  (the same compare-and-set the supervisor's completion write already
  carried). The production 5s cadence narrows this window; a slow or
  busy source widens it, so this was reachable in deployments.
- JSON_UNQUOTE and ->> results decode as text again: rc9 advertised
  the right LONG_BLOB type byte with the binary charset, so drivers
  returned raw Buffers (a customer's conformance diff saw base64 where
  MySQL answers text).
- Text result metadata echoes the collation id the client NEGOTIATED
  in its handshake - measured against MySQL, a mysql2 client sees 224
  where the CLI sees 255 - instead of a fixed charset default.
- Constant folding kept NULL = NULL's declared Boolean type; the wire
  advertised VAR_STRING where MySQL says LONGLONG.

### Changed

- The e2e gate is instrumented (per-phase run/converge/corpus splits
  in a ledger Timing table) and runs in 6.5 minutes: 250ms poll
  cadence, a 2.5s supervisor test cadence, a parallel corpus sweep
  over a source connection pool, DDL-invalidated metadata caching,
  documented-gap short-circuits in both convergence loops, an
  optional persistent source container (PINTAIL_E2E_KEEP_MYSQL), and
  a per-stage Docker host override so e2e can run beside the release
  chain. The wire-type battery now compares charset bytes beside type
  bytes. The release binary builds once per validation run; fmt and
  unit overlap the remote stages; the benchmark image build keeps
  incremental state through BuildKit cache mounts.

### Known limitations (docs/limitations.md)

- ROUND/CEIL/FLOOR of an exact integer and SUM over exact integers
  advertise narrower types than MySQL while values agree
  byte-for-byte; JSON arithmetic remains rejected.

## [0.0.4-rc9] - 2026-08-21

The conformance release: a customer's 106-case differential suite and
the questions it raised drove two campaigns - first collation and
coercion parity (PAD SPACE, BINARY, COLLATE, the JSON utf8mb4_bin
model), then the twelve next limitations on the ledger, worked front
to back. The oracle grew from 874 to 1,081 byte-exact cases and the
e2e gate from 1,829 to 2,096 checks; six additional defects those new
cases exposed are fixed below.

### Added

- JSON reaches MySQL parity for querying. JSON-to-JSON comparison,
  ordering, grouping, DISTINCT and set duplicate handling follow the
  JSON type-precedence ladder (numbers equal across integer/double
  spellings, objects equal whatever the member order). Paths accept
  wildcards (`.*`, `[*]`), recursive descent (`**`), ranges
  (`[M to N]`) and `last`-relative indexes with MySQL's autowrap
  rules. The modification family lands - JSON_SET, JSON_INSERT,
  JSON_REPLACE, JSON_REMOVE, JSON_MERGE_PATCH - beside MEMBER OF,
  JSON_OVERLAPS, JSON_DEPTH, JSON_QUOTE and JSON_PRETTY (MySQL's
  exact two-space layout).
- Session-collation semantics: the wire handshake's charset byte sets
  the connection collation, literal comparisons follow it (PAD SPACE
  under general_ci clients, NO PAD under 0900_ai_ci), an explicit
  COLLATE dictates its comparison as coercibility 0, and utf8mb4_bin
  joins the supported profiles.
- New scalar functions: SHA1, SHA2, CRC32, UUID, BIN, OCT, INET_ATON
  (including the classful 1-3 part shorthands) and INET_NTOA; TRIM
  with a pattern (`TRIM(BOTH 'x' FROM ...)`); the null-safe `<=>`
  operator; CAST AS UNSIGNED/SIGNED wrap through two's complement as
  MySQL's explicit casts do; EXTRACT composite units (YEAR_MONTH
  through MINUTE_SECOND).
- Joins widen: RIGHT JOIN anywhere in a chain (rewritten to a
  left-preserving nested group), range/inequality ON conditions with
  no equality key run on the nested loop behind the cross-join
  cardinality guard, and correlated EXISTS/IN subqueries decorrelate
  with range predicates, not just equalities.
- Size-tier compaction merges run on a background thread: the ingest
  path only spawns a merge and publishes its result, so a large merge
  no longer stalls replication (previously 583k to 343k rows/s once
  merges engaged). The inline pass remains behind
  `background_compaction=false`.
- The wire endpoint serves caching_sha2_password FULL authentication
  (RSA key exchange toward a per-process keypair, or cleartext from a
  client that trusts its transport) and KILL QUERY, which interrupts
  the target connection's running statement through the same
  cancellation a disconnect uses.
- Audit rows record the network peer they arrived from, and the
  dashboard tables view filters by state.

### Fixed

- JSON function results collate utf8mb4_bin, as MySQL's do: grouping,
  DISTINCT and comparisons over JSON_UNQUOTE/`->>` text are
  case-sensitive even in a case-insensitive session, each DISTINCT
  key deduping under its own coercibility-ladder collation.
- Three parser-precedence bugs of one class: the JSON `->`/`->>`
  arrows, prefix BINARY, and BINARY before LIKE/BETWEEN all swallowed
  the comparison that followed; each now reassociates to MySQL's
  grammar. One BINARY operand also forces the whole comparison to
  byte semantics instead of falling into numeric coercion.
- information_schema ORDER BY answers byte order, as measured against
  MySQL - the interpreter's case fold broke metadata convergence and
  ORM introspection snapshots the moment the corpus held capitalized
  table names beside lowercase ones.
- The probe retains non-unique secondary indexes, so
  information_schema.statistics, SHOW INDEX and SHOW CREATE TABLE
  stop pretending tables have none; Drizzle and Prisma introspection
  now reproduce MySQL's output byte-for-byte.
- Arithmetic with either BIGINT UNSIGNED operand stays unsigned, a
  negative signed operand subtracting in the unsigned domain exactly
  as MySQL evaluates it.
- SEC_TO_TIME, MAKETIME, CONVERT_TZ and JSON_UNQUOTE advertise
  MySQL's own wire types (TIME, DATETIME, LONG_BLOB) as direct
  projections, in both text and binary protocols.
- LIKE defaults its escape character to backslash without an ESCAPE
  clause; ordering by a group-key alias resolves the grouping
  expression's collation; an untyped NULL projection satisfies any
  derived column type; a dependent EXISTS in a self-join no longer
  sinks below its filter into the scan; a resync rebuilds through a
  physical column-type change.

### Verification

- The oracle holds 1,081 byte-exact differential cases (from 874),
  the e2e gate 2,096 checks (from 1,829) including a user's vendored
  conformance seed, an extended wire-type battery, and the ORM
  introspection paths. Full suite: oracle PASS, e2e PASS (0 failed,
  6 documented-gap warnings), browser PASS.

## [0.0.4-rc8] - 2026-08-20

Temporal wire-type parity, reported from a customer's driver-level diff:
values matched byte-for-byte while the advertised column types did not,
so drivers decoded strings where MySQL hands back Date objects.

### Fixed

- DATE(x) - and the family - carry their temporal types to the wire.
  The binder declared every temporal function result Utf8, so the wire
  advertised MYSQL_TYPE_VAR_STRING; the same class GEOMETRY had in
  0.0.3. DATE, CURDATE, LAST_DAY, FROM_DAYS and MAKEDATE are DATE; NOW
  and FROM_UNIXTIME are DATETIME; CURTIME is TIME; DATE_ADD and
  DATE_SUB type from their argument, mirroring the evaluator's
  rendering rule and MySQL's own behaviour. Values were already
  canonical carrier text, so nothing changes but the type byte.
- Stored TIMESTAMP columns advertise MYSQL_TYPE_TIMESTAMP (7) instead
  of DATETIME (12), so clients that key session-timezone semantics off
  the type byte behave as they do against MySQL. The column flag rides
  the geometry flag's route, stays outside the schema fingerprint, and
  rebuilds from the durable source type on every open - existing
  mirrors need no resync.
- STR_TO_DATE types statically from a literal format the way MySQL
  does: date-only specifiers are DATE, time-only TIME, both DATETIME.

### Added

- The e2e gate gained the systematic guard this class needs: a battery
  of temporal expressions whose wire column-type BYTES must equal
  MySQL's - value comparisons can never catch a type divergence that
  decodes cleanly on both sides. The gate is now 1,829 checks.

### Known limitations (docs/limitations.md)

- SEC_TO_TIME, MAKETIME and CONVERT_TZ stay VAR_STRING: their
  fractional-second width follows the input value, which the
  fixed-width temporal carrier cannot represent - typing them truncated
  the fraction, and the oracle caught all three. Values match MySQL
  byte-for-byte as strings.
- STR_TO_DATE with a non-literal format stays a string; with a
  time-only format the declared type matches MySQL but the value is a
  pre-existing NULL gap.

## [0.0.4-rc7] - 2026-08-20

A production-shaped browser soak suite, and the transaction-size bug it
caught on its first run.

### Added

- A `soak` validation stage: the dashboard driven end to end in headless
  Chromium at production volume - a 2,048,000-row initial sync through
  the wizard with visible progress, dashboard actions during live drip
  ingest with a two-minute convergence requirement, an 18.4M-row CDC
  backfill under a liveness contract (the mirrored count must grow at
  every sample), a full Reset at 20,480,000 rows demanding moving
  progress, and the vendored sakila dataset (ENUM, SET, YEAR, GEOMETRY,
  foreign keys) registered and value-checked against MySQL through the
  SQL console. Opt-in only; the two-minute smoke gate still runs
  everywhere. Measured on the shared host: 2M sync in 23s, 32,000
  rows/s sustained CDC ingest, 20M reset in 292s.
- A page-level copy progress strip on the database detail page while a
  snapshot or reset rewrites tables: N of M tables complete, overall
  percent from durable chunks, and the note that leaving the page is
  safe.

### Fixed

- One source transaction was capped at 65,535 row mutations by two
  independent 16-bit gates - the row-version ordinal and a hardcoded
  guard - so a single real backfill batch quarantined its table
  permanently while the database badge kept saying streaming. GTID mode
  now budgets 24 ordinal bits (16,777,215 mutations per transaction,
  upgrade-safe since GTID sequences only increase); the file-position
  fallback keeps 65,535 and is recorded in docs/limitations.md. Proven
  live at 65,536, 131,072 and 262,144 rows per transaction, then by the
  soak's full 20M run.
- Long-running mirror actions are visibly alive: the job-slot wait
  announces itself immediately (queued toast) and waits minutes rather
  than seconds, non-transient conflicts fail fast with the server's own
  words, and the reset dialog closes at the moment of intent.
- A workspace switch tears down before it swaps identity: caches clear
  into a loading state, the overview navigation happens first (taking
  the old page's pollers with it), and every async loader carries a
  session epoch so a late response from the previous workspace can
  never write into the new one. A failed switch rolls the token back.
- The default request deadline rose to 60s: a production mirror
  mid-copy answered /tables in just over 30s and the abort turned a
  slow-but-working control plane into an error banner.

## [0.0.4-rc6] - 2026-08-20

The "whole flow stuck" report, run to ground: four bugs in one causal
chain, each fixed at its own layer, plus the operator's reset escape
hatch.

### Added

- `POST /databases/{id}/reset` and a confirmed **Reset mirror** action in
  database settings: clears every tracked table (cascading to snapshot
  chunks, schema history and poll state), the replication checkpoint,
  quarantined events and the on-disk stores - holding the job slot
  through the wipe - then re-probes with the saved connection and copies
  everything fresh, continuing in the configured mode. Nothing about the
  connection is asked again.
- e2e: a schema-drift check reproducing the reported flow end to end
  (pause, DROP COLUMN at the source, purge the binlogs holding the DDL,
  resume, resync - asserting byte-identical convergence AND a live
  stream afterwards), and a full reset-lifecycle check. The gate is now
  1,828 checks.

### Fixed

- Resuming a paused database to `auto` no longer unschedules it forever.
  Two supervisor gates conspired: switching to `auto` clears
  `effective_mode` for a recomputation nothing ever ran, and the pause
  wrote `state='paused'` which the resume never rewrote - so the
  supervisor skipped the database every cadence while the badge kept
  saying streaming. A resumed database is now scheduled, the cycle
  derives cdc/polling the way the snapshot handoff would, and the first
  successful cycle re-persists both.
- Repair paths copy the source as it IS, not as it was probed. A source
  migrated while nothing was streaming - with the binlog holding the DDL
  purged before the stream returned - left every copy path SELECTing the
  remembered column list and dying on the source's own ERROR 1054
  "Unknown column", forever. The per-table resync and reconcile now
  re-probe first and persist the fresh report; the CDC auto-resnapshot
  re-probes before recopying its targets, evolving each store and
  recording the schema version the way the DDL path does.
- Schema history that cannot bridge off-stream drift no longer wedges
  the copy on a fingerprint mismatch: history is only written by DDL
  events, so the store-open shared by the copy paths adopts a fresh
  probe as a new schema version, or rebuilds the store outright when no
  history record exists - only for callers about to recopy the table
  wholesale. A resumable first snapshot and reconcile stay strict.

## [0.0.4-rc5] - 2026-08-19

Restart-safe table copies and a two-pass dashboard audit (data layer,
then every page) with the findings fixed and gated.

### Added

- Copy progress survives a reload: the server retains the last progress
  frame per table - cleared by the same completion, error and
  interrupted events that clear the live view - and `/tables` returns it
  as `elapsed_seconds`, so a dashboard opened mid-copy draws the bar
  immediately. The browser gate reloads mid-copy and asserts it.
- The per-table resnapshot renders a live progress bar with row count
  and ETA on the database page.
- Destructive one-click actions - deleting an API key, removing a
  member, discarding a dead letter - now confirm before acting; each was
  one mis-click from irreversible loss.
- Backup history refreshes itself while a run is live and announces
  completion or failure, instead of showing "running" until a manual
  refresh.

### Fixed

- A restart during a table copy no longer leaves the table answering as
  healthy with partial rows: tables still marked `snapshotting` at boot
  are quarantined to `needs_resync` with the reason recorded, and only
  the job that is copying a table may declare it done.
- A 409 on the job slot names the job that holds it and for how long,
  instead of the generic "already active".
- Dashboard data layer: every mutation now retries the supervisor's
  busy window and toasts failures (seven actions previously failed
  silently); the event and vitals streams reconnect with backoff; a
  mid-session 401 signs the operator out instead of freezing the
  dashboard on stale data; one unreachable database no longer freezes
  every other database's status; a failed workspace switch rolls the
  token back rather than stranding the operator signed out of both.
- Dashboard pages: the connection wizard no longer dead-ends on a
  spinner when starting the mirror fails; the delete-database dialog
  stays open on failure instead of closing exactly like a success;
  Resnapshot navigates to the snapshot tab only when the request
  succeeded; refreshing backup history no longer discards unsaved
  configuration edits (including a typed secret key); restore gets a
  deadline that outlives large restores; clipboard failures on
  show-once secrets toast instead of losing them silently; CSV export
  neutralizes spreadsheet formula injection; the SQL console's
  Cmd-Enter can no longer race two concurrent queries.

## [0.0.4-rc4] - 2026-08-19

Operability follow-ups from the customer's 19/19 parity run and the
resnapshot-responsiveness report.

### Added

- Row-constructor IN: `(a, b, c) IN ((...), (...))` - the natural
  predicate for composite-key tables - desugars to exact OR-of-AND
  equalities. Verified against MySQL 8.4 and pinned by a twelve-case
  oracle family (1,015 cases).
- `SELECT VERSION()` reports the deployed release
  (`8.4.0-pintail-<tag>` via the compose-provided build version), so a
  deployment identifies its build on the wire.
- A per-table resnapshot publishes progress events like the full
  snapshot; the dashboard animates through long copies instead of
  sitting on a motionless badge.

### Fixed

- The dashboard's Resync button retries the supervisor's busy window
  (the job lock is held through every replication cycle, so clicks
  frequently landed on a 409 that was swallowed silently) and failures
  now toast with the reason.

## [0.0.4-rc3] - 2026-08-19

A day of differential hunting: the oracle corpus grew to 1,003 cases,
three public datasets (sakila, employees at 2.8M rows, world) now
byte-diff against MySQL, and everything they caught is fixed.

### Fixed

- ENUM comparison corrected against real MySQL 8.4: ranges, BETWEEN and
  MIN/MAX compare the label STRING; only sorting walks the declared
  ordinal. rc2 shipped ordinal comparison everywhere, which real data
  refuted the same day.
- The ENUM ordinal now survives the server's remaining paths: memtable
  rows (fresh CDC writes) and repacked projection/aggregate batches
  both rebuilt plain strings and sorted alphabetically.
- A SET sorts by its member bitmask, as MySQL does; MIN/MAX and
  comparisons keep string semantics (measured).
- GEOMETRY replicates byte-for-byte: the poll path stripped a
  4-byte header from already-canonical values, and the intentional
  SRID canonicalization itself broke parity with what a MySQL client
  reads. Geometry now flows as MySQL's raw internal bytes end to end,
  the checksum hashes the same bytes it stores, and the wire advertises
  MYSQL_TYPE_GEOMETRY so drivers decode the column as MySQL's.
  Deployments upgrading across this fix should per-table resync
  geometry-bearing tables.
- Three wrong-results grouping defects, all found differentially: the
  fused inner-join aggregate emitted zero-count groups for unmatched
  build rows; two separate group finalizes kept only the LAST group
  when spellings folded to one collation key, silently dropping every
  earlier group's aggregates; and the local matcher's ASCII fast path
  ignored PAD SPACE so trailing-space spellings never folded at all.
- The fused join aggregate folds its group keys under the KEY's
  collation rather than the plan's.

### Known limitations

- A CASE/IF branch value renders at the unified DECIMAL scale
  (`0.00` where MySQL prints `0`); numerically equal, documented in
  docs/limitations.md.

## [0.0.4-rc2] - 2026-08-19

### Fixed

- An ENUM now follows the split MySQL actually implements, confirmed
  differentially against MySQL 8.4: SORTING - ORDER BY in both
  directions, grouped tie-breaks, DISTINCT, limited sorts, and window
  ordering - walks the declared ordinal, while COMPARISON - range
  predicates, BETWEEN, MIN/MAX - treats the value as its label string.
  Every one of those surfaces previously sorted alphabetically: the
  columnar batch path, the memtable row path (fresh CDC rows), and
  repacked projection/aggregate batches all rebuilt plain strings and
  erased the declaration index.
- Grouping keys of two text collations now answers instead of refusing:
  each key folds under its own collation - grouping never compares one
  key column against another - exactly as sorting already ordered each
  key by its own rules. Reported by a customer grouping a section name
  next to a school name.
- A distinct aggregate folds its values under its own expression's
  collation, not the query's: COUNT(DISTINCT general_ci_col) PAD-folds
  trailing spaces even when the rest of the query resolved 0900_ai_ci.
- The supervisor says why the CDC handoff rebuild is waiting (a
  resync.retry event naming the error) instead of retrying silently,
  so a database that pauses after a polling-to-cdc switch diagnoses
  itself in the event log.

## [0.0.4-rc1] - 2026-08-18

Two findings from the customer's re-check of 0.0.3, one of them a silent
wrong-results bug that blocks reading through Pintail at all.

### Fixed

- A table joined twice under two aliases returned the first alias's row
  for both. Physically the two inputs share database, table and column
  ids, and the expression compiler resolved a column by those alone and
  took the first match - so `u2.name` silently became `u1.name`. No
  error, entirely plausible values, and wrong: on one staging table 605
  of 4067 rows attributed an activity to the wrong person. The relation
  name is now part of a column's identity during resolution. The defect
  predates 0.0.3 - it became visible only once the join fixes let
  `created_by`/`updated_by` alias pairs run at all.
- Refusing to group keys of two text collations now names both
  collations. The refusal itself is unchanged, but it fired for a
  customer on two columns their schema declares identically, and Pintail
  exposes a column's collation nowhere else - the message was the only
  place the disagreement could ever be seen.
- A replication cycle finishing after a concurrent mode switch no longer
  reverts the switch. The cycle's completion wrote back the effective
  mode it started under, so a polling cycle straddling a polling-to-cdc
  switch flipped the database back to polling while its requested mode
  said cdc - the CDC handoff rebuild (keyed on the effective mode) then
  never fired, and the database kept polling indefinitely, never
  adopting tables created after the switch. The completion write is now
  a compare-and-set against the requested mode; a stale cycle loses.

### Known limitations

- The underlying collation disagreement - the engine believing two
  identically-declared columns differ - is diagnosable now but not yet
  explained. Grouping those two columns still rejects.

## [0.0.3] - 2026-08-18

Every finding from a customer conformance report against their own schema:
eight of nineteen dashboard queries were rejected outright and their
connection string could not be registered. All eight now run. Verified by
replaying their schema and data locally and running their own validation
harness, which moves from 9/19 to 18/19 identical results.

### Fixed

- A join whose `ON` clause compares the two inputs with something other
  than equality no longer rejects. The hash join keys on the equality and
  tests the remaining conjuncts against each candidate pair, so
  `ON a.id = b.id AND b.at >= COALESCE(a.from, '1900-01-01')` runs. The
  residual filters the match bucket rather than the join's output, which
  preserves outer semantics: a left row whose every candidate fails is
  NULL-extended, as MySQL does, where moving the predicate into `WHERE`
  drops it. Five queries.
- `ORDER BY` accepts an expression, over aggregates in a grouped query
  and over plain columns in an ungrouped one, carried as a hidden sort
  column. An aggregate appearing only in `ORDER BY` is computed rather
  than dangling. Three queries - the third found by running the
  customer's harness rather than re-reading their report, which had
  reported only the aggregate form.
- A correlated scalar subquery in a grouped select is accepted when it
  correlates only on grouping keys, where it has exactly one value per
  group. Correlation keys are matched by physical column identity, since
  `GROUP BY` binds after the decorrelated table joins and two bindings of
  one column need not be structurally equal. A subquery correlating on
  anything else still refuses: returning an arbitrary value per group
  would be silently wrong.
- A source connection string carrying client-driver parameters
  registers. `multipleStatements` and `dateStrings` configure a driver's
  own decoding, but made the whole URL unparseable. Only names known to
  be client-side are dropped; anything else unrecognised still fails, so
  a misspelled `require_ssl` cannot silently connect in plaintext.
- A forced snapshot no longer swallows DDL. It read the stored probe and
  handed the stream a position captured after it, so a table created in
  that window was never copied and never adopted - the stream kept
  reporting healthy with one fewer table, permanently.
- A connection's preamble no longer moves the resume point. The format
  description sits at the head of the file, and adopting its position
  rewound an idle cycle's resume point to the start of the binlog.

### Changed

- One query may use 512MiB by default rather than 64MiB. A nine-way
  dashboard join over a four-thousand-row table was refused at the old
  ceiling. The per-query limit never bounded the process - the shared
  concurrent total does, and still defaults to three quarters of host
  memory - and operators spill rather than fail above it, so this trades
  resident memory for fewer spills on the queries an analytical replica
  exists to serve. `docker-compose.yml` now names the per-query knob
  beside the total.

### Added

- `SELECT /*+ MAX_EXECUTION_TIME(5000) */ ...` is honoured. The session
  variable already produced a real deadline; the inline form MySQL
  documents rejected along with every other optimizer hint. The hint
  tightens the effective deadline and never loosens it, so it cannot be
  used to write around an administrator's limit. Hints Pintail does not
  implement still reject rather than being silently ignored.
- Replication and query telemetry that names what was previously
  invisible: what a query spent before planning, what a CDC cycle read
  and committed, and why a schema-drift heal declined.

### Known limitations

- `ORDER BY` on an `ENUM` sorts by label rather than by declared
  ordinal, which is not MySQL's order.

## [0.0.2] - 2026-08-18

Dashboard only. No engine, replication or storage changes.

### Added

- A **View** action on every row of a database's tables list opens the
  first 100 rows in a dialog, read through the query engine rather than a
  separate path - so merge-on-read visibility, typed fields, NULL
  rendering and value formatting are identical to the SQL console, and a
  footer link carries the table into that console for anything deeper.

### Fixed

- Switching workspaces flashed the connection wizard at operators whose
  databases were still loading. The switch clears the database cache
  before it reloads, and the empty states keyed on the cache alone, so
  "No databases yet" rendered for the width of two round trips - which
  reads as data loss, not as loading. An empty workspace still reaches
  the wizard; a populated one no longer passes through it.
- A long replication error stretched its column until the Reconcile and
  Resync buttons left the screen. Table errors truncate with the full
  text on hover, and dead-letter errors wrap instead of running past the
  viewport.
- Resync no longer jumps the view from the Tables tab to Snapshot. The
  action is requested from the tables list, and the operator is usually
  still reading it.
- The Resync button described itself as a mirror-wide resnapshot, which
  0.0.1 had already made per-table. The tooltip and toast now say what it
  does - the old warning was the one most likely to talk an operator out
  of the cheap repair.

## [0.0.1] - 2026-08-18

First stable tag. Folds in the performance work that was headed for an
rc15 that never shipped, together with the replication hardening that
followed it and made this the release instead.

### Fixed

- A schema change that never reaches the stream as DDL no longer costs the
  table a full resnapshot. This is the shape of a real outage: a
  hand-written `ALTER TABLE ... ADD COLUMN` on the source, no DDL in the
  stream, and every subsequent row image refused as one column wider than
  the probed schema - three days of dropped rows. The stream now treats an
  unplaceable row image as the signal to re-probe and adopts the refreshed
  schema in place when it is storage-compatible, exactly as if the
  statement had been seen. Under `binlog_row_metadata=FULL` a lagging image
  is placed by the column names the table map carries; under MINIMAL, which
  names nothing, it is placed by its column-type sequence when exactly one
  placement exists, and refused - never guessed - when more than one does.
  Both regimes are stress-tested end to end, including four invisible
  widenings under live traffic and an invisible DROP COLUMN.
- Re-probing a live database silently stopped its replication for good.
  `probed` is an onboarding state, and writing it over `streaming` removed
  the database from the supervisor's schedule with every table still
  reporting healthy. A probe of a replicating database is now an inventory
  refresh and leaves the lifecycle state alone.
- Switching a database from polling back to CDC only ever worked by
  accident. Every polling cycle overwrites the shared source checkpoint
  with one CDC cannot start from, and the old whole-database resync
  happened to rebuild it as a side effect. The supervisor now schedules
  that rebuild deliberately, so the transition heals without an operator
  knowing the checkpoint semantics.
- DDL is routed by the schema the statement names, not by the bare table
  name. `DROP TABLE other_db.t` from a session in the tracked schema
  orphaned the tracked `t`, and `CREATE TABLE other_db.t` errored the
  stream without advancing the checkpoint - retrying forever. Foreign-
  qualified names now produce no action, and an explicit tracked qualifier
  is honored even from a session sitting in another schema.

- The benchmark survives a ClickHouse crash mid-run. The container had no
  restart policy and the retry fired immediately, so a crashed server
  guaranteed ConnectionRefused and lost the whole stage; two runs died that
  way in one day. The container now restarts, the retry waits for the server
  to answer, and the container tail is captured at the moment of the drop.
- Buffered batches are sized to what the query can still afford, which is
  what unblocked raising the batch target to 65,536 rows: the aggregate's
  spill path could not retry mid-merge and failed at every size above 4,096.
- Nine findings from an external review, and a join inference that could
  admit an unsafe equality.

### Added

- `POST /databases/{id}/tables/{name}/resync` recopies one table instead of
  resnapshotting the whole database. The recopied table gets its own binlog
  fence - the same mechanism that protects a table auto-included
  mid-stream - so the other tables keep replicating untouched. On a large
  source this is the difference between minutes and hours to repair one
  table.
- The replication log says what happened when something declines: a drift
  heal that refuses states its reason, a quarantined table states the
  decode error that condemned it, and every clean CDC cycle records the
  events it read and the position it reached - a cycle that reads nothing
  while the binlog grows is a wedge, and it used to be invisible.
- The e2e gate covers destructive lifecycle shapes in both replication
  modes: a table dropped under CDC, dropped and recreated under the same
  name, dropped under polling (including recovery by re-probe), and a
  second registered database whose source is dropped outright.

- `GROUP BY` and `WHERE` can express a join the way SQL-89 does: equality
  predicates between two relations in a `WHERE` clause are inferred as join
  conditions, so `FROM a, b WHERE a.id = b.a_id` plans as a hash join
  instead of a cross product. Inference is refused for anything not provably
  side-separable, and for volatile expressions.
- The validation pipeline fails when banked evidence predates the code it
  measures. Benchmark results, TPC-H results, the production workload and
  the e2e gate are all checked by commit ancestry, because a release once
  shipped a README table describing an earlier run and nothing caught it.
- A TPC-H-derived correctness workload covering four query shapes the
  analytical suite lacks - multi-way joins, top-N over a join,
  high-cardinality join grouping - each verified byte-exact against MySQL.
  It is a correctness gate, not a performance benchmark, and its artifact
  now says so.
- The row-count probe counts exactly, abandoning a count that exceeds thirty
  seconds and falling back to statistics rather than hanging the caller.
- The scan pool's width is settable.

### Performance

The cache-disabled track - every query measured with the result memo off,
which is the like-for-like comparison against ClickHouse - now runs at a
geometric mean of 0.55x ClickHouse's MergeTree over the eight analytical
queries. Q4 (region x status breakdown) is FASTER than ClickHouse at 1.35x,
190ms against 251ms, and Q1 (full table count) is at parity. Q5 fell from
274ms to 161ms across this window and Q3 from 284ms to 189ms.

One measurement caveat is load-bearing and belongs next to those numbers:
this release's benchmark is the first run on a dedicated host. Every previous
bank shared a machine with a live deployment, which suppressed the older
figures by an amount nobody had quantified, so the improvement against
earlier releases is real but smaller than the raw geomeans suggest. Only
same-run Pintail/ClickHouse ratios are comparable, as benchmark/README.md
has always said.

The gains came from removing work rather than computing faster, which is
worth recording because the opposite was tried repeatedly and measured at
nothing:

- Bit-packed integer blocks decode in a single streaming pass. The old path
  built a zeroed sixteen-byte window per value and converted through u128,
  then a second per-row loop re-read the temporary vector, re-checked
  overflow and dispatched every value through a match. Two passes and five
  layers for one add and one store.
- A decoded chunk passes through as one batch. Segments decode as
  100,000-row chunks against a 65,536-row batch target, so every chunk was
  split and the remainder copied out of every column - about 110MB of pure
  reshaping per 20M-row query.
- Comparison masks fill in parallel. The WHERE clause ran its comparison
  loop on one thread: forty million date comparisons while fifteen cores
  idled, 35ms of a 118ms query.
- A column with no nulls carries a count instead of a byte per row, end to
  end from the segment builder to the executor's mask. The typed adoption
  phase fell from 14ms to 1.4ms.
- Date-part groups accumulate in dense slots instead of being buffered into
  partition buckets and read back, and both parts of a two-part key come
  from one civil-calendar conversion.
- Decimal units keep the width the store emits. They were widened to i128 on
  adoption and narrowed straight back to i64 by the aggregate lane that
  reads them - 320MB allocated and copied per pass between two points that
  both wanted 64 bits.
- Each aggregation worker gets several hash partitions rather than one, so a
  partition's map fits a core's private cache, and the scan decodes one
  segment per scan-pool thread rather than a hardcoded eight.

### Known limitations

- Under MINIMAL metadata a stream lagging more than one schema change, with
  no unique type placement, is flagged for resync rather than guessed at.
- A dropped source database is surfaced - loud connection errors, database
  state `error` - but not modelled; retained rows keep serving until an
  operator acts. `docs/limitations.md` records both.

## [0.0.1-rc14] - 2026-08-13

### Added

- `GROUP BY` accepts an ordinal and `HAVING` accepts a projection alias, both
  of which MySQL has always allowed and neither of which this engine could
  bind. A dashboard that generated `GROUP BY 1` - which many do, because it
  survives renaming the column - was refused outright.

### Fixed

- A long-running server no longer refuses every query eventually. The
  process-wide memory budget is shared, finite, and nothing refills it, so a
  query that returned less than it borrowed walked the balance in one
  direction until nothing could be admitted: about 1,500 queries into a
  30-minute benchmark phase, while replication carried on looking healthy and
  the logs said nothing. Borrowings are now repaid when the tracker is
  dropped, on every path including the error ones, and a clone inherits what
  the query is holding without inheriting the debt - which is what stops two
  trackers from repaying one borrowing twice and walking the balance the other
  way, into a limit that no longer limits.
- `HAVING` resolves a projection alias ahead of a source column of the same
  name. This was measured against MySQL 8.4 rather than reasoned about: the
  conservative reading - that a real column should outrank an alias - was
  implemented first, tested against the server, and found to be wrong.
- The fused join-aggregate declines the query when the build side spilled. A
  build side that outgrows the memory ceiling is drained into grace partitions
  and its resident map left empty; the fused path read that map directly and
  would have answered with silence rather than an error. No query is known
  that reaches it - every candidate tried resolves its group columns to the
  probe side and declines earlier - but the failure mode is a wrong answer, so
  it is guarded regardless.

### Changed

- The join answers roughly 10% faster with the result memo disabled, from two
  measured changes rather than a rewrite. Resolving which group a build row
  belongs to was generating a full collation sort key per row - 250,000 of
  them for a column holding eight distinct values - and those keys are now
  memoized by their text, which took ICU from 12.6% of the profile to 0.3%.
  The plan's two byte-keyed maps also hashed with SipHash, whose resistance to
  attacker-chosen keys buys nothing for data the query itself just produced.
  Dictionary-encoding the build side was tried for the same gap and measured
  15-30% SLOWER; it is recorded in `docs/decisions.md` as a dead end rather
  than left as an open direction.
- The benchmark measures throughput and p95 under concurrent clients, and
  ships a TPC-H workload alongside the commerce one, so the join numbers can
  be read against a recognised suite rather than only against our own.

## [0.0.1-rc13] - 2026-08-13

### Added

- `utf8mb4_general_ci` is a collation queries can use, not merely one the
  replica can store. It is MySQL 5.x's default and a table keeps whatever
  collation it was created with, so supporting only MySQL 8's default meant a
  source could snapshot, replicate and read back while every `WHERE`, `JOIN`,
  `GROUP BY` and `ORDER BY` on its text was refused. The weight table was
  extracted from a live server with `WEIGHT_STRING()` rather than transcribed,
  and the collation is reproduced as it behaves rather than as it ought to: it
  is PAD SPACE, so `''` equals `' '` and trailing spaces do not count; and
  every character above the BMP weighs the same, so all of them compare equal
  to each other. Both are real MySQL behaviour, verified differentially, and
  implementing something more sensible would be a parity bug.
- Query logging on the MySQL wire. A connection through that protocol recorded
  nothing about who opened it or what they ran, so the one surface accepting
  arbitrary SQL was the one with no audit trail. Statements are digested with
  literals replaced before they are stored, so the trail says what shape of
  query ran without becoming a copy of the data it read.
- An admin can change a member's role. A workspace could grant a role at invite
  time and revoke it by removing the member, but never move one between them,
  so promoting a teammate cost them their audit trail. Nobody may change their
  own role, which is what keeps a workspace administrable: no sequence of calls
  can leave it with nobody able to make the next change.

### Fixed

- Replication survives `ALTER TABLE ... CONVERT TO CHARACTER SET`. The SQL
  parser cannot represent that statement, so schema tracking returned a hard
  error and stopped the stream on DDL the source had already applied - and it
  is exactly the statement an operator runs to move a table onto a collation
  this engine can compare, so approaching a supported schema was what broke
  replication. It is now recognised ahead of the parser and treated as
  metadata-only: stored values are decoded characters rather than source bytes,
  so re-encoding a column leaves the logical value identical and only the
  collation changes.
- Text collation resolves per comparison rather than per query. A query reading
  two collations was refused outright even when every comparison inside it was
  internally consistent - a `general_ci` filter beside a `0900_ai_ci` ordering,
  which MySQL answers and which a schema part-way through a collation migration
  produces constantly. One comparison spanning two collations is still refused,
  because that is genuinely undecidable without coercibility rules.
- The wire endpoint no longer stops answering. Every await before
  authentication was unbounded, so a peer that opened a socket and vanished
  without closing it parked its task forever - what a firewall leaves behind
  when it drops an idle flow. Each stalled task pinned two descriptors, and the
  accept loop propagated every error, so exhaustion killed it and left a
  listening socket nobody was accepting on: connections were neither served nor
  refused, and the server logged nothing.

### Changed

- The benchmark reports engine speed separately from cache latency. The
  headline compared pintail answering from its result memo against ClickHouse
  executing, which measured one engine's cache against the other's execution.
  The same queries now also run with the memo disabled, on the same replica,
  and that table shows ClickHouse ahead - published because it is the honest
  measure of execution performance.
- The benchmark gate fails on a query that errors, is unsupported, or
  disagrees with MySQL. It recorded such outcomes and still exited zero, so a
  run where a quarter of the workload never executed could report success.
  Gaps declared before a run warn; anything else fails.

## [0.0.1-rc12] - 2026-08-12

### Fixed

- Replication survives `ALTER TABLE ... CONVERT TO CHARACTER SET`. The SQL
  parser cannot represent that statement, so schema tracking returned a hard
  error and stopped the stream on DDL the source had already accepted and
  applied — and it is exactly the statement an operator runs to move a table
  onto a collation this engine can compare, so the remedy for one collation
  problem triggered an outage through another. It is now recognised ahead of
  the parser and treated as metadata-only: stored values are decoded
  characters rather than source bytes, so re-encoding a column between
  character sets leaves the logical value identical and only the collation
  changes, which the re-probe adopts. A narrowing conversion MySQL cannot
  perform losslessly does change values and still needs a resnapshot, which is
  recorded as a limitation rather than guessed at from the statement text.
- The wire endpoint no longer stops answering. Every await before
  authentication was unbounded, so a peer that opened a socket and then
  vanished without closing it — what a firewall or NAT leaves behind when it
  drops an idle flow — parked its task forever on a read that never returned,
  holding the socket and the disconnect watch's dup of it. The idle timeout did
  not cover this, because it only wraps the serving loop a connection reaches
  after it authenticates. Meanwhile the accept loop propagated every error, so
  the first failure ended it and took the endpoint with it. Together the
  half-open sockets exhausted the descriptors and the accept loop died, leaving
  a listening socket nobody was accepting on: new connections were neither
  served nor refused, they sat in the backlog until the client's own deadline
  expired, and the server logged nothing because from its side nothing had
  happened. The handshake now has a thirty-second deadline and accept failures
  are logged and retried.

### Added

- `utf8mb4_general_ci` in the executor. It is MySQL 5.x's default, and a table
  keeps whatever collation it was created with, so supporting only MySQL 8's
  default meant a source could snapshot, replicate and read back while every
  `WHERE`, `JOIN`, `GROUP BY` and `ORDER BY` on its text columns was refused.
  The weight table was extracted from a live server with `WEIGHT_STRING()`
  rather than transcribed, and the collation is reproduced as it actually
  behaves rather than as it ought to: it is PAD SPACE, so trailing spaces are
  insignificant and `''` equals `' '`; and every character above the BMP weighs
  the same, so all of them compare equal to each other. Both are real MySQL
  behaviour, verified differentially, and implementing something more sensible
  would be a parity bug. The probe now also names any column whose collation
  the executor cannot compare, at probe time rather than at first query.
- Query logging on the wire. A connection through the MySQL protocol recorded
  nothing about who opened it or what they ran, so the one surface that
  accepts arbitrary SQL was the one with no audit trail. Statements are
  digested with literals replaced before they are stored, so the trail says
  what shape of query ran without becoming a copy of the data it read.
- An admin can change a member's role. A workspace could grant a role at
  invite time and revoke it by removing the member, but never move one between
  them: promoting a teammate meant removing them and re-inviting, which cost
  them their audit trail. Nobody may change their own role, which is what keeps
  a workspace administrable — no sequence of calls can leave it with nobody
  able to make the next change.

### Changed

- The differential gate carries a second collation. Every text column in its
  source was one the executor already compared, so the whole class of
  divergence above was invisible to it. It now exercises case folding, accent
  folding, PAD SPACE and the supplementary-plane collapse against a live MySQL
  every release, and converts a table's character set mid-stream to prove the
  schema change is survived. A documented gap now warns when the engine refuses
  a query, not only when it answers differently, so a gap can be recorded
  before it is fixed — which is what makes the fix verifiable rather than
  asserted.

## [0.0.1-rc11] - 2026-08-12

### Added

- The node issues and manages its own wire-protocol TLS certificate. Without
  one the server never advertised `CLIENT_SSL`, so a client that would have
  preferred TLS got plaintext and had no way to ask for better — the state a
  published port starts in. It is generated on first boot, kept across
  restarts, and reissued only when the names it covers change, since rewriting
  it invalidates whatever clients have pinned. Clients now get TLS through
  their own `PREFERRED` default with nothing configured, which is what
  actually protects users of managed database services: measured against
  DigitalOcean, a connection with no SSL flags negotiates TLSv1.3 while
  `--ssl-mode=DISABLED` still connects. One certificate covers the node
  because the TLS upgrade completes before the client sends its username, and
  here the username is the database name — so a per-database certificate
  cannot exist.
- The certificate is downloadable from Connect, and its hostnames are set in
  Settings, defaulting to the host of the public URL already configured for
  Google sign-in. Downloading it upgrades a connection from encrypted to
  verified; the hostnames are what make `VERIFY_IDENTITY` possible rather than
  only `VERIFY_CA`.
- Live CPU, memory and query-rate charts on the overview, streamed at one
  sample per second over SSE. CPU was not collected at all before, and memory
  was sampled by spawning `ps` — tolerable per Prometheus scrape, and not at
  1 Hz, where it would fork the process 86,400 times a day. Both read `/proc`
  on Linux, and both are measured against the cgroup limits rather than the
  host's totals, so a container capped at 4GB reads as busy at 3.5GB instead
  of using 5% of the machine.

- Remote diagnostics: crashes and errors to Sentry, every log line to Logtail
  (Better Stack). Both are spoken directly over their HTTP APIs rather than
  through an SDK, for the same reason `pintail-log` has no dependencies at all.
  A panic captures a backtrace — with `force_capture`, since `RUST_BACKTRACE`
  is unset in production and a crash report without a stack is the reason this
  exists — parses it into Sentry frames, and blocks the panicking thread until
  it has been delivered or five seconds pass. Logging never blocks: lines go
  into a bounded queue and are dropped and counted when it is full, because a
  replication loop stalling behind a slow log endpoint is worse than a missing
  line. Configured by `PINTAIL_SENTRY_DSN`, `PINTAIL_LOGTAIL_ENDPOINT`,
  `PINTAIL_LOGTAIL_TOKEN`, and optionally `PINTAIL_ENVIRONMENT` and
  `PINTAIL_RELEASE`. Entirely inert when unset.

### Changed

- The dashboard matches shadcn's default sizing. It was generated from the
  `reka-mira` style, about one step smaller throughout — `h-7` buttons with
  12px text where the default is `h-9` with 14px — with hand-written values
  between 8.8px and 10.1px layered on top. 28 components move to 14px; badges,
  tooltips, menu shortcuts and sidebar group labels stay at 12px because
  shadcn keeps them there.
- A refused Google sign-in names the address it refused, in the server log.
  Four different situations reach the browser as the single message "not
  invited" — no invite for that address at all, or one that is already
  accepted, revoked or expired — and none of them said which address Google
  had returned, so a report of "the invite does not work" could not be
  resolved without guessing. The log now distinguishes the four and records
  the address; the case where an account already exists without a linked
  Google identity is logged the same way. The browser message additionally
  points at the likeliest cause, that the account chosen at the Google consent
  screen is not the one the invite was addressed to.
- The sign-in gate reads the server's log while the run is in progress, so the
  two refusal checks assert the diagnostic line exists and names the account
  rather than only that the browser was refused.

- The dashboard has a 12px floor on type. Seventy declarations rendered below
  10.5px — nine arbitrary sizes between 8.8px and 10.1px, which is drift rather
  than a scale — and the worst of them were mono, uppercase and letter-spaced,
  each of which costs legibility on top of the size. They sat on operational
  data: driver, host and port, database and user, session subject. Every status
  badge was 9.1px, set with `!important` in one rule. All of it now sits at
  `text-xs` or above, which is the floor iOS and Material both put body text at
  and above what any of these were. Badge and small-button heights grew from
  20px to 24px so the larger text is not clipped.

## [0.0.1-rc10] - 2026-08-11

### Fixed

- An invite is redeemed by the link that was opened. The token reached only
  the public status lookup; the sign-in it started carried nothing, so
  admission was resolved by searching every invite for whatever address Google
  returned. An existing Google identity was matched by subject and returned
  before invites were consulted at all, which stranded anyone the pre-atomic
  admission had left with a user row and no membership — refused for belonging
  to no workspace, reported as "not invited", and unreachable by any number of
  fresh invites. The same silence swallowed second-workspace invites. The
  invite id now travels in the signed OAuth state (never the token, which is a
  bearer credential) and the callback claims that exact invite, for an existing
  user or a new one.
- Sign-in no longer picks the newest invite across the node. The address search
  chose the most recent claimable invite in any workspace, so an admin of any
  workspace could aim a newer, higher-privileged invite at an address and
  capture whoever followed a legitimate invite elsewhere. It survives only as a
  fallback for visitors who reach the login page directly, and refuses when
  more than one invite is open rather than guessing.
- Authorization is re-read per request instead of trusted from the token.
  Removing a member, demoting one or disabling an account changed nothing until
  the token expired up to twelve hours later — and a removed admin could mint
  fresh admin invites to the workspace they had been removed from, renewing the
  access indefinitely.
- Invite expiry is checked inside the claiming transaction, alongside accepted,
  revoked, email, workspace and role, so an invite cannot be consumed after
  expiring while it waited on the write lock. The status endpoint and the
  callback also disagreed about a timestamp that will not parse — the invite
  page called it valid, the sign-in called it expired — and both now fail
  closed, as does the status shown on the team page.
- A callback must prove it belongs to this browser's sign-in before anything in
  it is acted on. The provider-error branch returned before state was verified
  while the handler still cleared the state cookie, so anyone able to trigger a
  top-level GET could cancel a sign-in in progress. Provider-supplied error
  text is escaped before logging, since a percent-encoded newline could forge
  log lines.
- Invite addresses that no sign-in could ever match are refused at creation.
  The check was "not empty, and contains an @", which admitted internal spaces,
  zero-width characters and second @ signs — producing an invite that looked
  entirely ordinary while its holder was refused forever.

### Changed

- A refused sign-in says which refusal it was. Accepted, revoked and expired
  invites, several open invites, and an account belonging to no workspace all
  arrived as "you were not invited", which is misleading when the invite exists
  and each case needs a different action.
- Refusal logging names the address only for people already recorded here. An
  address with no account and no invite leaves only its domain, which is enough
  to spot an organization pointed at the wrong node without collecting the
  mailbox of anyone who merely pressed the button.
- The sign-in gate covers the admission paths that shipped unguarded: second
  workspace and orphan repair, immediate session revocation, contested invites,
  a revoked invite, and a forged callback. 16 checks, each verified to fail
  without the fix it guards.

## [0.0.1-rc9] - 2026-08-11

### Fixed

- Signing in with Google works. The dashboard is prerendered, so the cold load
  of `/?auth_code=...` that Google redirects back to hydrates against the
  payload for the query-less `/` route; while that resolves the router rewrites
  the address bar and restores it only afterwards. `app.vue` read the code
  inside that window, where both `route.query` and `window.location.search` are
  empty, so the code was never exchanged. Anyone who had just authenticated —
  including an invitee whose account, workspace membership and consumed invite
  were already committed on the server — was returned to the login form with no
  error shown. The same blind spot swallowed `?auth_error=`, so every refusal
  was silent too: `not_invited`, `link_required` and a disabled account all
  looked identical to nothing happening. The result is read once the router has
  settled, and the spent code is stripped from the address bar afterwards so a
  reload cannot replay it.

### Added

- A sign-in gate (`tests/browser/auth.ts`, 11 checks) drives the invite and
  "Continue with Google" paths end to end in a real browser, against a
  stand-in for Google's authorize, token and userinfo endpoints with
  single-use codes. It covers an invitee joining, the membership granted and
  the invite spent, a returning identity matching on its Google subject, and
  refusal of an uninvited account, an address that already has a password
  account, and an unverified Google email. It needs neither MySQL nor an
  object store, so it runs without Docker in seconds; the smoke suite keeps
  the replication coverage. This path had no browser coverage at all despite
  being the only way anyone joins a workspace, which is why three consecutive
  releases shipped it broken.
- The three Google OAuth endpoints can be pointed at another origin through
  `PINTAIL_GOOGLE_AUTH_URL`, `PINTAIL_GOOGLE_TOKEN_URL` and
  `PINTAIL_GOOGLE_USERINFO_URL`, which is what makes the gate above possible.
  They are read from the process environment rather than stored settings, so
  nothing reachable through the dashboard or the settings API can redirect
  sign-in elsewhere.

### Known limitations

- Accounts left half-created by the pre-rc8 behaviour are still not repaired.
  They need the workspace membership added, or the user row removed so a fresh
  invite can admit them.
- The invite page does not carry its token into the Google flow. Admission is
  resolved from whichever address Google returns, so an invitee who picks a
  Google account other than the invited one is refused as `not_invited`. That
  refusal is at least visible now rather than silent, but the mismatch is easy
  to hit because the consent screen asks which account to use.
- Duplicate callbacks remain non-idempotent, and their cause is still unknown.

## [0.0.1-rc8] - 2026-08-11

### Fixed

- An invited Google identity is admitted in one transaction. Creating the
  user, granting the workspace membership and consuming the invite were three
  separate writes, so a failure between the first and the second left an
  account that could never sign in again: the user row exists, so every later
  attempt skips the invite path, but no membership exists, so it is refused
  for belonging to no workspace. If the third write had also landed the invite
  was spent too, leaving no route back in through the UI.
- The invite is claimed as a compare-and-set. The first version of the guard
  checked `accepted_at IS NULL` but discarded the affected-row count and
  committed regardless, so a missing or already-consumed invite updated zero
  rows while the user and membership committed anyway — one invite could have
  admitted an unbounded number of accounts. The update now encodes every
  predicate that authorizes the admission and requires exactly one affected
  row, which also closes the window where an invite is revoked while a
  sign-in is in flight.

### Added

- A Google sign-in callback logs which of its five outcomes it took. Every one
  answers `303`, so an access log could not tell a successful sign-in from a
  refused one, and a user reporting that sign-in "just spins" could not be
  diagnosed at all. The one-time exchange code is never logged.

### Known limitations

- Accounts left half-created by the previous behaviour are not repaired by
  this release. They need the workspace membership added, or the user row
  removed so a fresh invite can admit them.
- Duplicate callbacks remain non-idempotent. Consistency is protected by the
  transaction, but the losing request fails a unique constraint and reports
  `sign_in_failed`. What requests a callback twice is not yet understood.

## [0.0.1-rc7] - 2026-08-10

### Added

- The SQL console completes table and column names from the connected
  database. Completion is fed from the local replica through a single
  `/tables/columns` request, so it never contacts the source: it keeps working
  while MySQL is unreachable and typing in the console cannot add load to
  production. A table that exists upstream but has not been snapshotted does
  not appear, which matches what can actually be queried.
- SQL formatting in the console, on a Format button and Shift-Alt-F, using
  sql-formatter's MySQL dialect. The formatter is imported on demand and
  compiles to a chunk the initial page never references, so it is downloaded
  only when someone formats. Unparseable SQL is left exactly as typed.

### Changed

- Activity and the audit trail are separate tabs rather than stacked cards,
  which previously meant scrolling the whole replication log to reach the
  audit trail. The dead-letter queue stays above both: it represents work
  that is stuck until an operator acts, so it must be visible from either tab.

### Verification

- The browser gate covers console completion and formatting, typing a prefix
  of a table that exists only in the test source so a pass cannot come from a
  built-in keyword list, and requiring the formatted query to still run.
- Failing browser checks now report the last browser-side errors. That
  capture immediately explained two checks previously written off as host
  contention: the control plane holds one job slot per database and answers
  409 while a supervisor cycle owns it, so a resnapshot click has to retry.
  A dead-letter check was also mutating the source while the mirror was still
  snapshotting, where the row is absorbed by the snapshot and no quarantine
  can occur.

## [0.0.1-rc6] - 2026-08-10

### Added

- Diagnostic logging across the engine, selected by `PINTAIL_LOG` (`error`,
  `info`, `debug`). Nine crates emit through a new zero-dependency
  `pintail-log` facade: every API request with its duration, all twenty-three
  control-plane events, the CDC resumed binlog position and each reconnect,
  snapshot start and chunk progress with the consistency verdict, per-table
  poll cycles with the strategy each chose, segment flushes and compaction
  deferrals, backup upload-versus-reuse counts, per-table probe timings, and
  why a wire connection ended.
- No log line carries a DSN, API key secret, invite token, OAuth exchange
  code, session JWT, or row value. Verified against a live source by
  searching the output for its password, host, user and session token.

### Fixed

- Replication failures reached no log at all. They were published to a
  broadcast channel that drops the event when nothing is subscribed, so a
  supervisor failing with no dashboard open left only a control-plane row
  written with a discarded result. `docker logs` showed two startup lines.
- The capability probe and connection test no longer share the 30-second
  control-plane deadline. Their cost scales with table count — a measured
  82-table source takes 11.8 seconds — so a large schema surfaced as
  "Request timed out after 30s" on a probe the server went on to complete.
  The timeout message now names the path that expired.
- `tokio-rustls` moves to 0.26, which drops `rustls` 0.22 and its
  `rustls-webpki` 0.102 (GHSA-82j2-j2ch-gfr8 high, GHSA-pwjx-qhcg-rvj4
  medium, GHSA-965h-392x-2mh5 and GHSA-xgp8-3hg3-c2mh low). The workspace
  already used `rustls` 0.23, so both a patched and a vulnerable webpki were
  compiled into the same binary; this removes the second TLS stack.
- `time` moves to 0.3.47 for CVE-2026-25727, a stack-exhaustion denial of
  service. The declared MSRV moves to 1.88 to match, because the MSRV-aware
  resolver would otherwise pull the workspace back to the vulnerable release.
- The Go wire-client matrix takes `filippo.io/edwards25519` 1.1.1 for
  CVE-2026-26958. Test-only; not shipped in the product.

## [0.0.1-rc5] - 2026-08-10

### Fixed

- A restored backup is assigned to the workspace it was restored from.
  `register_restored_database` inserted the row without a `workspace_id` while
  every dashboard listing filters on one, so restore reported success, wrote
  the segments and registered the tables, and produced a database that nothing
  could display and no screen could adopt.
- The dashboard reports failures that previously produced no visible change:
  API key enable/disable and revoke, dead-letter discard, and database removal
  all issued their request with no rejection handler, so a failed call left the
  identical screen behind and read as an inert click.
- Entering a workspace no longer awaits the SSE consumer loop, which never
  returns, so the create-workspace dialog closes instead of spinning behind a
  request that already succeeded.
- Every dashboard API request carries a deadline, so a hung call surfaces as a
  timeout instead of an indefinite spinner.
- The add-database wizard explains an empty table list. `information_schema`
  lists only tables the connecting user holds a privilege on, so the usual
  cause is a missing grant rather than an empty schema; the empty state now
  names that and prints the `GRANT` to run.
- The Google public URL is validated on the field. A non-HTTPS origin rejected
  the whole settings save, which appeared as the enable toggle turning itself
  off, a card still reading "Not Configured", and no Google button on the login
  page - three symptoms from one discarded field.
- Selecting a replication mode confirms the mode that was set. Every mode other
  than `paused` was reported as "Replication resumed", including CDC and
  polling.

### Verification

- The browser gate covers workspace create and switch, the API key lifecycle,
  replication mode changes and resnapshot, and backup destination/run/restore
  against a real S3-compatible object store rather than a stub.

## [0.0.1-rc4] - 2026-08-10

A deployment fix on top of rc3, with no engine changes.

### Fixed

- The process query memory budget reads a cgroup limit before host memory.
  rc3 derived its default from `/proc/meminfo`, which inside a container
  reports the host's memory rather than the container's ceiling, so a container
  capped at 512 MB computed roughly 45 GB of budget: the ceiling never engaged
  where a container limit makes it matter most, and the kernel OOM killer
  decided instead. Both cgroup versions are read, v2 first, and both spell
  "unlimited" as a value rather than an absence - v2 as the literal `max`, v1
  as a sentinel near `u64::MAX` - so an unlimited cgroup falls through to host
  memory instead of surfacing an absurd ceiling.

### Changed

- The production Compose file pulls the published image instead of building
  from source, so a deploy host no longer compiles the Rust workspace and the
  dashboard on every release; the source build moved to
  `docker-compose.dev.yml`.
- Storage relocates by variable — `PINTAIL_DATA` and `PINTAIL_SPILL_DATA` —
  each defaulting to a named volume and accepting an absolute host path, so the
  setting survives platforms that re-clone the repository on redeploy. A bind
  path must be owned by `10001:10001` before first boot, because the container
  does not run as root and does not fix its own ownership.
- Release builds cache to the container registry rather than the GitHub Actions
  cache, which was measured spending 235 s per run writing a cache that
  produced zero hits against its 10 GB cap.

## [0.0.1-rc3] - 2026-08-10

### Fixed

- ENUM values carry their declaration index and order by it, rather than
  alphabetically by label.
- The all-zero date `0000-00-00` is preserved as the value MySQL returns
  instead of being mapped to `NULL`, which had inverted `IS NULL`, equality and
  `COUNT` for those rows.
- `sql_mode` values that would change how a statement parses or evaluates —
  `ANSI_QUOTES`, `PIPES_AS_CONCAT`, `ALLOW_INVALID_DATES` and the rest — are
  refused rather than stored and silently ignored.
- The compatibility matrix counts `DATE_ADD` in the callable surface, and the
  function-surface reader reads both binder modules instead of one.

### Added

- Concurrent query execution on the wire server is bounded, so overload becomes
  backpressure instead of unbounded queueing: measured p99 fell 76% at 256
  concurrent clients and stopped tracking offered load.
- A process-wide query memory budget bounds the sum of concurrent queries
  rather than only each one individually, defaulting to three quarters of host
  memory.
- A concurrency load harness (`tests/load`) with banked before/after evidence
  for admission control.
- A MySQL keyword and function compatibility matrix in `parity.md`, generated
  from live MySQL and ClickHouse inventories rather than written from memory.

### Changed

- The MySQL wire protocol is implemented by the from-scratch
  `pintail-protocol` crate; the vendored `opensrv-mysql` fork is gone. This is
  what lets Pintail control the column metadata — length, charset, decimals —
  that the fork hardcoded.
- The five largest engine files are decomposed into focused modules: execution
  error, window, sort, join and aggregation paths; the block payload codec,
  projected scans and table snapshot in storage; function binding in the SQL
  binder; and MySQL temporal semantics in expression evaluation. No behaviour
  change.

### Verification

- The Playwright browser gate is a required gate rather than advisory, follows
  the dashboard's navigation roles and the redesigned snapshot flow, and
  redacts first-boot secrets from its logs.

## [0.0.1-rc2] - 2026-08-08

### Added

- PTSEG v3 adaptive block compression: normal flushes retain LZ4 only when it
  shrinks an encoded block by at least 5%, otherwise storing an exact-length raw
  payload. Existing LZ4/zstd segments remain readable and cold-tier compaction
  remains zstd; mixed raw/LZ4 reopen and corruption tests cover the new tag.
- The analytical benchmark's four ad-hoc query shapes now report medians over
  five distinct memo-cold predicate variants instead of one noisy cold run;
  MySQL expectations are cached per variant and JSON results retain the full
  cold-query evidence separately from the warm release gate. The first 20M-row
  benchmark-host run matched MySQL exactly and measured Pintail at 525/1,031/426/
  1,017 ms for N1-N4 versus MySQL at 1,086/10,732/5,893/52,533 ms.
- Query result metadata now retains the resolved source/default text collation
  through the shared query engine and HTTP response; non-text fields report no
  collation. CDC restart coverage also proves schema-history charset and
  collation metadata survive reopening a tracked table.
- MySQL differential oracle diversify batch: typed multi-table `orders` seed
  (`DECIMAL` / `DATETIME` / `JSON`), forty column-native match cases plus a
  twelve-case collation matrix (874 total),
  twelve fail-closed reject shapes (`documented_rejects_stay_explicit`), twelve
  additional e2e differential query shapes (47 total), and
  `scripts/oracle-coverage.ts` for family/template/function inventory. Prefer
  template entropy and typed-column coverage over raw case count.
- A pinned read-only ORM differential matrix exercises Sequelize, Prisma, and
  Drizzle against MySQL and Pintail, comparing decoded reads, generated query
  shapes, and schema-introspection artifacts. ORM writes and migration
  execution remain outside the compact compatibility scope.
- A BI dogfooding harness ingests JSONL, MySQL general-log exports, or plain
  SQL; keeps exact captures and replay evidence local; frequency-deduplicates
  redacted query shapes; excludes data-changing statements; and can compare
  MySQL and Pintail results through the same mysql2 client. Shareable reports
  omit result values, and replay credentials are read only from the process
  environment. This remains optional diagnostic tooling, not a BI integration
  or release requirement.
- Unaliased parenthesized join groups can now occupy a later join's right side,
  preserving bushy INNER/CROSS/LEFT boundaries, constituent qualified names,
  wildcard order, and nested nullability. Correlated subqueries in `ON`
  predicates execute through a bounded nested-loop fallback when hash-key
  extraction alone cannot represent the condition.
- Correlated scalar, `EXISTS`, and `IN` subqueries that cannot use the canonical
  decorrelation rewrites now have a bounded dependent-execution fallback. It
  supports wider predicates, nested scopes with local alias shadowing, HAVING,
  non-recursive CTEs, and derived-table shapes while retaining scalar
  cardinality errors and the query memory/deadline ceilings.
- MySQL wire sessions now implement `COM_RESET_CONNECTION`,
  `COM_CHANGE_USER`, and `COM_STMT_RESET`, restoring defaults, repeating
  scoped authentication where required, and invalidating stale prepared
  statements without dropping the pooled socket. An idle-connection deadline
  is configurable through TOML, environment, and CLI settings.
- Wire sessions implement `max_execution_time` as a cooperative millisecond
  statement deadline. Execution and subquery pulls return MySQL interruption
  error 1317 when it expires, and pool reset/change-user restore the disabled
  default.
- Dropping a MySQL wire connection now cancels its active text, prepare-preview,
  or prepared execution cooperatively across scans, joins, aggregates, windows,
  recursive CTEs, and nested subqueries instead of retaining server capacity
  until the abandoned query finishes.
- Recursive CTE execution has a session-scoped `cte_max_recursion_depth`
  guard with a safe default and bounded configurable range; attempts to
  disable the guard are rejected.
- Query spill now uses an isolated temporary directory per execution with
  configurable per-query and process-wide disk ceilings. Prometheus exposes
  active/written bytes, file count, and quota failures; `EXPLAIN ANALYZE`
  reports the same counters for its query.
- Standalone `DISTINCT` now switches to the external-sort spill path and
  removes adjacent equal rows with the same collation and exact-DECIMAL
  comparator used by ordering; forced-spill and in-memory results are pinned
  byte-for-byte in differential tests.
- `INTERSECT [ALL]` and `EXCEPT [ALL]` now use an external sort-merge path
  instead of retaining the complete right side in memory. Distinct and
  multiset counts share the ordering, collation, exact-DECIMAL, memory, and
  spill-quota rules used by sort and standalone DISTINCT.
- Set-expression boundaries now preserve MySQL precedence and scoping across
  mixed `UNION DISTINCT`/`UNION ALL`, `INTERSECT`, and `EXCEPT` chains.
  Parenthesized operands and branch-local `ORDER BY`/`LIMIT` lower through an
  internal derived boundary instead of leaking clauses onto the full chain.

### Changed

- Filter-first scans stop probing later segments after an entire prefetch
  proves the predicate cannot skip useful ranges. On the settled-memo-disabled
  20M-row N4 profile this reduced the steady local median from 581 ms to
  505 ms while preserving the exact eight-row result.
- `information_schema` honors MySQL `BINARY` casts with bytewise filtering,
  ordering, and DISTINCT projection semantics for ORM discovery queries.
- Source generation expressions and generated defaults now flow through
  `information_schema`, SHOW COLUMNS/DESCRIBE, and synthesized SHOW CREATE
  output instead of being erased or reported as ordinary columns.
- `information_schema.columns.numeric_precision` uses the source MySQL integer
  declaration, preserving SMALLINT and MEDIUMINT widths after normalization.
- The wire compatibility probe reports `lower_case_table_names=2`, matching
  Pintail's source-spelling-preserving, case-insensitive catalog lookup.
- Metadata retains raw MySQL `EXTRA` text, including `ON UPDATE` clauses;
  binary LIKE stays bytewise, while ordinary DISTINCT and GROUP BY use the
  metadata relation's case-insensitive identifier collation.
- Replica temporal policy is explicit and shared by snapshot and CDC: zero or
  invalid DATE/DATETIME values normalize to SQL NULL, `sql_mode` is retained
  but does not reinterpret stored source values, and named timezone DST folds
  choose the earlier instant while gaps return NULL.

### Fixed

- The E2E differential corpus no longer uses MySQL's reserved `LINES` keyword
  as an alias, and the documented table-rename metadata warning now verifies
  that the table name is the only differing field across the full projection.
- Low-cardinality and two-pass string grouping now merge dictionary codes on
  the shared ICU collation key, including accent and expansion equivalents;
  `LIKE` also keeps `_` bound to one original character instead of one
  expanded case-fold code point.
- Physical scan statistics now include filter-first predicate probes even when
  the selector keeps the full segment, so unselective late-materialization
  attempts no longer disappear from `EXPLAIN ANALYZE` evidence.
- Nested LEFT JOIN groups now carry null-extension through their bound column
  layouts, so downstream expression nullability and derived metadata agree
  with the rows produced by bushy outer joins.
- Dependent subquery executions subtract the live outer batch from their child
  memory allowance, so retained parent state, the current row batch, and inner
  materialization cannot jointly exceed the query ceiling.
- Dependent scalar subqueries in unselected `IF`/`CASE` branches and after the
  first non-NULL `COALESCE` argument no longer execute eagerly, preserving
  MySQL short-circuit behavior and avoiding spurious cardinality errors.
- BI capture replay ignores interleaved SQL comments when classifying CTEs and
  session statements, so comments cannot disguise a write or global/persistent
  mutation as read-only SQL.
- Canonical correlated scalar lookups preserve MySQL cardinality: zero inner
  matches produce NULL, one produces the value, and more than one raises a
  scalar-subquery row error through a bounded spillable join.
- A complete parenthesized root LEFT JOIN binds without flattening away its
  outer semantics, including when a later join extends the root's left-deep
  chain.
- Uncorrelated `EXISTS` stops its inner execution after one row, and scalar
  subqueries stop after the second row needed to raise the MySQL cardinality
  error; neither materializes an irrelevant tail before deciding its result.
- Text equality, ordering, grouping, hashing, DISTINCT, joins, IN, and
  aggregate extrema now share a primary-strength Unicode collation key for the
  initial `utf8mb4_0900_ai_ci` profile. Accent/case folding is no longer an
  opt-in process flag, and LIKE/locate use the same character-level
  case/accent policy while binary values remain bytewise.
- Source collations now survive probing, catalog binding, and derived-column
  layouts. Lossless projection remains available for unsupported collations,
  while collation-sensitive operations reject unsupported or mixed source
  profiles instead of silently applying `utf8mb4_0900_ai_ci` semantics.
- Explicit `COLLATE utf8mb4_0900_ai_ci` now binds for compatible text operands;
  other profiles and incompatible source collations fail explicitly. The
  declared MySQL 8 profile pins its NO PAD trailing-space behavior through the
  differential oracle and the shared comparison/hash key tests.
- Keyless-table identity and mutation guarantees are visible in the table API,
  dashboard, and Prometheus metrics. Ambiguous UPDATE/DELETE behavior is
  documented and acceptance-covered through quarantine plus exact
  duplicate-multiplicity repair; key promotion/demotion remains a safe
  resnapshot boundary rather than an in-place identity guess. If legacy
  durable metadata has a stable key but no readable probe classification, the
  table API reports an unknown key mode instead of guessing primary vs unique.
- Metadata now preserves source MySQL nullability independently from the
  permissive physical normalization carrier and reports it consistently
  through `information_schema`, SHOW/DESCRIBE, SHOW CREATE, and direct
  text/prepared `SELECT` result fields, including non-key columns.
- Interrupted HTTP queries now return Request Timeout instead of being
  misreported as an internal server error.
- Production image builds include the vendored `opensrv-mysql` path dependency
  in both cargo-chef stages; benchmark baselines retain an opaque host
  fingerprint instead of a private infrastructure name.
- Google OAuth callbacks keep session JWTs out of browser URLs by issuing a
  short-lived one-time exchange code, and signed OAuth state is bound to an
  HttpOnly SameSite cookie from the browser that initiated sign-in.
- Google OAuth redirect URIs come from a validated administrator-configured
  public origin rather than forwarded request headers. Incomplete identities,
  disabled users, and silent email-based account linking now fail closed.
- Existing users can explicitly link a matching verified Google identity from
  an authenticated Settings session. The signed link intent names that user,
  refuses cross-email or cross-account binding, and never replaces an existing
  different subject.

### Verification

- Drizzle compatibility requires a successful `drizzle-kit pull`; matching
  partial artifacts from failed introspection processes can no longer pass.
- The validation driver resolves Cargo explicitly, keeps its target directory
  in-repository, and serializes nextest execution on macOS loader cold starts.
- The nightly external wire matrix now includes Go `database/sql` with
  go-sql-driver/mysql parameter interpolation, covering authentication, a bound parameter,
  and information-schema discovery alongside mysql_async, mysql2, PyMySQL,
  and the MySQL 8.4 CLI.
- The production E2E binary is restarted per spillable operator with a small
  ceiling sized above one input batch and below accumulated operator state;
  live sort, grouped aggregation, standalone DISTINCT, and hash join must each
  report nonzero spill files and bytes before normal configuration is restored.
- The clean repository gate passes formatting and strict workspace Clippy, 411
  nextest cases, all 874 byte-exact MySQL 8.4 differential cases, and E2E with
  637 passes, zero failures, and two documented-gap warnings.
- The deterministic 20-million-order benchmark matches MySQL results and
  passes the required 50x aggregate-speedup gate. The ci-profile production
  snapshot and cold-query acceptance workload also passes with its declared
  unsupported-query boundaries unchanged.

## [0.0.1-rc1] - 2026-08-05

### Removed

- Point-in-time restore (`point_in_time` + `dsn` on the restore request,
  the bounded CDC catch-up, and the CDC stop bound) — product decision;
  recovery is re-snapshot or restore-latest-backup. Backup retention,
  restore validation, and the full/incremental cadence are unchanged.

### Added

- An experiment lab (`experiments/`) benchmarks contested engine designs as
  checksum-verified head-to-heads on both reference machines; verdicts and
  three literature results that failed to replicate are recorded in
  `experiments/RESULTS.md` and ratified as architecture decisions.
- A production-shaped workload (`benchmark/workloads/commerce-production-v1`)
  models multi-tenant commerce with Zipf skew, correlated statuses, lifecycle
  mutations, and a cascade-delete negative control, with smoke/ci/full
  profiles and phased execution including a mixed CDC read/write phase.
- Versioned benchmark datasets live in the pintail-ds repository with sha256
  manifests; runs load them via `--dataset` using server-side TSV bulk import
  with deferred index creation, and a provenance check flags aliases the
  current seeder can no longer reproduce.
- Production benchmark CI tiers: per-merge ci-profile runs, a nightly with the
  mixed CDC and kill-restart phases, and a full-profile release gate, all
  gated on a repository variable for the benchmark host.

### Changed

- Merge clusters refine to granule level: a base-plus-tail cluster splits into
  direct row-ranges of the dominant unique-key segment plus one merge bounded
  to the actual overlap, located through the segment footer's sparse index
  (previously written but never read); no storage format change.
- Low-cardinality string group-bys (one or two key columns) aggregate on
  per-batch dictionary codes with array-indexed accumulators; integer-keyed
  fused join aggregates probe a dense direct-address table when build keys
  occupy a small range; top-K materialization skips rows that cannot beat the
  current threshold before cloning them.
- Decimals, dates, and datetimes parse once at column construction into
  scaled/epoch integers consumed by filters, aggregates, and group hashing,
  with conservative fallback to their text carriers on any non-canonical
  value; typed projections build lazily so batches that never use them pay
  nothing.
- Scans partition the requested key range by actual segment overlap: disjoint
  unique-key clusters decode directly, only overlapping clusters pay the
  bounded last-write-wins merge, and memtable rows are served range-aware —
  previously any WAL row or overlap forced every row through the k-way merge.
- The release benchmark measures all engines on the same host under identical
  CPU/memory limits, adds a ReplacingMergeTree-with-FINAL fair reference,
  reports median-of-five warm runs, and fails on any result differing from
  MySQL; the previous cross-host ClickHouse comparison is retired.
- Column vectors build packed typed projections (integers, floats, string
  views, scaled-i128 decimals parsed once from their text carrier) during
  construction; comparison filters and SUM/AVG aggregates resolve from packed
  values instead of walking or re-parsing per-row `Value`s, with row-at-a-time
  semantics preserved as the fallback (text comparisons keep their
  collation-aware path).

## [M9] - 2026-07-30

### Added

- A deterministic Bun release workload ports Duckling's eight-query,
  20-million-order analytical suite to isolated MySQL 8.4, Pintail, and
  ClickHouse 25.8 instances, with exact row checks and a required aggregate
  speedup of at least 50× over source MySQL.
- A deterministic 30-minute CDC soak owns its MySQL 8.4 source, generates
  insert/update/delete traffic at a 5,500-event/s target, and records every
  lag, DLQ, RSS, convergence, and checksum sample as checked-in JSON and
  Markdown release evidence.
- An M9 release report and Duckling known-limit parity table make the v1
  compatibility boundary, operational tradeoffs, and full validation matrix
  explicit.

### Changed

- Immutable scans verify segment structure without whole-file reads, stream
  disjoint projected columns directly, and resolve large overlapping views by
  merging system-column headers before late-materializing winning values in
  bounded chunks.
- Joins, grouped aggregation, row accounting, storage-key scans, and projected
  segment prefetch use bounded streaming or parallel paths under the shared
  hard query-memory ceiling.
- Compaction merges checksummed input blocks incrementally, moves winners
  instead of cloning them, bounds admitted input rows, and partitions output
  segments so background maintenance remains inside the release RSS envelope.
- Oversized CDC source transactions spill to anonymous temporary storage
  without weakening atomic publication or checkpoint-before-replay safety.
- The CDC restart gate uses a source-side named-lock barrier after the tenth
  commit, proving the worker is SIGKILLed with 190 writes still pending instead
  of relying on process-start timing.
- The production builder copies the SQL-oracle workspace member required by
  the root Cargo manifest, so a clean multi-stage image build can resolve the
  complete workspace before compiling the Pintail binary.

### Verification

- The exact 20-million-order benchmark verified equal row counts across MySQL
  8.4, Pintail, and ClickHouse 25.8. MySQL's eight queries took 3,841,437 ms,
  Pintail took 22,205 ms, and ClickHouse took 5,191 ms; Pintail's 173.0×
  aggregate speedup passed the required 50× gate.
- The 30-minute soak generated 9,898,625 row events at 5,499.2 events/s,
  converged on an exact 2,576,375-row source/replica checksum with zero DLQ,
  observed at most 27 seconds of lag, peaked at 291.2 MiB RSS, and fitted a
  46.6 MiB/hour RSS slope. Every enforced gate passed.
- The complete release matrix passed twice consecutively on the same product
  source: frozen Bun dashboard builds; formatting; strict all-target,
  all-feature Clippy and tests; 600-query MySQL oracle; crash/resume,
  replication, polling, wire-client, MinIO, and all API integration gates;
  production non-root Compose health; and real-browser desktop/mobile checks
  with exact typed rows, zero console errors or warnings, and no horizontal
  overflow.

## [M8] - 2026-07-30

### Added

- Control-plane schema version 7 stores per-database S3-compatible backup
  configuration, encrypted credential material, and durable full/incremental
  backup run history with parent chains, object counts, byte totals, and
  terminal errors.
- Control-plane schema version 8 extends the table lifecycle with a detached
  `restored` state while preserving existing table and child metadata during
  in-place upgrades.
- Native S3-compatible backups pin and encode storage manifests, upload
  checksum-addressed immutable segments, reuse unchanged objects in
  incremental chains, publish portable JSON manifests last, and restore only
  into new side-by-side directories after SHA-256 verification.
- Authenticated backup APIs encrypt credentials at rest, configure per-database
  schedules, launch and audit manual full/incremental jobs, list their history,
  and restore a completed backup as a new detached, queryable database without
  exporting the source DSN.
- A process supervisor now runs finite CDC/polling cycles in isolated
  per-database workers, retries failed sources without stalling healthy
  mirrors, and launches due scheduled backups. Prometheus text metrics expose
  query latency/rows, ingest cycles and errors, replication lag, storage and
  compaction debt, memory, DLQ pressure, and backup outcomes; DLQ entries can
  be retried through a safe table reconciliation before removal.
- The Backups dashboard is fully active with S3/MinIO destination and schedule
  controls, encrypted-credential handoff, manual/full actions, recovery-chain
  history, and side-by-side restore. Activity and database views offer
  retry-before-discard DLQ controls, while Settings links the live Prometheus
  surface and reports the isolated supervisor policy.

### Verification

- An ignored Docker gate completes a full backup and an incremental backup
  against MinIO, proves that an unchanged immutable segment is reused, and
  restores both generations through SHA-256-verified object downloads.
- A three-source MySQL 8.4 gate runs CDC and polling databases concurrently,
  stops a third source, and proves that its durable error state does not
  interrupt healthy supervisor cycles or queries.
- Rust formatting, strict workspace Clippy, the locked all-feature workspace
  tests, Bun's frozen install/typecheck/static generation, and desktop/mobile
  Playwright checks pass with no browser errors, warnings, or horizontal
  overflow.

## [M7] - 2026-07-30

### Added

- Control-plane schema version 6 stores the `mysql_native_password`
  double-SHA-1 verifier alongside each new hash-only API key, enabling standard
  MySQL challenge-response authentication without retaining or recovering the
  one-time plaintext secret.
- A read-only MySQL wire server now listens on the configurable `wire.bind`
  address, authenticates database-scoped query keys, and routes text and
  prepared statements through the same reader-pinned SQL engine as HTTP.
  `SHOW`, `DESCRIBE`, `information_schema`, BI-style aggregates, EXPLAIN,
  session setup commands, bounded results, typed binary rows, and clear write
  rejection are covered by the compatibility gate.
- Node status now reports the active wire bind and read-only authentication
  policy. The dashboard marks that endpoint live and generates complete,
  copyable MySQL CLI, Bun/mysql2, and PyMySQL examples plus DBeaver and
  Metabase connection fields from the selected database, host, port, and key.

### Verification

- The wire compatibility gate passes `mysql_async`, the MySQL 8.4 CLI, mysql2
  under Bun, and PyMySQL, including native challenge authentication, database
  selection, metadata discovery, prepared parameters, BI-style queries, and
  exact binary-protocol values for decimal, temporal, JSON, Unicode, blob, and
  narrow numeric columns.

## [M6] - 2026-07-30

### Added

- Control-plane schema version 5 adds dashboard user state, scoped API-key
  metadata, per-database polling/reconciliation cadence, and per-table
  soft-delete mapping, with typed CRUD records and an in-place v4 upgrade.
- The HTTP control plane now supports one-time Argon2id admin setup, signed
  JWT login/session authentication, ChaCha20-Poly1305 encrypted source DSNs,
  database CRUD/test/probe routes, and SHA-256 hash-only database API keys
  whose `pk_` secret is shown exactly once and enforced by scope.
- Authenticated snapshot jobs now resume durable chunks, emit database-scoped
  SSE/WebSocket progress only after publication, and hand populated stores to
  a finite CDC catch-up or forced polling convergence before reporting ready.
- The read-only HTTP SQL surface now executes against reader-pinned table
  snapshots with typed fields, bounded results, and physical pruning stats;
  table schema/preview/count, activity, and dead-letter routes share the same
  database-scoped authorization model.
- The embedded Nuxt control plane now provides setup and login, fleet and
  database health, a guided source wizard, snapshot and replication progress,
  table/schema/storage inspection, a lazy-loaded CodeMirror SQL console with
  export, activity and dead-letter views, scoped API-key management, responsive
  navigation, and explicit preactivation states for later backup and settings
  milestones.
- Authenticated table controls now run checkpoint-preserving, table-local
  reconciliation for CDC and polling mirrors. Table resync actions use the
  safe database-wide snapshot handoff because source checkpoints are shared;
  both operations are durable activity records and publish scoped events.

### Verification

- Rust formatting, strict workspace Clippy, the locked workspace tests, Bun's
  frozen install, dashboard type checking, and static generation pass.
- The MySQL 8.4 HTTP gate passes connection test, capability probe, snapshot,
  polling handoff, typed SQL query, table-local reconciliation, and safe
  resnapshot through authenticated routes.
- The Playwright smoke passes first-boot setup, the four-step source wizard,
  snapshot-to-streaming progress, live query results, desktop and mobile
  layouts, accessible icon navigation, and a zero-error browser console.

## [M5] - 2026-07-30

### Added

- A durable polling engine with automatic timestamp/created/auto-increment
  cursor selection, inclusive boundary rereads, cheap count/maximum probes,
  monotonic poll versions, soft-delete mapping, complete primary-key
  reconciliation, cursor-less chunk checksums, and append-table rebuilds.
- Per-table checksum chunk fingerprints are persisted and replaced atomically
  with their polling checkpoint, including in-place metadata upgrades.
- Cursor-less tables compare source-side aggregate fingerprints with durable
  source/replica fingerprints and fetch full rows only for mismatched chunks;
  key-only sweeps repair deletes without re-shipping unchanged row payloads.
- Live table writers can publish compatible nullable-column additions and
  column drops by stable ID without closing the store; pinned readers retain
  their original schema view.
- Source DDL generations and serialized columns are persisted idempotently;
  dropped source tables are marked as retained orphans instead of deleting
  replica data.
- CDC query events now track MySQL DDL: ADD/DROP COLUMN evolve live stores,
  TRUNCATE publishes an empty generation, rename/type/key-affecting changes
  quarantine only their table for resnapshot, DROP retains orphaned data, and
  matching CREATE TABLE events auto-snapshot a new target. Durable stable
  column IDs allow evolved writers to reopen safely after restart.
- Polling UNIQUE audits now issue targeted primary-key existence lookups and
  tombstone only stale colliding rows. An opt-in query scan policy can hide
  lower-version secondary-UNIQUE collisions until that repair completes,
  including when the unique columns were not selected by the query.
- A reconciliation-only CDC path now repairs cascade/SET NULL child deletes
  and payload updates with versions above their live binlog rows while
  preserving the CDC mode and source checkpoint. The live gate proves both
  InnoDB negative controls before converging the child rows.
- Delete reconciliation now uses composite-safe keyset pagination. Poll syncs
  still run cursor-boundary, checksum, or append checks when count/MAX is
  unchanged, closing same-timestamp and count-neutral update windows without
  adding row-storage writes.
- Secondary-UNIQUE collision audits now trigger immediate delete repair, and
  probe-flagged cascade/SET NULL child tables can run reconciliation even when
  their primary replication mode is CDC.
- A binlog-disabled MySQL 8.4 gate covers polling CRUD, the count-neutral
  delete blind spot, unique-value reuse, soft deletes, cascade reconciliation,
  append tables, and ten idle forced scans with zero table-storage growth.

### Changed

- Count/MAX polling tokens are advisory: an unchanged token still runs the
  strategy-specific cursor boundary, aggregate checksum, or append-generation
  check. Delete reconciliation uses composite-safe keyset pagination.
- Pure ADD/DROP column changes preserve stable source-column IDs through live
  schema generations. Other ALTER operations conservatively quarantine only
  the affected table instead of risking a storage reinterpretation.

### Verification

- Rust formatting, strict workspace Clippy, the locked workspace tests, Bun's
  frozen install/typecheck/static generation, the plan-quality suite, and the
  600-query MySQL differential oracle pass.
- The MySQL 8.4 DDL gate passes ADD/DROP across restart, table-local rename
  quarantine, TRUNCATE, CREATE auto-snapshot, and retained DROP orphan checks.
- The binlog-disabled polling gate passes cursor and cursor-less CRUD,
  composite keys, exact unchanged-token delete/insert repair, unique reuse,
  soft deletes, append rebuild, and ten byte-stable idle cycles.
- The CDC cascade gate proves missing InnoDB child delete and update events
  before scheduled full-row reconciliation, preserving both CDC mode and its
  source checkpoint.

## [M4] - 2026-07-30

### Added

- Native `mysql_async` row-binlog streaming from MySQL GTID sets or classic
  file/position checkpoints, with MariaDB GTID sources using their captured
  file/position fallback.
- FULL-image INSERT, UPDATE, primary-key-changing UPDATE, and DELETE decoding
  into deterministic versioned rows and tombstones. GIPK/invisible primary
  keys, ENUM/SET indexes, packed timestamps, BIT, exact decimal text, JSON,
  blobs, utf8mb4, and latin1 are covered by live source tests.
- Transaction buffering with a 64 MiB default hard cap, InnoDB XID/query
  boundaries, MyISAM statement boundaries, WAL synchronization before the
  SQLite source checkpoint, and durable progress callbacks.
- Bounded exponential reconnect from the last durable checkpoint. Purged or
  out-of-range file positions and server error 1236 durably mark the source
  `needs_resync`.
- One-shot automatic resnapshot recovery that clears the stale chunk journal
  and checkpoint, publishes empty table generations without invalidating
  pinned readers, captures a fresh handoff, and resumes CDC.
- Idempotent SQLite DLQ records for row decode failures. A failed table is
  quarantined across restarts while unrelated tables keep streaming.
- A CDC-specific append ingest path whose binlog-derived keys make replayed
  inserts invisible instead of allocating duplicate local row IDs.
- Docker gates for GTID and file/position CRUD, type fidelity, GIPK,
  append-only rows, MyISAM, MySQL 5.7/8.4, MariaDB 11, checkpoint rewind,
  real-process SIGKILL under sustained writes, decode quarantine, binlog
  purge, and automatic resnapshot.

### Changed

- `mysql_async` now enables its protocol `binlog` feature while retaining the
  minimal Rustls/ring client feature set.
- Snapshot value normalization is shared with CDC after binlog-specific value
  adaptation, so zero and out-of-range temporal values become `NULL`
  consistently in both engines.
- CDC checkpoints update source/table streaming state in the same SQLite
  transaction and never clear a table's sticky `needs_resync` state.

### Verification

- Rust formatting, strict workspace Clippy, the locked workspace tests, Bun's
  frozen install/typecheck/static generation, the plan-quality suite, and the
  600-query MySQL differential oracle pass.
- The serialized CDC Docker suite passes all five worker tests against MySQL
  5.7, MySQL 8.4 GTID and file/position, and MariaDB 11.
- A CDC worker is SIGKILLed after ten durable checkpoints while its paced
  writer is still active; reopen converges to exactly 200 rows with the
  expected 19,900 ID sum.
- A captured binlog is purged on MySQL 8.4, producing durable resync state;
  the default one-shot recovery snapshots the missing row and resumes from a
  newly captured file/position.

## [M3] - 2026-07-30

### Added

- Read-only MySQL/MariaDB capability probing through `mysql_async`, including
  server flavor, binlog/GTID settings, grants, source tables, columns,
  charsets/collations, generated columns, and deterministic primary,
  non-nullable UNIQUE, or append-row-id key selection.
- Exact logical schema types for signed and unsigned integer widths,
  `Float32`, bounded decimal precision/scale, dates, fractional date-times and
  times, and canonical JSON, with probe warnings for DECIMAL precision above
  38 and unknown source types.
- A coordinated snapshot engine that briefly takes a global read lock,
  captures GTID or file/position, establishes parallel repeatable-read
  consistent transactions, then releases the lock before copying data.
- Keyset pagination for scalar and composite source keys, single-worker
  offset scanning for PK-less tables, explicit source projections, escaped
  identifiers, and configurable workers and durable chunk sizes.
- Lossless snapshot conversion for the M3 MySQL type matrix, including
  latin1-to-UTF-8 connection transcoding, BIT, ENUM/SET, binary/blob,
  geometry WKB, stored generated columns, JSON canonicalization, and mandatory
  NULL normalization for zero or invalid dates and date-times.
- Direct snapshot bulk ingest that validates, sorts, and publishes immutable
  version-zero PTSEG runs without writing the memtable or WAL.
- SQLite snapshot chunk journals, exact durable row totals, progress
  callbacks with throughput bytes and ETA, idempotent chunk completion, and
  first-position preservation across process restarts.
- Docker gates for one million rows across ten tables, a real child-process
  SIGKILL and resume, source/Pintail count-sum-CRC parity, the complete M3 type
  matrix, composite/UNIQUE/append/GIPK keys, MySQL 5.7 and 8.4,
  MariaDB 11 GTID, and a binlog-disabled polling source.

### Changed

- PTSEG version one now accepts exact M3 logical schemas while retaining its
  existing six physical scalar carriers; logical parameters participate in
  schema fingerprints and round-trip through reopen.
- Executor vectors, scalar normalization, joins, and numeric key pruning
  recognize the physical carrier associated with each logical M3 type.
- Internal snapshot schemas make only physical sort-key columns required, so
  zero dates from source `NOT NULL` columns can still normalize safely to
  `NULL`.
- Store recovery removes interrupted dot-prefixed segment writes as well as
  unpublished segment orphans.

### Verification

- Rust formatting, strict Clippy for all targets, component tests, and the
  complete locked workspace test suite pass.
- Bun's frozen install, dashboard type check, and static generation pass.
- The MySQL 8.4 gate snapshots 1,000,000 fact rows, validates exact aggregate
  checksums, kills a live snapshot worker after durable chunks, and resumes
  100,000 rows with no visible duplicates or gaps.
- The compatibility gate passes against MySQL 8.4 file/position, MySQL 5.7
  file/position, MariaDB 11 GTID, and MySQL 8.4 with binary logging disabled.
- The 600-query MySQL differential oracle and physical plan-quality gate
  remain green after the M3 logical type expansion.

## [M2] - 2026-07-30

### Added

- MySQL-dialect SQL parsing façade with backtick identifiers, MySQL
  offset/count limits, metadata statements, explain, common table expressions,
  and explicit single-statement request validation.
- Immutable catalog snapshots with stable database and table identities,
  case-insensitive name indexes, deterministic metadata iteration, versioned
  table schemas, and exact row-count statistics for planning.
- Query binder for table aliases, qualified and ambiguous columns, wildcard
  expansion, literals, core scalar operators, predicates, DISTINCT, and
  normalized MySQL limits, with stable catalog IDs in every bound reference.
- Logical query plans with explicit one-row, scan, cross-join, filter,
  projection, distinct, and limit operators plus conservative catalog-based
  cardinality estimates.
- Rule-based logical optimization with conservative constant folding,
  single-table conjunct pushdown, stable-ID projection pruning,
  cardinality-ordered cross joins, and semantics-safe scan limit propagation.
- Trivially safe aggregate pushdown through unreferenced identity cross-join
  inputs whose predicate-free catalog cardinality is exactly one.
- Typed columnar executor batches targeting 4,096 rows, including nullable
  vectors, zero-column relational rows, and compact shared selection masks.
- Pull-based physical execution for empty, one-row, scan, filter, project, and
  limit plans, with compiled scalar expressions, MySQL three-valued coercion,
  validated scan layouts, and a clear hard query-memory-cap error.
- Storage-backed scan provider that reads pinned table snapshots into bounded
  projected batches, validates schema generations, and supports zero-column
  scans for constant-per-row queries.
- Morsel-style projected scans that read independent segment headers and
  late-materialized column blocks concurrently on a Pintail-owned Rayon worker
  pool, followed by deterministic version-winner resolution.
- Memory-accounted streaming DISTINCT and materialized cross-join execution,
  with catalog cardinality required up front and a one-million-row Cartesian
  safety guard.
- Bound and logical explicit join chains for inner, left, semi, anti, and
  cross semantics, preserving ON predicates and outer-join-safe filter
  placement for physical hash-join planning.
- Memory-capped build-right equi hash joins with case-insensitive UTF-8 keys,
  SQL NULL non-matching, and inner, left, semi, and anti output semantics.
- Typed `GROUP BY` and `HAVING` binding with strict grouped-column validation,
  deduplicated aggregate slots, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, and
  `GROUP_CONCAT`, including DISTINCT aggregate inputs.
- Memory-capped hash aggregation with case-insensitive UTF-8 grouping and
  extrema, SQL empty-input aggregate results, post-aggregate HAVING
  evaluation, and positional projection of grouping keys and aggregate
  results.
- Output-alias, ordinal, and projected-expression `ORDER BY` binding with
  MySQL NULL placement, memory-capped full sorting, case-insensitive UTF-8
  ordering, and LIMIT-aware top-K partitioning.
- Type-checked `UNION ALL` binding and streaming branch concatenation, with
  outer ordering and limits applied after every branch in SQL source order.
- Typed and vectorized MySQL scalar expressions for `CONCAT`, `SUBSTRING`,
  `LOWER`, `UPPER`, `TRIM`, `LENGTH`, `CHAR_LENGTH`, `REPLACE`, `LEFT`,
  `RIGHT`, `LOCATE`, `IF`, `IFNULL`, `COALESCE`, `NULLIF`, searched and simple
  `CASE`, `LIKE`, list `IN`, `BETWEEN`, and core scalar casts, including SQL
  three-valued NULL behavior, `CAST` and MySQL `CONVERT` syntax, and
  short-circuit conditional evaluation.
- Local-session date/time evaluation for `NOW`, `CURDATE`, `DATE`, component
  extraction, `DATE_FORMAT`, single-field `DATE_ADD`/`DATE_SUB`, `DATEDIFF`,
  `UNIX_TIMESTAMP`, and `FROM_UNIXTIME`, including calendar-aware month
  arithmetic and invalid-date errors.
- Optimizer metadata substitution for predicate-free global `COUNT(*)`,
  returning exact catalog row counts without opening a storage scan.
- Stable physical `EXPLAIN` output for optimized queries, including operator
  hierarchy, scan estimates, stable projected column IDs, pushed-predicate
  counts, scan limits, join and aggregation strategies, and top-K bounds.
- `EXPLAIN ANALYZE` execution with actual segment and logical key-block
  read/prune counts plus decoded-block work, backed by projected range scans
  that translate supported single-component primary-key predicates into
  inclusive storage bounds.
- Deterministic catalog-backed `SHOW DATABASES`, `SHOW TABLES`, `SHOW COLUMNS`,
  and `DESCRIBE` responses with MySQL-compatible field names and type strings.
- Catalog-backed `information_schema.schemata`, `.tables`, and `.columns`
  basics with projection, aliases, case-insensitive filtering, ordering,
  limits, and `COUNT(*)`.
- Typed lowering for uncorrelated constant scalar subqueries and `IN` subqueries
  over `UNION ALL`, including empty scalar results, multi-row scalar errors, and
  SQL NULL membership semantics.
- One-time, memory-capped execution of uncorrelated table-reading scalar and
  `IN` subqueries, including aggregate results, filter predicates, empty
  results, and multi-row scalar cardinality errors.
- Typed non-recursive common table expressions and derived tables with fresh
  relation identities, projected column aliases, nested optimization, and
  execution through outer filters, aggregation, sorting, and hash joins.
- A Docker-backed 600-query MySQL 8.4 differential oracle combining generated
  cases with hand-written DISTINCT, nullable-table, three-valued logic,
  left/cross/inner join, scalar/date, subquery, scan, sort, aggregation, and
  `UNION ALL` workloads over equivalent pinned storage snapshots, with
  order-insensitive comparison where SQL does not specify row order.
- A plan-quality gate proving a selective predicate reads one of two segments
  and one of two key blocks while returning the MySQL-equivalent result.

### Changed

- UTF-8 `MIN` and `MAX` now use Pintail's case-insensitive comparison
  semantics, matching text predicates, grouping, joins, and ordering.
- Physical key pruning now requires explicit stable catalog key-column
  metadata and lossless integer conversion; unsafe first-column, text,
  append-row-id, out-of-range, and string-coercing assumptions fall back to a
  full scan.
- DISTINCT and mixed signed/unsigned hash joins now share the executor's
  case-insensitive and lossless numeric equality semantics.
- Projected scans transfer owned rows into pull batches without cloning the
  complete result, LIMIT-aware top-K trims after every input batch, and
  retained scan/container/subquery state participates in the hard query cap.
- Constant folding and `information_schema` filtering now reuse MySQL
  three-valued and case-insensitive runtime semantics.

### Verification

- Rust formatting, workspace Clippy with warnings denied, and the complete
  locked workspace test suite pass.
- Bun's frozen install, dashboard type check, and static generation pass.
- The Docker-backed generated and hand-written differential corpus matches all
  600 queries against MySQL 8.4.
- The plan-quality gate proves a selective key predicate reads one of two
  segments and one of two logical key blocks.

## [M1] - 2026-07-30

### Added

- Dependency-free typed schema, scalar value, composite-key, and versioned-row
  model shared by Pintail's data-path modules.
- Single-writer table store with atomic typed batches, an RCU-style memtable,
  configurable WAL synchronization, length-prefixed records, and per-record
  xxh3 checksums.
- Database store with one globally sequenced WAL multiplexed by stable table
  ID; per-table flush checkpoints preserve every other table's unpublished
  records.
- WAL recovery that discards a torn final record while rejecting checksum or
  sequence corruption with the failing byte offset.
- Immutable version-1 `PTSEG` files with independently checksummed,
  LZ4-compressed column blocks, null bitmaps, block statistics, sparse
  primary-key indexes, bloom filters, and checksummed footers.
- Atomic, checksummed table manifests that publish flushed segments before WAL
  truncation and pin reader snapshots by reference-counted generation.
- Adaptive version-1 block codecs for plain, dictionary, run-length,
  bit-packed, and delta-bit-packed values, with typed min/max statistics and
  retained 64-register HLL sketches.
- Bounded size-tier compaction for similarly sized overlapping segments,
  including byte-debt reporting, max-version collapse, partial-merge
  tombstone retention, full-merge tombstone removal, and zstd cold output.
- Reference-counted obsolete-segment reclamation that preserves pinned reader
  generations across writer drop/reopen and cleans unreferenced crash orphans
  only after the last process-local snapshot releases.
- Metadata-only nullable column additions for older segment and WAL rows,
  stable-ID dropped-column reads, and compaction-time removal of dropped
  bytes, with incompatible physical changes rejected.
- Stable column IDs embedded in every WAL batch so reordered, inserted, and
  dropped columns recover without positional value shifts; schemas also
  reject IDs reserved for physical storage metadata.
- Explicit primary, UNIQUE-fallback, and append-rowid table modes; append mode
  generates durable monotonic storage keys and deliberately performs no
  source-key deduplication.
- Enforced memtable bounds: a threshold-crossing batch performs one bounded
  flush, compaction, and obsolete-file maintenance step.
- Storage metrics for memtable bytes, live segment count, and compaction debt;
  compaction yields between input segments to preserve query scheduling
  opportunities.
- Manifest-resident primary-key bounds and bloom filters with pruned point and
  inclusive range reads that skip unrelated segment block decoding.
- Retained-version range scans that prune segments whose stored version bounds
  do not overlap the requested filter interval.
- Projected range scans with checksummed key-block zone-map pruning,
  cross-segment winner resolution before late materialization of requested
  user columns, and physical scan counters.
- Whole-block xxh3 coverage for null bitmaps, codec metadata, compressed
  values, zone maps, and HLL sketches, preventing corrupt statistics from
  causing false pruning.
- A manifest `globally_unique_keys` marker on full-compaction output and a
  single-segment scan fast path that bypasses merge-on-read state.

### Verification

- Public-interface tests verify well-typed rows and reject nullability or type
  mismatches before ingestion.
- Reopen tests verify checkpoint recovery, pinned reader snapshots,
  last-version-wins tombstones, pre-WAL validation, torn-tail repair, and
  precise checksum failures.
- WAL storage-exhaustion tests inject `StorageFull` after a partial record and
  verify recovery preserves and truncates to the prior complete prefix; live
  write and `always`-sync append failures roll back before a caller can retry.
- Multi-table tests verify global WAL sequencing, recovery through one
  database log, safe partial-table flushes, and rejection of unregistered WAL
  table IDs.
- Segment tests cover every scalar and null representation, multi-block
  reopen, pre-flush snapshots, max-version merge-on-read across segments and
  WAL recovery, and precise block-checksum corruption.
- On-disk format tests force and round-trip all five version-1 block encodings.
- Compaction tests cover delayed reclamation, partial versus full tombstone
  rules, zstd cold output, and 96 deterministic randomized segment-count,
  non-monotonic-version, and tombstone interleavings against a naive reference
  model.
- Recovery tests verify live footers during open, discard unpublished segment
  orphans, and prefer a durable manifest checkpoint when a crash leaves the
  pre-flush WAL in place.
- A process-level crash-fuzz test performs 100 kill/reopen cycles while a
  separate writer loops two tables through the shared database WAL, flush,
  manifest, and compaction paths; each reopen is checked against an external
  acknowledged-commit oracle for the full two-table state. A dedicated
  child-to-parent acknowledgement pipe prevents test-harness output capture
  from making that oracle stale.

## [M0] - 2026-07-30

### Added

- Rust 2024 Cargo workspace and SQLite WAL-mode control plane.
- Complete version 1 metadata schema, transactional migrations, and
  insert-once settings.
- Bun-managed Nuxt 4 + shadcn-vue dashboard source with a generated Badge
  component and responsive M0 shell.
- Prescribed Rust crate, integration-test, load-generator, SQL-logic, and
  benchmark boundaries for every planned component.
- `pintail-api` Axum `/health` route and build-time embedding of freshly
  generated dashboard assets.
- Single `pintail` executable with TOML, `PINTAIL_*`, and CLI configuration.
- First-boot JWT and DSN-encryption secrets, displayed only when created; the
  JWT is insert-once SQLite metadata and the DSN key uses an owner-only Unix
  boot-secret file.
- Owner-only Unix permissions for the data directory, SQLite control-plane
  database, and its WAL sidecars.
- Bun-only multi-stage container build and persistent Docker Compose
  deployment.
- M0 milestone gate report, local quick start, and architecture decisions for
  build tooling and control-plane boundaries.

### Verification

- Migration tests verify every required control-plane table and idempotent
  reopen.
- Settings tests verify insert-once secret persistence.
- Bun type checking and static generation verify the dashboard source.
- Dashboard HTTP tests verify embedded HTML and the JSON health response.
- Binary boot/restart tests verify SQLite initialization, `/health`, and
  one-time secret display.
- Unix permission tests protect every file that can contain first-boot
  secrets.
- Concurrent first-boot tests verify that another process waits for a complete,
  durably published boot-secret file.
- Unified CI generates the dashboard before running Rust formatting, linting,
  and workspace tests against those exact static assets.
