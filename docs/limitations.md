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

## Milestone boundary

M2 is an in-process query-engine milestone. The snapshot, CDC, polling, HTTP
query API, MySQL wire server, prepared-statement compatibility, and BI-client
smokes belong to M3 through M7 and are not claimed here.
