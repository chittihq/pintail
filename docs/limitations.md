# Known limitations

This document records deliberate compatibility boundaries and known
differences. A query rejected with an explicit error is preferable to a
plausible but incorrect result.

## M2 query engine

### SQL surface

- Scalar and `IN` subqueries may read tables, derived tables, and
  non-recursive CTEs, but they must be uncorrelated. An inner reference that
  depends on an outer query scope is rejected during binding.
- Recursive CTEs, window functions, and set operations other than
  `UNION ALL` are not implemented.
- `GROUP_CONCAT` accepts one expression with optional `DISTINCT`. MySQL's
  aggregate-local `ORDER BY`, custom `SEPARATOR`, and session
  `group_concat_max_len` behavior are not implemented.
- `CONVERT(value, type)` supports Pintail's scalar target types.
  `CONVERT(value USING charset)` distinguishes binary from character output,
  but does not perform byte-level transcoding among MySQL character sets.
- `information_schema` currently exposes catalog-backed `schemata`, `tables`,
  and `columns` basics. It supports simple projection, aliases,
  case-insensitive `=`, `<>`, `IN`, `LIKE`, `IS NULL`, Boolean filters,
  ordering, limits, and `COUNT(*)`. Other metadata tables, joins, aggregates,
  and the full MySQL column inventory are deferred. `COLUMN_KEY` and `EXTRA`
  are empty until source index/generated-column metadata enters the catalog.

### MySQL semantic differences

- Text comparison, grouping, hashing, `LIKE`, and ordering use a
  case-insensitive Unicode-lowercase approximation. Pintail does not yet
  implement MySQL's complete collation matrix, accent weights, locale
  tailoring, coercibility rules, or pad-space behavior. Binary values remain
  bytewise.
- `NOW()`, `CURDATE()`, and no-argument `UNIX_TIMESTAMP()` read the host clock
  and timezone when evaluated. Pintail does not yet expose a MySQL session
  timezone, and multiple evaluations in a long query are not pinned to one
  statement timestamp.
- Date parsing accepts the canonical date and date-time forms implemented by
  the M2 evaluator. `DATE_ADD` and `DATE_SUB` accept one interval field at a
  time; compound intervals and the full `DATE_FORMAT` directive inventory are
  not implemented.
- Pintail maps an empty scalar-subquery result to `NULL`. During oracle
  development, MySQL 8.4's constant `SELECT` with `LIMIT 0` produced a
  special-case result that did not follow this behavior; that MySQL-only
  corner is excluded from the common-workload corpus.
- Integer and floating arithmetic use Pintail's current `Int64`, `UInt64`,
  and `Float64` execution types. Exact `DECIMAL` query arithmetic will arrive
  with the wider type mapping work; numeric overflow returns an error.

### Planning and execution

- Storage predicate-to-key-range translation requires an explicitly declared
  one-column physical key mapping, an `Int64` or `UInt64` key, and an exact
  or losslessly convertible integer literal. Text keys, numeric/string
  coercions, out-of-range signedness conversions, synthetic append-row IDs,
  undeclared mappings, and composite keys remain correct but deliberately
  skip physical range pruning.
- Parallel projected scans schedule independent segment header and
  late-materialization work on a Pintail-owned Rayon pool. A snapshot
  containing only one relevant segment has no smaller storage morsels to
  parallelize.
- Hash joins, hash aggregation, sorting, distinct state, subquery
  materialization, retained projected scans, and cross joins obey a hard
  per-query memory cap. LIMIT-aware top-K retains only the current candidates
  plus one input batch; full sorting still materializes its complete input.
  Spill to disk is intentionally a v1.1 feature. Cross joins also require
  catalog cardinalities and reject estimates above one million rows.
- Aggregate pushdown is intentionally conservative. M2 removes only
  unreferenced predicate-free cross-join inputs with an exact catalog
  cardinality of one; Pintail has no relationship or uniqueness statistics
  that would justify broader rewrites safely.
