# Known limitations

This document records deliberate compatibility boundaries and known
differences. A query rejected with an explicit error is preferable to a
plausible but incorrect result.

## M2 query engine

### SQL surface

- Scalar subqueries may read tables, derived tables, and non-recursive
  CTEs, but they must be uncorrelated. `EXISTS`/`NOT EXISTS` and
  `IN`/`NOT IN` support the canonical correlated form — a single-table
  subquery whose `WHERE` combines any number of equalities against the
  outer scope with inner-only filters — by rewriting to (multi-key)
  semi/anti joins with the inner-only filters kept in a derived input.
  Correlated `NOT IN` additionally requires both membership sides to be
  provably non-nullable: with a possible NULL, MySQL's three-valued
  `NOT IN` diverges from an anti join, so those shapes reject instead.
  Other correlated shapes are rejected during binding. Join `ON`
  conditions may be an `AND` of equality pairs (multi-key hash joins);
  non-equality join conditions remain rejected.
- Window functions cover `ROW_NUMBER`, `RANK`, `DENSE_RANK`, and
  `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` over `PARTITION BY` / `ORDER BY` with
  MySQL's default frames, nested anywhere in projection expressions and
  over grouped output. Explicit frames (`ROWS`/`RANGE BETWEEN`), named
  windows, `LAG`/`LEAD`/`NTILE`/`FIRST_VALUE`/`LAST_VALUE`, and windows
  combined with `DISTINCT` are not implemented. `UNION [ALL | DISTINCT]`,
  `INTERSECT [ALL]`, and `EXCEPT [ALL]` follow MySQL's left-associative
  set semantics, including exact `ALL` multiset counts (a distinct union
  under a later `UNION ALL` rejects explicitly). UNION branches unify
  numeric types the way MySQL does: signed with `BIGINT UNSIGNED`
  becomes `DECIMAL(20,0)` and integer with `DECIMAL` widens the integer
  part, with branch values cast to the unified type. `WITH RECURSIVE`
  supports the canonical `anchor UNION [ALL] member` form: one recursive
  member that scans the CTE exactly once in its `FROM`, no aggregates,
  windows, `DISTINCT`, `GROUP BY`, `ORDER BY`, or `LIMIT` inside the
  member, member column storage types matching the anchor's, and MySQL's
  default `cte_max_recursion_depth` of 1000 (not configurable; a
  non-converging recursion aborts with an explicit error). `RIGHT JOIN`
  supports the two-table form.
- `GROUP_CONCAT` accepts one expression with optional `DISTINCT`,
  aggregate-local `ORDER BY`, and `SEPARATOR`, truncating at MySQL's
  default `group_concat_max_len` of 1024 bytes (the session variable is
  not configurable).
- `JSON_OBJECT`, `JSON_ARRAY`, and `JSON_ARRAYAGG` render MySQL-shaped
  JSON text (`", "`/`": "` separators, object keys ordered by length
  then bytes, last duplicate key wins). Values map by execution type:
  NULL becomes JSON null and integers/floats stay numbers, but DECIMAL
  and temporal values encode as JSON strings where MySQL would emit
  numbers or datetime scalars — there is no JSON column type in the
  executor. `JSON_ARRAYAGG` collects in input order; MySQL does not
  guarantee an order either.
- `CONVERT(value, type)` supports Pintail's scalar target types.
  `CONVERT(value USING charset)` distinguishes binary from character output,
  but does not perform byte-level transcoding among MySQL character sets.
- `information_schema` supports simple projection, aliases, case-insensitive
  `=`, `<>`, `IN`, `LIKE`, `IS NULL`, Boolean filters, ordering, limits, and
  `COUNT(*)` over the served tables; joins, aggregates beyond `COUNT(*)`,
  and metadata tables outside the served set (`schemata`, `tables`,
  `columns`, `statistics`, `key_column_usage`, `table_constraints`,
  `referential_constraints`) are deferred.

### MySQL semantic differences

- `ENUM` values compare and sort as their text, not as MySQL's
  declaration-index order; `CAST(col AS CHAR)` on the MySQL side produces
  matching orderings.
