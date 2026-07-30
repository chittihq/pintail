# Changelog

All notable changes to Pintail are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