- `EXPLAIN ANALYZE` scan counters accumulate work from all executions of a
  stable table in the statement, including uncorrelated subqueries.

## M3 snapshot engine

- M3 provides the probe and snapshot library surfaces plus durable chunk
  journals; the supervisor and REST/SSE controls that invoke them arrive in
  M6. Snapshot completion therefore leaves tables pending for the M4 CDC or M5
  polling owner.
- A missing `FLUSH TABLES WITH READ LOCK` privilege can be allowed explicitly.
  Every worker still uses a repeatable-read consistent transaction, but their
  start instants can differ and the result reports the degraded guarantee.
- Resume preserves the first attempt's CDC handoff position and replays
  already published chunks idempotently. This converges correctly once M4
  replays the overlap. Before that replay, a source changed between attempts
  can expose a mixed-time snapshot. On binlog-disabled sources, M5 polling and
  reconciliation own convergence.
- PK-less tables use a single-stream `LIMIT`/`OFFSET` scan and generated
  append-row IDs. Source changes between attempts can shift offsets; polling
  reconciliation is required because there is no stable source identity.
- Exact MySQL logical types and parameters are retained in schemas, while
  PTSEG v1 uses its existing physical carriers: narrow integers use 64-bit
  values, `Float32` uses the 64-bit float carrier, and decimal/temporal/JSON
  values use canonical UTF-8. DECIMAL values are lossless in storage, but M2
  query arithmetic still coerces them through the existing numeric executor
  and is not exact decimal arithmetic.
- `DECIMAL` precision above 38 maps to text with a probe warning. ENUM and SET
  snapshot values are textual. Virtual generated columns are skipped, while
  stored generated columns are included.
- Progress row estimates use `information_schema.TABLES.TABLE_ROWS`, which is
  approximate for InnoDB. Durable completed row and chunk counts are exact.

## M4 CDC engine

- M4 is still an in-process library surface. The multi-database supervisor,
  retention-pressure metrics and alerts, REST/SSE controls, and DLQ UI arrive
  in M6 and M8. The runner itself retries eight consecutive connection
  failures with exponential backoff capped at five seconds.
- MariaDB GTID text is captured and retained for diagnostics, but
  `mysql_common` 0.37 does not encode MariaDB's GTID dump request. MariaDB 11
  therefore resumes from the file/position captured alongside its GTID.
- DDL/schema-history handling belongs to M5. M4 uses live TableMap metadata for
  each row event but expects the probed target schema to remain compatible.
- Tables without a primary or safe UNIQUE key support idempotent INSERT CDC
  through deterministic append keys. UPDATE and DELETE have no stable source
  identity, so they enter the DLQ and mark that table `needs_resync`. MySQL 8
  GIPK tables have a real invisible key and support full CRUD.
- Binlog text transcoding currently covers utf8mb4/utf8mb3, ASCII, and MySQL
  latin1 (cp1252). Another source charset is quarantined through the DLQ
  instead of being silently interpreted.
- Versions reserve 16 bits for the intra-transaction mutation ordinal. GTID
  sequences must fit 48 bits. File/position versions support a 16-bit numeric
  file suffix and 32-bit event offset. A source transaction above 65,535
  physical mutations or the configured retained-byte cap fails explicitly.
- Automatic purge recovery is deliberately database-wide and attempted once
  per runner invocation. It resets every included target because one global
  source coordinate cannot safely advance while a table retains an
  unfillable gap.
- The M4 type gate asserts source-to-storage fidelity. HTTP and MySQL wire
  assertions cannot exist before their M6 and M7 server surfaces and remain
  future gates.

## Milestone boundary

M4 completes native snapshot plus CDC replication-library behavior. Polling,
DDL evolution, HTTP query APIs, the MySQL wire server, prepared-statement
compatibility, and BI-client smokes belong to M5 through M7 and are not
claimed here.