- Text comparison, grouping, hashing, `LIKE`, and ordering use a
  case-insensitive Unicode-lowercase approximation by default. Setting
  `PINTAIL_COLLATION=utf8mb4_0900_ai_ci` opts every text comparison into
  an accent-insensitive approximation of MySQL's default collation (NFD
  with combining marks stripped, then lowercased) — closer to
  `utf8mb4_0900_ai_ci` for Latin scripts, but still not the UCA weight
  tables, locale tailoring, coercibility rules, or pad-space behavior.
  The flag reads once at process start. Binary values remain bytewise.
- `NOW()`, `CURDATE()`, `CURTIME()`, and no-argument `UNIX_TIMESTAMP()` are
  pinned to one timestamp per statement, read from the host clock and
  timezone at plan time. Pintail does not yet expose a MySQL session
  timezone.
- Date parsing accepts the canonical date and date-time forms implemented by
  the M2 evaluator. `DATE_ADD` and `DATE_SUB` accept one interval field at a
  time; compound intervals and the full `DATE_FORMAT` directive inventory are
  not implemented.
- Pintail maps an empty scalar-subquery result to `NULL`. During oracle
  development, MySQL 8.4's constant `SELECT` with `LIMIT 0` produced a
  special-case result that did not follow this behavior; that MySQL-only
  corner is excluded from the common-workload corpus.
- Integer and floating arithmetic use Pintail's current `Int64`, `UInt64`,
  and `Float64` execution types. `DECIMAL` values are stored losslessly, and
  the operations MySQL keeps exact over exact numerics are exact here too:
  division (`/`) and `AVG` produce a DECIMAL widened by four fraction digits
  with half-away-from-zero rounding, `SUM` accumulates scaled integers, and
  CASE/IF/COALESCE branches that mix decimals with integers unify to a
  decimal instead of truncating, `+`/`-`/`*` over decimal columns, casts,
  and literals compute exactly on scaled units, and `CAST(x AS
  DECIMAL(p, s))` rounds half away from zero. Remaining gap: chained
  expressions whose intermediates are division results (`a / b / c`,
  `(a / b) * c`) round each step to its own result scale while MySQL
  carries extra unrounded digits between steps. Numeric overflow returns
  an error.

- `REPEAT`, `SPACE`, `LPAD`, and `RPAD` cap their result at 4096 bytes and
  error beyond it; MySQL's ceiling is `max_allowed_packet`. `FORMAT` uses
  en_US grouping only (no locale argument).

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
- Large overlapping scans merge key/version/tombstone headers and
  late-materialize winners in chunks of at most 8,192 rows. Views below
  65,536 candidate rows still use the simpler materialized merge path, which
  remains covered by the query memory ceiling.
- Hash joins, hash aggregation, sorting, distinct state, subquery
  materialization, retained projected scans, and cross joins obey a hard
  per-query memory cap. The cap is process-configurable but applies
  independently to every HTTP and MySQL-wire query. Sorting and the generic
  grouped aggregation spill to disk under the cap, but hash joins, DISTINCT
  state, GROUP_CONCAT/JSON_ARRAYAGG aggregation, the single-column
  direct-path aggregation, and materialized query outputs do not — those
  still fail at the ceiling. Spill files carry no disk quota. Cross joins
  also require catalog cardinalities and reject estimates above one million
  rows.
- Aggregate pushdown is intentionally conservative. M2 removes only
  unreferenced predicate-free cross-join inputs with an exact catalog
  cardinality of one; Pintail has no relationship or uniqueness statistics
  that would justify broader rewrites safely.
- `EXPLAIN ANALYZE` scan counters accumulate work from all executions of a
  stable table in the statement, including uncorrelated subqueries.

## Snapshot engine

- A missing `FLUSH TABLES WITH READ LOCK` privilege can be allowed explicitly.
  Every worker still uses a repeatable-read consistent transaction, but their
  start instants can differ and the result reports the degraded guarantee.
- Resume preserves the first attempt's CDC handoff position and replays
  already published chunks idempotently. A source changed between attempts
  can leave a mixed-time snapshot only until the mandatory post-snapshot CDC
  catch-up replays the overlap. On binlog-disabled sources, polling and
  reconciliation own that convergence.
- PK-less tables use a single-stream `LIMIT`/`OFFSET` scan and generated
  append-row IDs. Source changes between attempts can shift offsets; polling
  reconciliation is required because there is no stable source identity.
