# Changelog

All notable changes to Pintail are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Removed

- Point-in-time restore (`point_in_time` + `dsn` on the restore request,
  the bounded CDC catch-up, and the CDC stop bound) — product decision;
  recovery is re-snapshot or restore-latest-backup. Backup retention,
  restore validation, and the full/incremental cadence are unchanged.

### Added

- MySQL wire sessions now implement `COM_RESET_CONNECTION`,
  `COM_CHANGE_USER`, and `COM_STMT_RESET`, restoring defaults, repeating
  scoped authentication where required, and invalidating stale prepared
  statements without dropping the pooled socket. An idle-connection deadline
  is configurable through TOML, environment, and CLI settings.
- Wire sessions implement `max_execution_time` as a cooperative millisecond
  statement deadline. Execution and subquery pulls return MySQL interruption
  error 1317 when it expires, and pool reset/change-user restore the disabled
  default.
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

- Replica temporal policy is explicit and shared by snapshot and CDC: zero or
  invalid DATE/DATETIME values normalize to SQL NULL, `sql_mode` is retained
  but does not reinterpret stored source values, and named timezone DST folds
  choose the earlier instant while gaps return NULL.
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

### Fixed

- Canonical correlated scalar lookups preserve MySQL cardinality: zero inner
  matches produce NULL, one produces the value, and more than one raises a
  scalar-subquery row error through a bounded spillable join.
- A complete parenthesized root LEFT JOIN now binds without flattening away
  its outer semantics, including when a later join extends the root's
  left-deep chain. Aliased groups and bushy right-side join groups still reject
  explicitly.
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

- The nightly external wire matrix now includes Go `database/sql` with
  go-sql-driver/mysql parameter interpolation, covering authentication, a bound parameter,
  and information-schema discovery alongside mysql_async, mysql2, PyMySQL,
  and the MySQL 8.4 CLI.
- The production E2E binary is restarted per spillable operator with a small
  ceiling sized above one input batch and below accumulated operator state;
  live sort, grouped aggregation, standalone DISTINCT, and hash join must each
  report nonzero spill files and bytes before normal configuration is restored.
- The clean repository gate passes formatting and strict workspace Clippy, 394
  nextest cases, all 806 byte-exact MySQL 8.4 differential cases, and E2E with
  441 passes, zero failures, and three documented-gap warnings.
- The deterministic 20-million-order benchmark matches MySQL results and
  passes the required 50x aggregate-speedup gate. The ci-profile production
  snapshot and cold-query acceptance workload also passes with its declared
  unsupported-query boundaries unchanged.

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
