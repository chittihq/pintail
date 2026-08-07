# MySQL parity

What Pintail implements against MySQL 8.4. Gaps live in `docs/limitations.md`;
the two are disjoint on purpose.

The differential oracle (`tests/sqllogic/tests/mysql_oracle.rs`) contains 806
cases. All 806 pass byte-exactly against MySQL 8.4 in the full repository gate,
including the focused JSON, temporal-parsing, and DECIMAL-chain cases.

## Surface

| Area | Status |
|---|---|
| Callable functions | 134 — `bun run scripts/function-surface.ts` reads them from the binder, and a unit test holds this number to what it prints |
| Aggregates | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT`, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, `ANY_VALUE`, `STDDEV`/`STD`/`STDDEV_POP`/`STDDEV_SAMP`, `VARIANCE`/`VAR_POP`/`VAR_SAMP`, `BIT_AND`/`BIT_OR`/`BIT_XOR` |
| Window functions | `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `LAG`, `LEAD`, `NTILE`, `FIRST_VALUE`, `LAST_VALUE` |
| Window frames | explicit `ROWS BETWEEN` with all bound forms and the `ROWS n PRECEDING` shorthand; value-based `RANGE` bounds over numeric keys (including exact fractional DECIMAL offsets) and simple temporal `INTERVAL` offsets; `GROUPS` and DISTINCT window aggregates reject as MySQL 8.4 requires |
| Named windows | `WINDOW w AS (…)` referenced as `OVER w`; chained earlier definitions and legal additive inheritance without clause redefinition; forward/cyclic references and parenthesized inheritance from a framed base reject as MySQL requires |
| Joins | Inner, left, right (two-table), semi, anti; multi-key hash on `AND` of equalities; parenthesized root INNER/CROSS groups |
| Subqueries | Uncorrelated scalar/`IN`; correlated `EXISTS`/`IN` in the single-table equality form; correlated scalar aggregates and unique-key lookups, decorrelated to joins |
| Set operations | `UNION [ALL\|DISTINCT]`, `INTERSECT [ALL]`, `EXCEPT [ALL]` with exact `ALL` multiset counts |
| CTEs | Non-recursive; `WITH RECURSIVE` in the canonical `anchor UNION [ALL] member` form, with duplicate-eliminating fixpoints, query-memory accounting, and session `cte_max_recursion_depth` |
| JSON | Build: `JSON_OBJECT`, `JSON_ARRAY`, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, with JSON-vs-VARCHAR identity retained through execution and `MYSQL_TYPE_JSON` results. Read: single- and multi-path `JSON_EXTRACT`, `JSON_VALUE` (with `RETURNING`), `JSON_UNQUOTE`, `->`, `->>`. Inspect: `JSON_VALID`, `JSON_TYPE`, `JSON_LENGTH`, `JSON_KEYS`. Search: `JSON_CONTAINS`, `JSON_CONTAINS_PATH`, `JSON_SEARCH`. Unsupported JSON key semantics reject explicitly |
| Regex | `REGEXP_LIKE` with optional `match_type` (`c`/`i`/`m`/`n`/`u`), `REGEXP_INSTR`, `REGEXP_REPLACE`, `REGEXP_SUBSTR`, and the `REGEXP`/`RLIKE` operators including their `NOT` forms. POSIX bracket classes follow ICU's Unicode definitions; binary operands reject; query-owned literal programs and uncached dynamic programs obey pattern/program/query-memory limits |
| Conversion | `CAST`, `CONVERT(value, type)`, `CONVERT(value USING charset)` |
| `information_schema` | 8 tables, aliases, narrow INNER/LEFT/CROSS client-discovery joins, projection/filter/order/limit, grouping, and `COUNT`/`MIN`/`MAX`/`SUM` |

## Semantics

| Behaviour | Parity |
|---|---|
| Numeric literals | A dotted literal is `DECIMAL` (exact); an exponent literal is `DOUBLE` (approximate), as MySQL types them |
| `ROUND` | Half away from zero for exact operands, nearest-even for approximate ones — the mode follows the operand's type |
| `DECIMAL` arithmetic | Exact on scaled `i128` units, including comparison, hashing, grouping, DISTINCT, joins, IN, MIN/MAX and values beyond f64 precision. `/` and `AVG` widen by 4 fraction digits; chained arithmetic retains MySQL's base-1e9 internal division digits; overflow errors |
| `BIGINT UNSIGNED` | Native `UInt64` across the full `0..=2^64-1` range |
| Statement time | `NOW`/`CURDATE`/`CURTIME`/`UNIX_TIMESTAMP()` pinned to one timestamp per statement |
| Session time zone | `SET time_zone` per connection on the MySQL wire endpoint |
| `DATE_FORMAT` | Full directive inventory, including all four `WEEK` modes (`%U %u %V %v`) and paired years (`%X %x`) via a port of MySQL's `calc_week`; unknown directives copy their bare character |
| `WEEK(date, mode)` | All literal modes 0–7 via the same MySQL `calc_week` port |
| `EXTRACT` | `YEAR`, `MONTH`, `DAY`, `HOUR`, `MINUTE`, `SECOND`, `QUARTER`, `WEEK` |
| Temporal types | `DATE`/`DATETIME`/`TIMESTAMP`/`TIME` distinctions survive binding and wire metadata |
| Text collation | `utf8mb4_0900_ai_ci` compatibility profile uses one primary-strength Unicode collation key for comparison, grouping, hashing, DISTINCT, joins, IN, MIN/MAX, and ordering; LIKE/locate apply case/accent folding while binary values remain bytewise; unsupported or mixed source collations reject on collation-sensitive operations |
| Generated columns | Stored columns included; virtual skipped |
| Type fidelity | Exact decimal text, normalized-zero and negative temporals, JSON, Unicode, binary, `BIT`, Boolean, narrow integers — across snapshot, CDC, HTTP and wire |
| Rejection | Always an explicit error, never a different answer |