- Exact MySQL logical types and parameters are retained in schemas, while
  PTSEG v1 uses its existing physical carriers: narrow integers use 64-bit
  values, `Float32` uses the 64-bit float carrier, and decimal/temporal/JSON
  values use canonical UTF-8. DECIMAL values are lossless in storage, but M2
  query arithmetic still coerces them through the existing numeric executor
  and follows the exactness
  boundaries recorded in the M2 section above.
- `DECIMAL` precision above 38 maps to text with a probe warning. ENUM and SET
  snapshot values are textual. Virtual generated columns are skipped, while
  stored generated columns are included.
- Spatial columns are retained as binary WKB (without MySQL's four-byte SRID
  prefix), but Pintail has no spatial logical type, index, or query functions.
  They can be exported as bytes but cannot be used for spatial predicates.
- Progress row estimates use `information_schema.TABLES.TABLE_ROWS`, which is
  approximate for InnoDB. Durable completed row and chunk counts are exact.

## CDC engine

- The supervisor runs finite CDC catch-up cycles on a five-second cadence.
  Each runner retries eight consecutive connection failures with exponential
  backoff capped at five seconds; a later supervisor cycle retries a database
  that remains in error.
- MariaDB GTID text is captured and retained for diagnostics, but
  `mysql_common` 0.37 does not encode MariaDB's GTID dump request. MariaDB 11
  therefore resumes from the file/position captured alongside its GTID.
- Tables without a primary or safe UNIQUE key support idempotent INSERT CDC
  through deterministic append keys. UPDATE and DELETE have no stable source
  identity, so they enter the DLQ and mark that table `needs_resync`. MySQL 8
  GIPK tables have a real invisible key and support full CRUD.
- Binlog text transcoding currently covers utf8mb4/utf8mb3, ASCII, and MySQL
  latin1 (cp1252). Another source charset is quarantined through the DLQ
  instead of being silently interpreted.
- `binlog_row_metadata` may be MINIMAL (or absent, as on MySQL 5.7 and
  MariaDB): column identity is ordinal against the probed
  `information_schema` schema, enum/set labels and charsets come from the
  probed declarations, and unsigned integers are reinterpreted at their
  declared width because MINIMAL row events omit the SIGNEDNESS field. The
  hard CDC requirements remain `binlog_format=ROW` and
  `binlog_row_image=FULL`; a non-FULL row image demotes the source to
  polling.
- Versions reserve 16 bits for the intra-transaction mutation ordinal. GTID
  sequences must fit 48 bits. File/position versions support a 16-bit numeric
  file suffix and 32-bit event offset. A source transaction above 65,535
  physical mutations fails explicitly. Retained transaction data spills to an
  anonymous temporary file after the configurable in-memory threshold
  (256 MiB by default); the spill is intentionally ephemeral because the
  durable source checkpoint advances only after table WAL synchronization,
  so a crash safely replays the source transaction.
- Automatic purge recovery is deliberately database-wide and attempted once
  per runner invocation. It resets every included target because one global
  source coordinate cannot safely advance while a table retains an
  unfillable gap.
- Type fidelity covers snapshot and CDC storage plus HTTP and wire
  presentation for exact decimal text, valid and normalized-zero temporal
  values, negative TIME, JSON, Unicode, binary data, BIT values, Boolean
  values, and narrow integers.

- Persistent per-segment SMAs (manifest v2) answer bare
  COUNT/SUM/AVG/MIN/MAX without scanning while replication ingests, but
  only when the fold is provably exact: no tombstones, pairwise-disjoint
  segment key ranges, memtable strictly above the segment key space, no
  unique-key visibility, no predicates, no GROUP BY, no DISTINCT.
  Everything else scans normally. Grouped sub-cubes and predicate-covered
  blocks are deliberate follow-ups; v1 manifests (no SMAs) stay readable
  and simply decline the fold.

## DDL and polling
- Polling converges source state; it cannot reproduce intermediate states that
  exist entirely between cycles. Hard deletes on cursor tables remain visible
  until a scheduled key reconciliation, except when a secondary-UNIQUE
  collision triggers immediate targeted repair. Soft-delete mappings arrive
  through ordinary cursor sync.
- Count/MAX tokens are diagnostic only. Pintail still performs an inclusive
  cursor-boundary read, aggregate-chunk comparison, or append-generation check
  when the token is unchanged. This closes count-neutral and same-timestamp
  windows at the cost of source-side check queries on every scheduled sync.
