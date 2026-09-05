# Changelog

All notable changes to Pintail are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- A local (writable) database refuses `BEGIN`, `START TRANSACTION`,
  `COMMIT`, `ROLLBACK`, `SAVEPOINT` and `SET autocommit=0` with MySQL
  error 1149 instead of accepting them as compatibility no-ops. The
  no-op told a client that a `ROLLBACK` had undone an `INSERT` whose row
  was durably stored - a wrong answer the client had no way to detect.
  Every statement on a local database is still its own autocommit
  transaction until explicit transactions land. Replicated databases are
  unchanged: they write nothing, so the no-op that lets drivers and BI
  tools open a transaction before a `SELECT` claims nothing false.

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
  the e2e gate 2,096 checks (from 1,829) including Chitti's vendored
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