## Execution

| Behaviour | Parity |
|---|---|
| Memory | Hard per-query cap on joins, aggregation, sort, distinct, subquery materialization and cross joins |
| Spill | Sort, standalone DISTINCT, grouped aggregation and hash-join build sides spill into query-isolated directories; grace joins re-partition up to 3 times; per-query/global disk quotas, Prometheus counters and EXPLAIN ANALYZE counters are enforced |
| Parallelism | Segment header and late-materialization work on a Pintail-owned Rayon pool |
| Merge-on-read | Key/version/tombstone merge in chunks of ≤8,192 rows |
| Aggregate fold | Per-segment SMAs answer bare `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` without scanning when provably exact |
| Key pruning | Declared one-column `Int64`/`UInt64` key with a losslessly convertible literal |

## Wire protocol

| Behaviour | Parity |
|---|---|
| Mode | Read-only |
| Auth | `caching_sha2_password` fast-auth, `mysql_native_password` |
| Session vars | `SET time_zone`; `SET NAMES`/`character_set_results` (utf8 family and binary, including result metadata); `SET sql_mode` (echoed) |
| Prepared statements | Numeric, decimal, temporal, JSON, text, binary tags; params incl. binary `DATE`/`DATETIME`/`TIME`; type-derived length, session result charset, DECIMAL scale and temporal FSP metadata |
| TLS | rustls, default modern policy; `PINTAIL_WIRE_TLS_CERT`/`_KEY` PEM paths or `[wire]` config keys, `PINTAIL_WIRE_REQUIRE_TLS` to refuse plaintext |
| Client gate | `mysql_async`, MySQL 8.4 CLI, mysql2, PyMySQL |

## Ranked gaps

From `scripts/function-surface.ts` against `tests/corpus/bi-shapes.sql`.

| Function | Needed by | Issue |
|---|---|---|
| Compound intervals | Superset — blocked in sqlparser, not the engine | #13 |

`tests/corpus/bi-shapes.sql` is **reconstructed** from documented BI-tool
behaviour, not a captured query log. It establishes which functions are needed,
not how often. #24 tracks capturing a real log; replace this ranking with its
output rather than merging the two.

## Duckling known-limit parity

| Duckling limitation | Status | Evidence |
|---|---|---|
| PeerDB corrupts zero and minimum dates | Fixed | Decoders normalize zero/partial-zero dates to `NULL` and retain `1000-01-01` |
| PeerDB rejects a pre-populated destination | Fixed by design | One native ownership path; source position persisted before handoff |
| Polling dumps inconsistent under mid-dump writes | Fixed for CDC sources | Coordinated repeatable-read transactions; degraded guarantee reported, not hidden |
| Count-neutral delete/insert evades polling | Fixed | Chunk checksums, key reconciliation and secondary-UNIQUE audit |
| `BIGINT UNSIGNED` overflows a signed carrier | Fixed | Native `UInt64` value and segment carrier |
| High-precision `DECIMAL` truncates | Fixed | Exact to precision 38 on scaled integer units |
| Spatial values unusable | Inherited | WKB bytes preserved; no spatial type, index or functions |
| Binary and BIT break CDC updates | Fixed | Native row decoding; snapshot, CDC, HTTP and wire gates |

## CDC mode versus polling mode

Polling sources are second-class by construction: without a binlog there is no
record of what happened between two reads.

| Guarantee | CDC | Polling |
|---|---|---|
| Transaction atomicity | Visible together at the XID boundary | None — a cycle can expose part of a transaction |
| Intermediate states | Every row version reaches the replica | Lost; only cycle-boundary state is observed |
| Hard deletes | Tombstones from the binlog, seconds behind | Invisible until key reconciliation (default 10 min) |
| Cascaded deletes | Reconciler on the database's interval | Same reconciler, same cadence |
| Secondary UNIQUE collisions | Cannot occur | Transient on delete-then-reuse; audit repairs in seconds |
| Soft deletes | Ordinary row updates | Ordinary cursor sync, converge in seconds |
| Cursor-less tables | n/a | Chunk-checksum sync |
| PK uniqueness | Merge-on-read, always on | Identical |
| Cross-table ordering | One binlog position | None |

The dashboard carries a persistent polling-mode banner stating the delete
latency and intermediate-state loss, so the trade is visible to whoever reads
the data rather than only to whoever configured it.