- Source-key reconciliation uses composite-safe keyset pagination but currently
  materializes the full source and replica keysets in memory. Very large tables
  therefore need memory proportional to their key inventory until a
  bloom-assisted or partitioned anti-join is implemented.
- CDC-side cascade/SET NULL reconciliation compares complete source and replica
  rows so it can repair invisible payload updates as well as deletes. It
  currently materializes that table-sized comparison in memory.
- Cursor-less keyed checksums use ordered source chunks and MySQL-side CRC32
  aggregates. Inserts or deletes that shift chunk boundaries can cause adjacent
  chunks to be re-dumped; correctness is preserved, but repair work can exceed
  the number of rows that changed.
- Tables without a stable source key use append-generation replacement. They
  converge to the current source contents, but individual source UPDATE or
  DELETE identities and intermediate history are unknowable.
- The optional secondary-UNIQUE read policy uses Pintail's current
  case-insensitive Unicode-lowercase approximation. It does not reproduce the
  full MySQL collation matrix, binary/nonbinary coercibility, or pad-space
  behavior.
- Pure ADD COLUMN and DROP COLUMN events evolve live. Other ALTER operations,
  including rename, type/key changes, index-only changes, and default-only
  changes, conservatively mark that table `needs_resync` while unrelated
  tables continue.
- DDL catch-up re-probes the source after each observed query event. If several
  schema changes occur while Pintail is offline and the final source schema no
  longer represents an event's intermediate shape, Pintail quarantines an
  incompatible table rather than reconstructing missing historical layouts
  from SQL text. A table resnapshot is then required.
- Auto-inclusion of a new table uses case-insensitive exact allow/deny names
  and requires a writable target root. Glob patterns and dashboard rule editing
  arrive with the supervisor/API surface. DROP TABLE retains the replica as an
  orphan; M5 does not provide an operator purge action.

## HTTP API and dashboard

- HTTP and wire query responses share the same reader-pinned execution facade,
  row ceiling, memory ceiling, catalog construction, and physical scan
  counters. The HTTP surface serializes binary values as lowercase `0x` hex
  strings; JSON columns remain canonical JSON text rather than being silently
  retyped as nested response objects.
- The embedded dashboard is a local control plane, not a multi-tenant security
  boundary. Its first-boot admin and signed sessions protect operations, while
  network exposure and TLS remain deployment responsibilities.

## MySQL wire protocol

- The `caching_sha2_password` full-authentication fallback (RSA key exchange
  or cleartext-over-TLS) is not implemented; only the fast-auth exchange is
  served, which requires the verifier stored at key creation. Keys created
  before metadata schema version 13 must be rotated before use with
  caching_sha2_password clients; keys from before schema version 6 also lack
  the mysql_native_password verifier.
- The wire endpoint is read-only. Transaction boundaries and most `SET`
  variables are accepted for client compatibility without creating session
  state; the exceptions with real semantics are `SET time_zone` (shifts
  statement-pinned time functions; unknown zones error), `SET NAMES`
  (utf8-family charsets only, others error), and `SET sql_mode` (stored and
  echoed, no semantic effect). The session time zone does not affect
  `CONVERT_TZ` or stored temporal values. Multiple SQL statements in one
  command are not supported.
- Prepared result rows preserve MySQL numeric, decimal, temporal, JSON, text,
  and binary type tags. Prepared parameters support NULL, integers, floats,
  UTF-8 strings, binary strings, and binary DATE/DATETIME/TIME values
  (including negative and fractional-second times).
- The wire endpoint terminates TLS when configured
  (`PINTAIL_WIRE_TLS_CERT`/`PINTAIL_WIRE_TLS_KEY` PEM paths or the
  `[wire]` config keys, plus `PINTAIL_WIRE_REQUIRE_TLS` to refuse
  plaintext clients after the greeting). rustls with its default modern
  protocol and cipher policy; certificate rotation requires a restart.
  The HTTP endpoint still expects a TLS-capable ingress when exposed
  across a network.
- The automated compatibility gate runs `mysql_async`, MySQL 8.4 CLI, mysql2,
  and PyMySQL. DBeaver and Metabase use the same documented MySQL 8 connection
  profile, but their full application-level smokes are not automated on this
  workstation.

- Binary result columns carry `BINARY_FLAG`, but the wire library
  (`opensrv-mysql` 0.7) hardcodes column charset 33 (utf8) in result
  metadata, so clients that detect binary columns via charset 63 — mysql2,
  most connector libraries — decode raw binary bytes as text. The bytes on
  the wire are the exact stored value; clients honoring `BINARY_FLAG` or
  reading raw buffers receive them losslessly.

## Operations and backup

- The embedded supervisor is deliberately finite-cycle rather than a
  permanently attached stream. Its five-second cadence bounds idle resource
  ownership and source failure blast radius, but a newly committed event may
  wait for the next cycle before ingestion starts.
- Default size-tier maintenance admits at most 50,000 input rows per
  compaction pass and partitions output at 128,000 rows. A candidate above
  the admission limit remains as overlapping immutable segments and is
  resolved correctly by streaming merge-on-read. The current compaction-debt
  metric reports the next eligible plan, so it does not quantify an
  oversized deferred window. These storage limits are engine options rather
  than TOML/CLI settings in v1.
- RSS is obtained from the host `ps` process table. Sandboxed or minimal
  environments without a compatible `ps` command report zero rather than
  guessing. Storage and segment metrics walk the local data directory and can
  be comparatively expensive for very large deployments.
- DLQ retry performs a table reconciliation before removal. A database-level
  DLQ entry cannot be reconstructed from one row and requires a database
  resnapshot.
- S3-compatible backup credentials are encrypted at rest, but object-store
  authorization remains the operator's responsibility. Prefix validation
  prevents accidental cross-prefix writes; it is not tenant isolation.
- Backups do not yet apply an automatic retention policy. Incremental
  generations depend on their parent chain, so operators must retain every
  ancestor referenced by a manifest.
- Restore is intentionally side-by-side and detached. It does not recover or
  expose the encrypted source DSN and never overwrites an active replica.

## Duckling known-limit parity

The M9 audit covers every limitation named in Duckling's README and legacy
type-fidelity guide. “Inherited” means Pintail preserves the data but
deliberately lacks the higher-level operation for the stated architectural
reason; it does not mean silent corruption is accepted.

| Duckling limitation | Pintail status | Reason or evidence |
|---|---|---|
| PeerDB corrupts zero and minimum dates | Fixed | Native snapshot and CDC decoders normalize zero/partial-zero dates to `NULL` and retain `1000-01-01`; storage, HTTP, and wire gates assert both. |
| PeerDB rejects attachment to a pre-populated destination | Fixed by design | Snapshot and CDC are one native ownership path. The captured source position is persisted before handoff, so no external mirror attaches to Pintail's files. |
| Polling dumps are inconsistent under mid-dump writes | Fixed for CDC-capable sources | Snapshot workers use coordinated repeatable-read transactions and replay from the captured position. Without the global-lock privilege, Pintail reports a degraded cross-worker guarantee instead of hiding it. |
| Count-neutral delete/insert can evade polling | Fixed | Count/MAX tokens only schedule work; chunk checksums, key reconciliation, and secondary-UNIQUE audit detect the unchanged-count window. |
| `BIGINT UNSIGNED` overflows a signed carrier | Fixed | Pintail has a native `UInt64` value and segment carrier through the full `0..=2^64-1` range. |
| High-precision `DECIMAL` can truncate | Fixed in replication; inherited in arithmetic | Precision up to 38 is stored and returned as exact canonical text. Arithmetic and `SUM`/`AVG` currently use `Float64` because PTSEG v1 and the vectorized executor have no fixed-point arithmetic kernel. Queries requiring exact decimal math should aggregate in MySQL until that kernel is added. |
| Spatial/geometry values are unusable | Inherited | Pintail removes MySQL's SRID prefix and preserves WKB bytes, but the from-scratch engine intentionally has no spatial logical type, index, or functions yet. |
| Binary and BIT values break CDC updates | Fixed | Native row decoding stores binary values as bytes and BIT as unsigned values; snapshot, CDC, HTTP, and prepared-wire gates cover insert and update paths. |

## Release boundary

Pintail v1 is a single-node, read-only analytical replica. It does not provide
clustered query execution, synchronous high availability, source writes,
multi-tenant isolation, TLS termination, exact decimal arithmetic, spatial
querying, or query spill-to-disk. Those boundaries are explicit rather than
emulated with results that look plausible but may be wrong.
