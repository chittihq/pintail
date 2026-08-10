# MySQL parity

What Pintail implements against MySQL 8.4. Gaps live in `docs/limitations.md`;
the two are disjoint on purpose.

The differential oracle (`tests/sqllogic/tests/mysql_oracle.rs`) contains 874
cases, all passing byte-exactly against MySQL 8.4 in the current repository
gate, including the focused JSON, temporal-parsing, DECIMAL-chain,
dependent-correlation, bushy-join, and set-scoping cases.

## Surface

| Area | Status |
|---|---|
| Callable functions | 135 — `bun run scripts/function-surface.ts` reads them from the binder, and a unit test holds this number to what it prints |
| Aggregates | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP_CONCAT`, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, `ANY_VALUE`, `STDDEV`/`STD`/`STDDEV_POP`/`STDDEV_SAMP`, `VARIANCE`/`VAR_POP`/`VAR_SAMP`, `BIT_AND`/`BIT_OR`/`BIT_XOR` |
| Window functions | `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `LAG`, `LEAD`, `NTILE`, `FIRST_VALUE`, `LAST_VALUE` |
| Window frames | explicit `ROWS BETWEEN` with all bound forms and the `ROWS n PRECEDING` shorthand; value-based `RANGE` bounds over numeric keys (including exact fractional DECIMAL offsets) and simple temporal `INTERVAL` offsets; `GROUPS` and DISTINCT window aggregates reject as MySQL 8.4 requires |
| Named windows | `WINDOW w AS (…)` referenced as `OVER w`; chained earlier definitions and legal additive inheritance without clause redefinition; forward/cyclic references and parenthesized inheritance from a framed base reject as MySQL requires |
| Joins | Inner, left, right (two-table), semi, anti; multi-key hash on `AND` of equalities; parenthesized root and bushy right-side INNER/CROSS/LEFT groups; bounded nested-loop evaluation for subqueries in `ON` |
| Subqueries | Uncorrelated scalar/`IN`; canonical correlated `EXISTS`/`IN`, scalar aggregates, and unique-key lookups decorrelated to joins; bounded dependent execution for wider and nested correlations in projection, filtering, HAVING, join `ON`, CTEs, and derived tables |
| Set operations | `UNION [ALL\|DISTINCT]`, `INTERSECT [ALL]`, `EXCEPT [ALL]` with exact `ALL` multiset counts, MySQL precedence, parenthesized operands, and branch-local `ORDER BY`/`LIMIT` |
| CTEs | Non-recursive; `WITH RECURSIVE` in the canonical `anchor UNION [ALL] member` form, with duplicate-eliminating fixpoints, query-memory accounting, and session `cte_max_recursion_depth` |
| JSON | Build: `JSON_OBJECT`, `JSON_ARRAY`, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, with JSON-vs-VARCHAR identity retained through execution and `MYSQL_TYPE_JSON` results. Read: single- and multi-path `JSON_EXTRACT`, `JSON_VALUE` (with `RETURNING`), `JSON_UNQUOTE`, `->`, `->>`. Inspect: `JSON_VALID`, `JSON_TYPE`, `JSON_LENGTH`, `JSON_KEYS`. Search: `JSON_CONTAINS`, `JSON_CONTAINS_PATH`, `JSON_SEARCH`. Unsupported JSON key semantics reject explicitly |
| Regex | `REGEXP_LIKE` with optional `match_type` (`c`/`i`/`m`/`n`/`u`), `REGEXP_INSTR`, `REGEXP_REPLACE`, `REGEXP_SUBSTR`, and the `REGEXP`/`RLIKE` operators including their `NOT` forms. POSIX bracket classes follow ICU's Unicode definitions; binary operands reject; query-owned literal programs and uncached dynamic programs obey pattern/program/query-memory limits |
| Conversion | `CAST`, `CONVERT(value, type)`, `CONVERT(value USING charset)` |
| `information_schema` | 10 relations (`SCHEMATA`, `TABLES`, `COLUMNS`, `STATISTICS`, `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS`, `REFERENTIAL_CONSTRAINTS`, `CHECK_CONSTRAINTS`, `ROUTINES`, `VIEWS`), aliases, narrow INNER/LEFT/CROSS client-discovery joins, projection/filter/order/limit/DISTINCT, grouping, and `COUNT`/`MIN`/`MAX`/`SUM` |

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
| Text collation | `utf8mb4_0900_ai_ci` compatibility profile uses one primary-strength Unicode collation key for comparison, grouping, hashing, DISTINCT, joins, IN, MIN/MAX, and ordering; explicit `COLLATE utf8mb4_0900_ai_ci` and its NO PAD trailing-space behavior are supported; LIKE/locate apply case/accent folding while binary values remain bytewise; unsupported or mixed source collations reject on collation-sensitive operations |
| Generated columns | Stored columns included; virtual skipped |
| Type fidelity | Exact decimal text, normalized-zero and negative temporals, JSON, Unicode, binary, `BIT`, Boolean, narrow integers — across snapshot, CDC, HTTP and wire |
| Rejection | Always an explicit error, never a different answer |

## Execution

| Behaviour | Parity |
|---|---|
| Memory | Hard per-query cap on joins, aggregation, sort, distinct, subquery materialization and cross joins |
| Spill | Sort, standalone DISTINCT, `INTERSECT`/`EXCEPT`, grouped aggregation and hash-join build sides spill into query-isolated directories; grace joins re-partition up to 3 times; per-query/global disk quotas, Prometheus counters and EXPLAIN ANALYZE counters are enforced |
| Parallelism | Segment header and late-materialization work on a Pintail-owned Rayon pool |
| Merge-on-read | Key/version/tombstone merge in chunks of ≤8,192 rows |
| Aggregate fold | Per-segment SMAs answer bare `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` without scanning when provably exact |
| Key pruning | Declared one-column `Int64`/`UInt64` key with a losslessly convertible literal |

## Wire protocol

| Behaviour | Parity |
|---|---|
| Mode | Read-only |
| Auth | `caching_sha2_password` fast-auth, `mysql_native_password` |
| Session vars | `SET time_zone`; `SET NAMES`/`character_set_results` (utf8 family and binary, including result metadata); `SET sql_mode` (echoed); `SET max_execution_time` (cooperative millisecond deadline, error 1317) |
| Prepared statements | Numeric, decimal, temporal, JSON, text, binary tags; params incl. binary `DATE`/`DATETIME`/`TIME`; type-derived length, session result charset, DECIMAL scale and temporal FSP metadata |
| TLS | rustls, default modern policy; `PINTAIL_WIRE_TLS_CERT`/`_KEY` PEM paths or `[wire]` config keys, `PINTAIL_WIRE_REQUIRE_TLS` to refuse plaintext |
| Client gate | `mysql_async`, MySQL 8.4 CLI, mysql2, PyMySQL, Go `database/sql`/go-sql-driver/mysql with parameter interpolation |

## Ranked gaps

From `scripts/function-surface.ts` against `tests/corpus/bi-shapes.sql`.

| Function | Needed by | Issue |
|---|---|---|
| Compound intervals | Superset — blocked in sqlparser, not the engine | #13 |

`tests/corpus/bi-shapes.sql` is **reconstructed** from documented BI-tool
behaviour, not a captured query log. It establishes which functions are needed,
not how often. Capturing production dashboard traffic is not a release
requirement: Pintail targets the documented MySQL-compatible SQL surface, not
tool-specific integrations. An optional local-only capture, redaction, and
dual-engine replay utility remains documented in
`tests/corpus/bi-captured/README.md` for diagnostics.

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

## MySQL keyword and function matrix

Generated by `bun run scripts/compatibility-matrix.ts`. Every column is
read from a live inventory rather than written from memory, because a
compatibility matrix is the artifact people migrate on:

| Column | Source |
|---|---|
| MySQL keywords | `information_schema.KEYWORDS` on MySQL 8.4 |
| MySQL functions | `mysql.help_topic` joined to its Function/Operator categories — MySQL's own documentation catalogue |
| ClickHouse | `system.functions` and `system.keywords` on `clickhouse/clickhouse-server:25.8`, matched case-insensitively so its MySQL-compatible aliases count |
| Pintail functions | the binder's own match arms, the same source `scripts/function-surface.ts` reads |
| Pintail keywords | **curated, not machine-read** — the binder has no keyword table, it either binds a construct or rejects it |

There is no MySQL support column: this is MySQL's own keyword and function
inventory, so every row would read "yes" and the column would carry no
information.

| Mark | Meaning |
|---|---|
| ✅ | callable or accepted by this exact MySQL name |
| ❌ | not callable by this MySQL name |
| ➖ | out of scope by design — a read-only replica cannot encounter it |

In the **MySQL reserved** column ✅ means the word is reserved in MySQL
8.4, not that anything supports it. Support is only ever the Pintail and
ClickHouse columns.

**The ClickHouse column measures the name, not the capability.** ClickHouse
implements much of this surface under different spellings: it answers `no`
to `JSON_EXTRACT` while shipping 28 `JSONExtract*` functions, and `no` to
`DATE_ADD` while shipping `date_diff` and the `toYear`/`toMonth` family. A
`no` here means "not callable by the MySQL name", which is what matters for
pointing an existing MySQL client at it - not "cannot do this".

➖ marks a keyword a read-only analytical replica cannot encounter by
design — DDL, DML writes, replication and administration. Those are out of
scope rather than missing, and counting them as gaps would make this table
read as far worse than the engine is.

**Functions:** 392 MySQL functions — Pintail 130, ClickHouse 151.

**Keywords:** 734 MySQL keywords — Pintail 95 supported and 123 out of scope, ClickHouse 208.

### Functions

| Function | Pintail | ClickHouse |
|---|---|---|
| `ABS` | ✅ | ✅ |
| `ACOS` | ❌ | ✅ |
| `ADDDATE` | ❌ | ✅ |
| `ADDTIME` | ❌ | ❌ |
| `AES_DECRYPT` | ❌ | ❌ |
| `AES_ENCRYPT` | ❌ | ❌ |
| `AND` | ❌ | ✅ |
| `ANY_VALUE` | ✅ | ✅ |
| `ASCII` | ✅ | ✅ |
| `ASIN` | ❌ | ✅ |
| `ASYMMETRIC_DECRYPT` | ❌ | ❌ |
| `ASYMMETRIC_ENCRYPT` | ❌ | ❌ |
| `ASYMMETRIC_SIGN` | ❌ | ❌ |
| `ASYMMETRIC_VERIFY` | ❌ | ❌ |
| `ATAN` | ❌ | ✅ |
| `ATAN2` | ❌ | ✅ |
| `AVG` | ✅ | ✅ |
| `BENCHMARK` | ❌ | ❌ |
| `BIN` | ❌ | ✅ |
| `BIN_TO_UUID` | ❌ | ❌ |
| `BIT_AND` | ✅ | ✅ |
| `BIT_COUNT` | ❌ | ❌ |
| `BIT_LENGTH` | ❌ | ❌ |
| `BIT_OR` | ✅ | ✅ |
| `BIT_XOR` | ✅ | ✅ |
| `CAN_ACCESS_COLUMN` | ❌ | ❌ |
| `CAN_ACCESS_DATABASE` | ❌ | ❌ |
| `CAN_ACCESS_TABLE` | ❌ | ❌ |
| `CAN_ACCESS_USER` | ❌ | ❌ |
| `CAN_ACCESS_VIEW` | ❌ | ❌ |
| `CAST` | ❌ | ✅ |
| `CEIL` | ✅ | ✅ |
| `CEILING` | ✅ | ✅ |
| `CHARACTER_LENGTH` | ✅ | ✅ |
| `CHARSET` | ❌ | ❌ |
| `CHAR_LENGTH` | ✅ | ✅ |
| `COALESCE` | ✅ | ✅ |
| `COERCIBILITY` | ❌ | ❌ |
| `COLLATION` | ❌ | ❌ |
| `COMPRESS` | ❌ | ❌ |
| `CONCAT` | ✅ | ✅ |
| `CONCAT_WS` | ✅ | ✅ |
| `CONNECTION_ID` | ❌ | ✅ |
| `CONV` | ✅ | ❌ |
| `CONVERT` | ❌ | ❌ |
| `CONVERT_TZ` | ✅ | ❌ |
| `COS` | ❌ | ✅ |
| `COT` | ❌ | ❌ |
| `COUNT` | ✅ | ✅ |
| `CRC32` | ❌ | ✅ |
| `CREATE_ASYMMETRIC_PRIV_KEY` | ❌ | ❌ |
| `CREATE_ASYMMETRIC_PUB_KEY` | ❌ | ❌ |
| `CREATE_DIGEST` | ❌ | ❌ |
| `CUME_DIST` | ❌ | ❌ |
| `CURDATE` | ✅ | ✅ |
| `CURRENT_DATE` | ❌ | ✅ |
| `CURRENT_ROLE` | ❌ | ❌ |
| `CURRENT_TIME` | ✅ | ❌ |
| `CURRENT_TIMESTAMP` | ❌ | ✅ |
| `CURRENT_USER` | ❌ | ✅ |
| `CURTIME` | ✅ | ❌ |
| `DATABASE` | ❌ | ✅ |
| `DATEDIFF` | ✅ | ✅ |
| `DATE_ADD` | ✅ | ❌ |
| `DATE_FORMAT` | ✅ | ✅ |
| `DATE_SUB` | ✅ | ❌ |
| `DAY` | ✅ | ✅ |
| `DAYNAME` | ✅ | ❌ |
| `DAYOFMONTH` | ✅ | ✅ |
| `DAYOFWEEK` | ✅ | ✅ |
| `DAYOFYEAR` | ✅ | ✅ |
| `DEFAULT` | ❌ | ❌ |
| `DEGREES` | ❌ | ✅ |
| `DENSE_RANK` | ✅ | ✅ |
| `DIV` | ❌ | ❌ |
| `ELT` | ✅ | ❌ |
| `EXISTS` | ❌ | ❌ |
| `EXP` | ✅ | ✅ |
| `EXPORT_SET` | ❌ | ❌ |
| `EXTRACT` | ❌ | ✅ |
| `FIELD` | ✅ | ❌ |
| `FIND_IN_SET` | ✅ | ❌ |
| `FIRST_VALUE` | ❌ | ✅ |
| `FLOOR` | ✅ | ✅ |
| `FORMAT` | ✅ | ✅ |
| `FORMAT_BYTES` | ❌ | ✅ |
| `FORMAT_PICO_TIME` | ❌ | ❌ |
| `FOUND_ROWS` | ❌ | ❌ |
| `FROM_BASE64` | ✅ | ✅ |
| `FROM_DAYS` | ✅ | ✅ |
| `FROM_UNIXTIME` | ✅ | ✅ |
| `GET_DD_COLUMN_PRIVILEGES` | ❌ | ❌ |
| `GET_DD_CREATE_OPTIONS` | ❌ | ❌ |
| `GET_DD_INDEX_SUB_PART_LENGTH` | ❌ | ❌ |
| `GET_FORMAT` | ❌ | ❌ |
| `GET_LOCK` | ❌ | ❌ |
| `GREATEST` | ✅ | ✅ |
| `GROUPING` | ❌ | ❌ |
| `GROUP_CONCAT` | ✅ | ✅ |
| `HEX` | ✅ | ✅ |
| `HOUR` | ✅ | ✅ |
| `ICU_VERSION` | ❌ | ❌ |
| `IFNULL` | ✅ | ✅ |
| `IN` | ❌ | ✅ |
| `INET6_ATON` | ❌ | ✅ |
| `INET6_NTOA` | ❌ | ✅ |
| `INET_ATON` | ❌ | ✅ |
| `INET_NTOA` | ❌ | ✅ |
| `INSTR` | ✅ | ✅ |
| `INTERNAL_AUTO_INCREMENT` | ❌ | ❌ |
| `INTERNAL_AVG_ROW_LENGTH` | ❌ | ❌ |
| `INTERNAL_CHECKSUM` | ❌ | ❌ |
| `INTERNAL_CHECK_TIME` | ❌ | ❌ |
| `INTERNAL_DATA_FREE` | ❌ | ❌ |
| `INTERNAL_DATA_LENGTH` | ❌ | ❌ |
| `INTERNAL_DD_CHAR_LENGTH` | ❌ | ❌ |
| `INTERNAL_GET_COMMENT_OR_ERROR` | ❌ | ❌ |
| `INTERNAL_GET_ENABLED_ROLE_JSON` | ❌ | ❌ |
| `INTERNAL_GET_HOSTNAME` | ❌ | ❌ |
| `INTERNAL_GET_USERNAME` | ❌ | ❌ |
| `INTERNAL_GET_VIEW_WARNING_OR_ERROR` | ❌ | ❌ |
| `INTERNAL_INDEX_COLUMN_CARDINALITY` | ❌ | ❌ |
| `INTERNAL_INDEX_LENGTH` | ❌ | ❌ |
| `INTERNAL_IS_ENABLED_ROLE` | ❌ | ❌ |
| `INTERNAL_IS_MANDATORY_ROLE` | ❌ | ❌ |
| `INTERNAL_KEYS_DISABLED` | ❌ | ❌ |
| `INTERNAL_MAX_DATA_LENGTH` | ❌ | ❌ |
| `INTERNAL_TABLE_ROWS` | ❌ | ❌ |
| `INTERNAL_UPDATE_TIME` | ❌ | ❌ |
| `INTERVAL` | ❌ | ❌ |
| `IS` | ❌ | ❌ |
| `ISNULL` | ❌ | ✅ |
| `IS_FREE_LOCK` | ❌ | ❌ |
| `IS_IPV4` | ❌ | ❌ |
| `IS_IPV4_COMPAT` | ❌ | ❌ |
| `IS_IPV4_MAPPED` | ❌ | ❌ |
| `IS_IPV6` | ❌ | ❌ |
| `IS_USED_LOCK` | ❌ | ❌ |
| `IS_UUID` | ❌ | ❌ |
| `IS_VISIBLE_DD_OBJECT` | ❌ | ❌ |
| `JSON_ARRAY` | ✅ | ❌ |
| `JSON_ARRAYAGG` | ✅ | ❌ |
| `JSON_ARRAY_APPEND` | ❌ | ❌ |
| `JSON_ARRAY_INSERT` | ❌ | ❌ |
| `JSON_CONTAINS` | ✅ | ❌ |
| `JSON_CONTAINS_PATH` | ✅ | ❌ |
| `JSON_DEPTH` | ❌ | ❌ |
| `JSON_EXTRACT` | ✅ | ❌ |
| `JSON_INSERT` | ❌ | ❌ |
| `JSON_KEYS` | ✅ | ❌ |
| `JSON_LENGTH` | ✅ | ❌ |
| `JSON_MERGE` | ❌ | ❌ |
| `JSON_OBJECT` | ✅ | ❌ |
| `JSON_OBJECTAGG` | ✅ | ❌ |
| `JSON_OVERLAPS` | ❌ | ❌ |
| `JSON_PRETTY` | ❌ | ❌ |
| `JSON_QUOTE` | ❌ | ❌ |
| `JSON_REMOVE` | ❌ | ❌ |
| `JSON_REPLACE` | ❌ | ❌ |
| `JSON_SCHEMA_VALID` | ❌ | ❌ |
| `JSON_SCHEMA_VALIDATION_REPORT` | ❌ | ❌ |
| `JSON_SEARCH` | ✅ | ❌ |
| `JSON_SET` | ❌ | ❌ |
| `JSON_STORAGE_FREE` | ❌ | ❌ |
| `JSON_STORAGE_SIZE` | ❌ | ❌ |
| `JSON_TABLE` | ❌ | ❌ |
| `JSON_TYPE` | ✅ | ❌ |
| `JSON_UNQUOTE` | ✅ | ❌ |
| `JSON_VALID` | ✅ | ❌ |
| `JSON_VALUE` | ✅ | ✅ |
| `LAG` | ❌ | ✅ |
| `LAST_DAY` | ✅ | ✅ |
| `LAST_INSERT_ID` | ❌ | ❌ |
| `LAST_VALUE` | ❌ | ✅ |
| `LCASE` | ✅ | ✅ |
| `LEAD` | ❌ | ✅ |
| `LEAST` | ✅ | ✅ |
| `LEFT` | ✅ | ✅ |
| `LENGTH` | ✅ | ✅ |
| `LIKE` | ❌ | ✅ |
| `LN` | ✅ | ✅ |
| `LOAD_FILE` | ❌ | ❌ |
| `LOCALTIME` | ❌ | ❌ |
| `LOCALTIMESTAMP` | ❌ | ❌ |
| `LOCATE` | ✅ | ✅ |
| `LOG` | ✅ | ✅ |
| `LOG10` | ✅ | ✅ |
| `LOG2` | ✅ | ✅ |
| `LOWER` | ✅ | ✅ |
| `LPAD` | ✅ | ✅ |
| `LTRIM` | ❌ | ✅ |
| `MAKEDATE` | ✅ | ✅ |
| `MAKETIME` | ✅ | ❌ |
| `MAKE_SET` | ❌ | ❌ |
| `MAX` | ✅ | ✅ |
| `MBRCONTAINS` | ❌ | ❌ |
| `MBRCOVEREDBY` | ❌ | ❌ |
| `MBRCOVERS` | ❌ | ❌ |
| `MBRDISJOINT` | ❌ | ❌ |
| `MBREQUALS` | ❌ | ❌ |
| `MBRINTERSECTS` | ❌ | ❌ |
| `MBROVERLAPS` | ❌ | ❌ |
| `MBRTOUCHES` | ❌ | ❌ |
| `MBRWITHIN` | ❌ | ❌ |
| `MD5` | ✅ | ✅ |
| `MICROSECOND` | ❌ | ❌ |
| `MID` | ❌ | ✅ |
| `MIN` | ✅ | ✅ |
| `MINUTE` | ✅ | ✅ |
| `MOD` | ✅ | ✅ |
| `MONTH` | ✅ | ✅ |
| `MONTHNAME` | ✅ | ✅ |
| `NAME_CONST` | ❌ | ❌ |
| `NOW` | ✅ | ✅ |
| `NTH_VALUE` | ❌ | ✅ |
| `NTILE` | ❌ | ✅ |
| `NULLIF` | ✅ | ✅ |
| `OCT` | ❌ | ❌ |
| `OCTET_LENGTH` | ❌ | ✅ |
| `OR` | ❌ | ✅ |
| `ORD` | ✅ | ❌ |
| `PERCENT_RANK` | ❌ | ✅ |
| `PERIOD_ADD` | ❌ | ❌ |
| `PERIOD_DIFF` | ❌ | ❌ |
| `PI` | ❌ | ✅ |
| `POSITION` | ❌ | ✅ |
| `POW` | ✅ | ✅ |
| `POWER` | ✅ | ✅ |
| `PS_CURRENT_THREAD_ID` | ❌ | ❌ |
| `PS_THREAD_ID` | ❌ | ❌ |
| `QUARTER` | ✅ | ✅ |
| `QUOTE` | ❌ | ❌ |
| `RADIANS` | ❌ | ✅ |
| `RAND` | ✅ | ✅ |
| `RANDOM_BYTES` | ❌ | ❌ |
| `RANK` | ✅ | ✅ |
| `REGEXP` | ❌ | ❌ |
| `REGEXP_INSTR` | ✅ | ❌ |
| `REGEXP_LIKE` | ✅ | ❌ |
| `REGEXP_REPLACE` | ✅ | ✅ |
| `REGEXP_SUBSTR` | ✅ | ❌ |
| `RELEASE_ALL_LOCKS` | ❌ | ❌ |
| `RELEASE_LOCK` | ❌ | ❌ |
| `REVERSE` | ✅ | ✅ |
| `RIGHT` | ✅ | ✅ |
| `ROLES_GRAPHML` | ❌ | ❌ |
| `ROUND` | ✅ | ✅ |
| `ROW_COUNT` | ❌ | ❌ |
| `ROW_NUMBER` | ✅ | ✅ |
| `RPAD` | ✅ | ✅ |
| `RTRIM` | ❌ | ✅ |
| `SCHEMA` | ❌ | ✅ |
| `SECOND` | ✅ | ✅ |
| `SEC_TO_TIME` | ✅ | ❌ |
| `SESSION_USER` | ❌ | ❌ |
| `SHA1` | ❌ | ✅ |
| `SHA2` | ❌ | ❌ |
| `SIGN` | ✅ | ✅ |
| `SIN` | ❌ | ✅ |
| `SLEEP` | ❌ | ✅ |
| `SOUNDEX` | ❌ | ✅ |
| `SPACE` | ✅ | ✅ |
| `SQRT` | ✅ | ✅ |
| `STATEMENT_DIGEST` | ❌ | ❌ |
| `STATEMENT_DIGEST_TEXT` | ❌ | ❌ |
| `STD` | ✅ | ✅ |
| `STDDEV` | ✅ | ❌ |
| `STDDEV_POP` | ✅ | ✅ |
| `STDDEV_SAMP` | ✅ | ✅ |
| `STRCMP` | ❌ | ❌ |
| `STR_TO_DATE` | ✅ | ✅ |
| `ST_AREA` | ❌ | ❌ |
| `ST_ASBINARY` | ❌ | ❌ |
| `ST_ASGEOJSON` | ❌ | ❌ |
| `ST_ASTEXT` | ❌ | ❌ |
| `ST_BUFFER` | ❌ | ❌ |
| `ST_BUFFER_STRATEGY` | ❌ | ❌ |
| `ST_CENTROID` | ❌ | ❌ |
| `ST_COLLECT` | ❌ | ❌ |
| `ST_CONTAINS` | ❌ | ❌ |
| `ST_CONVEXHULL` | ❌ | ❌ |
| `ST_CROSSES` | ❌ | ❌ |
| `ST_DIFFERENCE` | ❌ | ❌ |
| `ST_DIMENSION` | ❌ | ❌ |
| `ST_DISJOINT` | ❌ | ❌ |
| `ST_DISTANCE` | ❌ | ❌ |
| `ST_DISTANCE_SPHERE` | ❌ | ❌ |
| `ST_ENDPOINT` | ❌ | ❌ |
| `ST_ENVELOPE` | ❌ | ❌ |
| `ST_EQUALS` | ❌ | ❌ |
| `ST_EXTERIORRING` | ❌ | ❌ |
| `ST_FRECHETDISTANCE` | ❌ | ❌ |
| `ST_GEOHASH` | ❌ | ❌ |
| `ST_GEOMCOLLFROMTEXT` | ❌ | ❌ |
| `ST_GEOMCOLLFROMWKB` | ❌ | ❌ |
| `ST_GEOMETRYN` | ❌ | ❌ |
| `ST_GEOMETRYTYPE` | ❌ | ❌ |
| `ST_GEOMFROMGEOJSON` | ❌ | ❌ |
| `ST_GEOMFROMTEXT` | ❌ | ❌ |
| `ST_GEOMFROMWKB` | ❌ | ❌ |
| `ST_HAUSDORFFDISTANCE` | ❌ | ❌ |
| `ST_INTERIORRINGN` | ❌ | ❌ |
| `ST_INTERSECTION` | ❌ | ❌ |
| `ST_INTERSECTS` | ❌ | ❌ |
| `ST_ISCLOSED` | ❌ | ❌ |
| `ST_ISEMPTY` | ❌ | ❌ |
| `ST_ISSIMPLE` | ❌ | ❌ |
| `ST_ISVALID` | ❌ | ❌ |
| `ST_LATFROMGEOHASH` | ❌ | ❌ |
| `ST_LATITUDE` | ❌ | ❌ |
| `ST_LENGTH` | ❌ | ❌ |
| `ST_LINEFROMTEXT` | ❌ | ❌ |
| `ST_LINEFROMWKB` | ❌ | ✅ |
| `ST_LINEINTERPOLATEPOINT` | ❌ | ❌ |
| `ST_LINEINTERPOLATEPOINTS` | ❌ | ❌ |
| `ST_LONGFROMGEOHASH` | ❌ | ❌ |
| `ST_LONGITUDE` | ❌ | ❌ |
| `ST_MAKEENVELOPE` | ❌ | ❌ |
| `ST_MLINEFROMTEXT` | ❌ | ❌ |
| `ST_MLINEFROMWKB` | ❌ | ✅ |
| `ST_MPOINTFROMTEXT` | ❌ | ❌ |
| `ST_MPOINTFROMWKB` | ❌ | ❌ |
| `ST_MPOLYFROMTEXT` | ❌ | ❌ |
| `ST_MPOLYFROMWKB` | ❌ | ✅ |
| `ST_NUMGEOMETRIES` | ❌ | ❌ |
| `ST_NUMINTERIORRINGS` | ❌ | ❌ |
| `ST_NUMPOINTS` | ❌ | ❌ |
| `ST_OVERLAPS` | ❌ | ❌ |
| `ST_POINTATDISTANCE` | ❌ | ❌ |
| `ST_POINTFROMGEOHASH` | ❌ | ❌ |
| `ST_POINTFROMTEXT` | ❌ | ❌ |
| `ST_POINTFROMWKB` | ❌ | ✅ |
| `ST_POINTN` | ❌ | ❌ |
| `ST_POLYFROMTEXT` | ❌ | ❌ |
| `ST_POLYFROMWKB` | ❌ | ✅ |
| `ST_SIMPLIFY` | ❌ | ❌ |
| `ST_SRID` | ❌ | ❌ |
| `ST_STARTPOINT` | ❌ | ❌ |
| `ST_SWAPXY` | ❌ | ❌ |
| `ST_SYMDIFFERENCE` | ❌ | ❌ |
| `ST_TOUCHES` | ❌ | ❌ |
| `ST_TRANSFORM` | ❌ | ❌ |
| `ST_UNION` | ❌ | ❌ |
| `ST_VALIDATE` | ❌ | ❌ |
| `ST_WITHIN` | ❌ | ❌ |
| `ST_X` | ❌ | ❌ |
| `ST_Y` | ❌ | ❌ |
| `SUBDATE` | ❌ | ✅ |
| `SUBSTR` | ✅ | ✅ |
| `SUBSTRING` | ✅ | ✅ |
| `SUBSTRING_INDEX` | ✅ | ✅ |
| `SUBTIME` | ❌ | ❌ |
| `SUM` | ✅ | ✅ |
| `SYSDATE` | ❌ | ❌ |
| `SYSTEM_USER` | ❌ | ❌ |
| `TAN` | ❌ | ✅ |
| `TIMEDIFF` | ❌ | ✅ |
| `TIMESTAMPADD` | ✅ | ❌ |
| `TIMESTAMPDIFF` | ✅ | ✅ |
| `TIME_FORMAT` | ❌ | ❌ |
| `TIME_TO_SEC` | ✅ | ❌ |
| `TO_BASE64` | ✅ | ✅ |
| `TO_DAYS` | ✅ | ✅ |
| `TO_SECONDS` | ❌ | ❌ |
| `TRIM` | ✅ | ✅ |
| `TRUNCATE` | ✅ | ✅ |
| `UCASE` | ✅ | ✅ |
| `UNCOMPRESS` | ❌ | ❌ |
| `UNCOMPRESSED_LENGTH` | ❌ | ❌ |
| `UNHEX` | ✅ | ✅ |
| `UNIX_TIMESTAMP` | ✅ | ❌ |
| `UPPER` | ✅ | ✅ |
| `USER` | ❌ | ✅ |
| `UTC_DATE` | ❌ | ❌ |
| `UTC_TIME` | ❌ | ❌ |
| `UTC_TIMESTAMP` | ❌ | ✅ |
| `UUID` | ❌ | ❌ |
| `UUID_SHORT` | ❌ | ❌ |
| `UUID_TO_BIN` | ❌ | ❌ |
| `VALIDATE_PASSWORD_STRENGTH` | ❌ | ❌ |
| `VALUES` | ❌ | ❌ |
| `VARIANCE` | ✅ | ❌ |
| `VAR_POP` | ✅ | ✅ |
| `VAR_SAMP` | ✅ | ✅ |
| `VERSION` | ❌ | ✅ |
| `WEEK` | ✅ | ✅ |
| `WEEKDAY` | ✅ | ❌ |
| `WEEKOFYEAR` | ✅ | ❌ |
| `WEIGHT_STRING` | ❌ | ❌ |
| `XOR` | ❌ | ✅ |
| `YEAR` | ✅ | ✅ |
| `YEARWEEK` | ✅ | ✅ |

### Keywords

| Keyword | MySQL reserved | Pintail | ClickHouse |
|---|---|---|---|
| `ACCESSIBLE` | ✅ | ❌ | ❌ |
| `ACCOUNT` |  | ❌ | ❌ |
| `ACTION` |  | ❌ | ❌ |
| `ACTIVE` |  | ❌ | ❌ |
| `ADD` | ✅ | ❌ | ✅ |
| `ADMIN` |  | ❌ | ❌ |
| `AFTER` |  | ❌ | ✅ |
| `AGAINST` |  | ❌ | ❌ |
| `AGGREGATE` |  | ❌ | ❌ |
| `ALGORITHM` |  | ❌ | ✅ |
| `ALL` | ✅ | ✅ | ✅ |
| `ALTER` | ✅ | ➖ | ✅ |
| `ALWAYS` |  | ❌ | ❌ |
| `ANALYZE` | ✅ | ➖ | ❌ |
| `AND` | ✅ | ✅ | ✅ |
| `ANY` |  | ❌ | ✅ |
| `ARRAY` |  | ❌ | ❌ |
| `AS` | ✅ | ✅ | ✅ |
| `ASC` | ✅ | ✅ | ✅ |
| `ASCII` |  | ❌ | ❌ |
| `ASENSITIVE` | ✅ | ❌ | ❌ |
| `ASSIGN_GTIDS_TO_ANONYMOUS_TRANSACTIONS` |  | ❌ | ❌ |
| `AT` |  | ❌ | ❌ |
| `ATTRIBUTE` |  | ➖ | ❌ |
| `AUTHENTICATION` |  | ➖ | ❌ |
| `AUTO` |  | ❌ | ❌ |
| `AUTO_INCREMENT` |  | ❌ | ✅ |
| `AUTOEXTEND_SIZE` |  | ❌ | ❌ |
| `AVG` |  | ✅ | ❌ |
| `AVG_ROW_LENGTH` |  | ❌ | ❌ |
| `BACKUP` |  | ➖ | ✅ |
| `BEFORE` | ✅ | ❌ | ❌ |
| `BEGIN` |  | ❌ | ❌ |
| `BERNOULLI` |  | ❌ | ❌ |
| `BETWEEN` | ✅ | ✅ | ✅ |
| `BIGINT` | ✅ | ❌ | ❌ |
| `BINARY` | ✅ | ✅ | ❌ |
| `BINLOG` |  | ➖ | ❌ |
| `BIT` |  | ❌ | ❌ |
| `BLOB` | ✅ | ❌ | ❌ |
| `BLOCK` |  | ❌ | ❌ |
| `BOOL` |  | ❌ | ❌ |
| `BOOLEAN` |  | ❌ | ❌ |
| `BOTH` | ✅ | ❌ | ✅ |
| `BTREE` |  | ❌ | ❌ |
| `BUCKETS` |  | ❌ | ❌ |
| `BULK` |  | ❌ | ❌ |
| `BY` | ✅ | ✅ | ✅ |
| `BYTE` |  | ❌ | ❌ |
| `CACHE` |  | ➖ | ❌ |
| `CALL` | ✅ | ❌ | ❌ |
| `CASCADE` | ✅ | ❌ | ✅ |
| `CASCADED` |  | ❌ | ❌ |
| `CASE` | ✅ | ✅ | ✅ |
| `CATALOG_NAME` |  | ❌ | ❌ |
| `CHAIN` |  | ❌ | ❌ |
| `CHALLENGE_RESPONSE` |  | ❌ | ❌ |
| `CHANGE` | ✅ | ❌ | ✅ |
| `CHANGED` |  | ❌ | ✅ |
| `CHANNEL` |  | ➖ | ❌ |
| `CHAR` | ✅ | ✅ | ✅ |
| `CHARACTER` | ✅ | ✅ | ✅ |
| `CHARSET` |  | ❌ | ❌ |
| `CHECK` | ✅ | ❌ | ✅ |
| `CHECKSUM` |  | ➖ | ❌ |
| `CIPHER` |  | ❌ | ❌ |
| `CLASS_ORIGIN` |  | ❌ | ❌ |
| `CLIENT` |  | ❌ | ❌ |
| `CLONE` |  | ➖ | ❌ |
| `CLOSE` |  | ❌ | ❌ |
| `COALESCE` |  | ✅ | ❌ |
| `CODE` |  | ❌ | ❌ |
| `COLLATE` | ✅ | ✅ | ✅ |
| `COLLATION` |  | ❌ | ❌ |
| `COLUMN` | ✅ | ❌ | ✅ |
| `COLUMN_FORMAT` |  | ❌ | ❌ |
| `COLUMN_NAME` |  | ❌ | ❌ |
| `COLUMNS` |  | ❌ | ✅ |
| `COMMENT` |  | ❌ | ✅ |
| `COMMIT` |  | ➖ | ✅ |
| `COMMITTED` |  | ❌ | ❌ |
| `COMPACT` |  | ❌ | ❌ |
| `COMPLETION` |  | ❌ | ❌ |
| `COMPONENT` |  | ➖ | ❌ |
| `COMPRESSED` |  | ❌ | ❌ |
| `COMPRESSION` |  | ❌ | ✅ |
| `CONCURRENT` |  | ❌ | ❌ |
| `CONDITION` | ✅ | ❌ | ❌ |
| `CONNECTION` |  | ❌ | ❌ |
| `CONSISTENT` |  | ❌ | ❌ |
| `CONSTRAINT` | ✅ | ❌ | ✅ |
| `CONSTRAINT_CATALOG` |  | ❌ | ❌ |
| `CONSTRAINT_NAME` |  | ❌ | ❌ |
| `CONSTRAINT_SCHEMA` |  | ❌ | ❌ |
| `CONTAINS` |  | ❌ | ❌ |
| `CONTEXT` |  | ❌ | ❌ |
| `CONTINUE` | ✅ | ❌ | ❌ |
| `CONVERT` | ✅ | ✅ | ❌ |
| `CPU` |  | ❌ | ❌ |
| `CREATE` | ✅ | ➖ | ✅ |
| `CROSS` | ✅ | ✅ | ✅ |
| `CUBE` | ✅ | ❌ | ✅ |
| `CUME_DIST` | ✅ | ❌ | ❌ |
| `CURRENT` |  | ✅ | ❌ |
| `CURRENT_DATE` | ✅ | ❌ | ❌ |
| `CURRENT_TIME` | ✅ | ❌ | ❌ |
| `CURRENT_TIMESTAMP` | ✅ | ❌ | ❌ |
| `CURRENT_USER` | ✅ | ❌ | ✅ |
| `CURSOR` | ✅ | ➖ | ❌ |
| `CURSOR_NAME` |  | ❌ | ❌ |
| `DATA` |  | ❌ | ✅ |
| `DATABASE` | ✅ | ❌ | ✅ |
| `DATABASES` | ✅ | ❌ | ✅ |
| `DATAFILE` |  | ➖ | ❌ |
| `DATE` |  | ✅ | ✅ |
| `DATETIME` |  | ✅ | ❌ |
| `DAY` |  | ✅ | ✅ |
| `DAY_HOUR` | ✅ | ❌ | ❌ |
| `DAY_MICROSECOND` | ✅ | ❌ | ❌ |
| `DAY_MINUTE` | ✅ | ❌ | ❌ |
| `DAY_SECOND` | ✅ | ❌ | ❌ |
| `DEALLOCATE` |  | ❌ | ✅ |
| `DEC` | ✅ | ❌ | ❌ |
| `DECIMAL` | ✅ | ✅ | ❌ |
| `DECLARE` | ✅ | ❌ | ❌ |
| `DEFAULT` | ✅ | ✅ | ✅ |
| `DEFAULT_AUTH` |  | ❌ | ❌ |
| `DEFINER` |  | ❌ | ✅ |
| `DEFINITION` |  | ❌ | ❌ |
| `DELAY_KEY_WRITE` |  | ❌ | ❌ |
| `DELAYED` | ✅ | ❌ | ❌ |
| `DELETE` | ✅ | ➖ | ✅ |
| `DENSE_RANK` | ✅ | ❌ | ❌ |
| `DESC` | ✅ | ✅ | ✅ |
| `DESCRIBE` | ✅ | ❌ | ✅ |
| `DESCRIPTION` |  | ❌ | ❌ |
| `DETERMINISTIC` | ✅ | ❌ | ❌ |
| `DIAGNOSTICS` |  | ❌ | ❌ |
| `DIRECTORY` |  | ❌ | ❌ |
| `DISABLE` |  | ❌ | ❌ |
| `DISCARD` |  | ❌ | ❌ |
| `DISK` |  | ❌ | ✅ |
| `DISTINCT` | ✅ | ✅ | ✅ |
| `DISTINCTROW` | ✅ | ✅ | ❌ |
| `DIV` | ✅ | ❌ | ✅ |
| `DO` |  | ❌ | ❌ |
| `DOUBLE` | ✅ | ❌ | ❌ |
| `DROP` | ✅ | ➖ | ✅ |
| `DUAL` | ✅ | ✅ | ❌ |
| `DUMPFILE` |  | ❌ | ❌ |
| `DUPLICATE` |  | ❌ | ❌ |
| `DYNAMIC` |  | ❌ | ❌ |
| `EACH` | ✅ | ❌ | ❌ |
| `ELSE` | ✅ | ✅ | ✅ |
| `ELSEIF` | ✅ | ✅ | ❌ |
| `EMPTY` | ✅ | ❌ | ✅ |
| `ENABLE` |  | ❌ | ❌ |
| `ENCLOSED` | ✅ | ❌ | ❌ |
| `ENCRYPTION` |  | ❌ | ❌ |
| `END` |  | ✅ | ✅ |
| `ENDS` |  | ❌ | ❌ |
| `ENFORCED` |  | ❌ | ✅ |
| `ENGINE` |  | ➖ | ✅ |
| `ENGINE_ATTRIBUTE` |  | ➖ | ❌ |
| `ENGINES` |  | ➖ | ❌ |
| `ENUM` |  | ❌ | ❌ |
| `ERROR` |  | ❌ | ❌ |
| `ERRORS` |  | ❌ | ❌ |
| `ESCAPE` |  | ✅ | ❌ |
| `ESCAPED` | ✅ | ❌ | ❌ |
| `EVENT` |  | ➖ | ✅ |
| `EVENTS` |  | ➖ | ✅ |
| `EVERY` |  | ❌ | ✅ |
| `EXCEPT` | ✅ | ✅ | ✅ |
| `EXCHANGE` |  | ❌ | ❌ |
| `EXCLUDE` |  | ❌ | ❌ |
| `EXECUTE` |  | ❌ | ✅ |
| `EXISTS` | ✅ | ✅ | ✅ |
| `EXIT` | ✅ | ❌ | ❌ |
| `EXPANSION` |  | ❌ | ❌ |
| `EXPIRE` |  | ❌ | ❌ |
| `EXPLAIN` | ✅ | ❌ | ✅ |
| `EXPORT` |  | ❌ | ❌ |
| `EXTENDED` |  | ❌ | ✅ |
| `EXTENT_SIZE` |  | ❌ | ❌ |
| `FACTOR` |  | ❌ | ❌ |
| `FAILED_LOGIN_ATTEMPTS` |  | ❌ | ❌ |
| `FALSE` | ✅ | ✅ | ✅ |
| `FAST` |  | ❌ | ❌ |
| `FAULTS` |  | ❌ | ❌ |
| `FETCH` | ✅ | ❌ | ✅ |
| `FIELDS` |  | ❌ | ✅ |
| `FILE` |  | ❌ | ✅ |
| `FILE_BLOCK_SIZE` |  | ❌ | ❌ |
| `FILTER` |  | ❌ | ✅ |
| `FINISH` |  | ❌ | ❌ |
| `FIRST` |  | ❌ | ✅ |
| `FIRST_VALUE` | ✅ | ❌ | ❌ |
| `FIXED` |  | ❌ | ❌ |
| `FLOAT` | ✅ | ❌ | ❌ |
| `FLOAT4` | ✅ | ❌ | ❌ |
| `FLOAT8` | ✅ | ❌ | ❌ |
| `FLUSH` |  | ➖ | ❌ |
| `FOLLOWING` |  | ✅ | ✅ |
| `FOLLOWS` |  | ❌ | ❌ |
| `FOR` | ✅ | ❌ | ✅ |
| `FORCE` | ✅ | ❌ | ✅ |
| `FOREIGN` | ✅ | ❌ | ✅ |
| `FORMAT` |  | ❌ | ✅ |
| `FOUND` |  | ❌ | ❌ |
| `FROM` | ✅ | ✅ | ✅ |
| `FULL` |  | ❌ | ✅ |
| `FULLTEXT` | ✅ | ❌ | ✅ |
| `FUNCTION` | ✅ | ➖ | ✅ |
| `GENERAL` |  | ❌ | ❌ |
| `GENERATE` |  | ❌ | ❌ |
| `GENERATED` | ✅ | ❌ | ❌ |
| `GEOMCOLLECTION` |  | ➖ | ❌ |
| `GEOMETRY` |  | ➖ | ❌ |
| `GEOMETRYCOLLECTION` |  | ➖ | ❌ |
| `GET` | ✅ | ❌ | ❌ |
| `GET_FORMAT` |  | ❌ | ❌ |
| `GET_SOURCE_PUBLIC_KEY` |  | ❌ | ❌ |
| `GLOBAL` |  | ❌ | ✅ |
| `GRANT` | ✅ | ➖ | ✅ |
| `GRANTS` |  | ❌ | ❌ |
| `GROUP` | ✅ | ✅ | ❌ |
| `GROUP_REPLICATION` |  | ❌ | ❌ |
| `GROUPING` | ✅ | ❌ | ❌ |
| `GROUPS` | ✅ | ✅ | ✅ |
| `GTID_ONLY` |  | ➖ | ❌ |
| `GTIDS` |  | ➖ | ❌ |
| `HANDLER` |  | ➖ | ❌ |
| `HASH` |  | ❌ | ✅ |
| `HAVING` | ✅ | ✅ | ✅ |
| `HELP` |  | ❌ | ❌ |
| `HIGH_PRIORITY` | ✅ | ❌ | ❌ |
| `HISTOGRAM` |  | ❌ | ❌ |
| `HISTORY` |  | ❌ | ❌ |
| `HOST` |  | ❌ | ✅ |
| `HOSTS` |  | ❌ | ❌ |
| `HOUR` |  | ✅ | ✅ |
| `HOUR_MICROSECOND` | ✅ | ❌ | ❌ |
| `HOUR_MINUTE` | ✅ | ❌ | ❌ |
| `HOUR_SECOND` | ✅ | ❌ | ❌ |
| `IDENTIFIED` |  | ❌ | ✅ |
| `IF` | ✅ | ✅ | ❌ |
| `IGNORE` | ✅ | ❌ | ❌ |
| `IGNORE_SERVER_IDS` |  | ❌ | ❌ |
| `IMPORT` |  | ❌ | ❌ |
| `IN` | ✅ | ✅ | ✅ |
| `INACTIVE` |  | ❌ | ❌ |
| `INDEX` | ✅ | ❌ | ✅ |
| `INDEXES` |  | ❌ | ✅ |
| `INFILE` | ✅ | ❌ | ❌ |
| `INITIAL` |  | ❌ | ❌ |
| `INITIAL_SIZE` |  | ❌ | ❌ |
| `INITIATE` |  | ❌ | ❌ |
| `INNER` | ✅ | ✅ | ✅ |
| `INOUT` | ✅ | ❌ | ❌ |
| `INSENSITIVE` | ✅ | ❌ | ❌ |
| `INSERT` | ✅ | ➖ | ❌ |
| `INSERT_METHOD` |  | ❌ | ❌ |
| `INSTALL` |  | ➖ | ❌ |
| `INSTANCE` |  | ❌ | ❌ |
| `INT` | ✅ | ❌ | ❌ |
| `INT1` | ✅ | ❌ | ❌ |
| `INT2` | ✅ | ❌ | ❌ |
| `INT3` | ✅ | ❌ | ❌ |
| `INT4` | ✅ | ❌ | ❌ |
| `INT8` | ✅ | ❌ | ❌ |
| `INTEGER` | ✅ | ❌ | ❌ |
| `INTERSECT` | ✅ | ✅ | ✅ |
| `INTERVAL` | ✅ | ✅ | ✅ |
| `INTO` | ✅ | ❌ | ❌ |
| `INVISIBLE` |  | ❌ | ✅ |
| `INVOKER` |  | ❌ | ✅ |
| `IO` |  | ❌ | ❌ |
| `IO_AFTER_GTIDS` | ✅ | ❌ | ❌ |
| `IO_BEFORE_GTIDS` | ✅ | ❌ | ❌ |
| `IO_THREAD` |  | ❌ | ❌ |
| `IPC` |  | ❌ | ❌ |
| `IS` | ✅ | ✅ | ❌ |
| `ISOLATION` |  | ❌ | ❌ |
| `ISSUER` |  | ❌ | ❌ |
| `ITERATE` | ✅ | ❌ | ❌ |
| `JOIN` | ✅ | ✅ | ✅ |
| `JSON` |  | ✅ | ❌ |
| `JSON_TABLE` | ✅ | ❌ | ❌ |
| `JSON_VALUE` |  | ❌ | ❌ |
| `KEY` | ✅ | ❌ | ✅ |
| `KEY_BLOCK_SIZE` |  | ❌ | ❌ |
| `KEYRING` |  | ❌ | ❌ |
| `KEYS` | ✅ | ❌ | ✅ |
| `KILL` | ✅ | ❌ | ✅ |
| `LAG` | ✅ | ❌ | ❌ |
| `LANGUAGE` |  | ❌ | ❌ |
| `LAST` |  | ❌ | ✅ |
| `LAST_VALUE` | ✅ | ❌ | ❌ |
| `LATERAL` | ✅ | ❌ | ❌ |
| `LEAD` | ✅ | ❌ | ❌ |
| `LEADING` | ✅ | ❌ | ✅ |
| `LEAVE` | ✅ | ❌ | ❌ |
| `LEAVES` |  | ❌ | ❌ |
| `LEFT` | ✅ | ✅ | ✅ |
| `LESS` |  | ❌ | ❌ |
| `LEVEL` |  | ❌ | ✅ |
| `LIKE` | ✅ | ✅ | ✅ |
| `LIMIT` | ✅ | ✅ | ✅ |
| `LINEAR` | ✅ | ❌ | ✅ |
| `LINES` | ✅ | ❌ | ❌ |
| `LINESTRING` |  | ➖ | ❌ |
| `LIST` |  | ❌ | ✅ |
| `LOAD` | ✅ | ❌ | ❌ |
| `LOCAL` |  | ❌ | ✅ |
| `LOCALTIME` | ✅ | ❌ | ❌ |
| `LOCALTIMESTAMP` | ✅ | ❌ | ❌ |
| `LOCK` | ✅ | ➖ | ❌ |
| `LOCKED` |  | ❌ | ❌ |
| `LOCKS` |  | ❌ | ❌ |
| `LOG` |  | ❌ | ❌ |
| `LOGFILE` |  | ❌ | ❌ |
| `LOGS` |  | ❌ | ❌ |
| `LONG` | ✅ | ❌ | ❌ |
| `LONGBLOB` | ✅ | ❌ | ❌ |
| `LONGTEXT` | ✅ | ❌ | ❌ |
| `LOOP` | ✅ | ❌ | ❌ |
| `LOW_PRIORITY` | ✅ | ❌ | ❌ |
| `MANUAL` |  | ❌ | ❌ |
| `MASTER` |  | ➖ | ❌ |
| `MATCH` | ✅ | ❌ | ✅ |
| `MAX_CONNECTIONS_PER_HOUR` |  | ❌ | ❌ |
| `MAX_QUERIES_PER_HOUR` |  | ❌ | ❌ |
| `MAX_ROWS` |  | ❌ | ❌ |
| `MAX_SIZE` |  | ❌ | ❌ |
| `MAX_UPDATES_PER_HOUR` |  | ❌ | ❌ |
| `MAX_USER_CONNECTIONS` |  | ❌ | ❌ |
| `MAXVALUE` | ✅ | ❌ | ❌ |
| `MEDIUM` |  | ❌ | ❌ |
| `MEDIUMBLOB` | ✅ | ❌ | ❌ |
| `MEDIUMINT` | ✅ | ❌ | ❌ |
| `MEDIUMTEXT` | ✅ | ❌ | ❌ |
| `MEMBER` |  | ❌ | ❌ |
| `MEMORY` |  | ❌ | ✅ |
| `MERGE` |  | ➖ | ❌ |
| `MESSAGE_TEXT` |  | ❌ | ❌ |
| `MICROSECOND` |  | ✅ | ✅ |
| `MIDDLEINT` | ✅ | ❌ | ❌ |
| `MIGRATE` |  | ❌ | ❌ |
| `MIN_ROWS` |  | ❌ | ❌ |
| `MINUTE` |  | ✅ | ✅ |
| `MINUTE_MICROSECOND` | ✅ | ❌ | ❌ |
| `MINUTE_SECOND` | ✅ | ❌ | ❌ |
| `MOD` | ✅ | ❌ | ✅ |
| `MODE` |  | ❌ | ❌ |
| `MODIFIES` | ✅ | ❌ | ❌ |
| `MODIFY` |  | ❌ | ✅ |
| `MONTH` |  | ✅ | ✅ |
| `MULTILINESTRING` |  | ➖ | ❌ |
| `MULTIPOINT` |  | ➖ | ❌ |
| `MULTIPOLYGON` |  | ➖ | ❌ |
| `MUTEX` |  | ❌ | ❌ |
| `MYSQL_ERRNO` |  | ❌ | ❌ |
| `NAME` |  | ❌ | ✅ |
| `NAMES` |  | ❌ | ❌ |
| `NATIONAL` |  | ❌ | ❌ |
| `NATURAL` | ✅ | ✅ | ❌ |
| `NCHAR` |  | ✅ | ❌ |
| `NDB` |  | ❌ | ❌ |
| `NDBCLUSTER` |  | ❌ | ❌ |
| `NESTED` |  | ❌ | ❌ |
| `NETWORK_NAMESPACE` |  | ❌ | ❌ |
| `NEVER` |  | ❌ | ❌ |
| `NEW` |  | ❌ | ❌ |
| `NEXT` |  | ❌ | ✅ |
| `NO` |  | ❌ | ❌ |
| `NO_WAIT` |  | ❌ | ❌ |
| `NO_WRITE_TO_BINLOG` | ✅ | ❌ | ❌ |
| `NODEGROUP` |  | ❌ | ❌ |
| `NONE` |  | ❌ | ✅ |
| `NOT` | ✅ | ✅ | ✅ |
| `NOWAIT` |  | ❌ | ❌ |
| `NTH_VALUE` | ✅ | ❌ | ❌ |
| `NTILE` | ✅ | ❌ | ❌ |
| `NULL` | ✅ | ✅ | ✅ |
| `NULLS` |  | ❌ | ✅ |
| `NUMBER` |  | ❌ | ❌ |
| `NUMERIC` | ✅ | ❌ | ❌ |
| `NVARCHAR` |  | ❌ | ❌ |
| `OF` | ✅ | ❌ | ❌ |
| `OFF` |  | ❌ | ❌ |
| `OFFSET` |  | ✅ | ✅ |
| `OJ` |  | ❌ | ❌ |
| `OLD` |  | ❌ | ❌ |
| `ON` | ✅ | ✅ | ✅ |
| `ONE` |  | ❌ | ❌ |
| `ONLY` |  | ❌ | ✅ |
| `OPEN` |  | ❌ | ❌ |
| `OPTIMIZE` | ✅ | ➖ | ❌ |
| `OPTIMIZER_COSTS` | ✅ | ❌ | ❌ |
| `OPTION` | ✅ | ❌ | ❌ |
| `OPTIONAL` |  | ❌ | ❌ |
| `OPTIONALLY` | ✅ | ❌ | ❌ |
| `OPTIONS` |  | ❌ | ❌ |
| `OR` | ✅ | ✅ | ✅ |
| `ORDER` | ✅ | ✅ | ❌ |
| `ORDINALITY` |  | ❌ | ❌ |
| `ORGANIZATION` |  | ❌ | ❌ |
| `OTHERS` |  | ❌ | ❌ |
| `OUT` | ✅ | ❌ | ❌ |
| `OUTER` | ✅ | ✅ | ✅ |
| `OUTFILE` | ✅ | ❌ | ❌ |
| `OVER` | ✅ | ✅ | ✅ |
| `OWNER` |  | ❌ | ❌ |
| `PACK_KEYS` |  | ❌ | ❌ |
| `PAGE` |  | ❌ | ❌ |
| `PARALLEL` |  | ❌ | ❌ |
| `PARSE_TREE` |  | ❌ | ❌ |
| `PARSER` |  | ❌ | ❌ |
| `PARTIAL` |  | ❌ | ✅ |
| `PARTITION` | ✅ | ✅ | ✅ |
| `PARTITIONING` |  | ➖ | ❌ |
| `PARTITIONS` |  | ➖ | ✅ |
| `PASSWORD` |  | ➖ | ❌ |
| `PASSWORD_LOCK_TIME` |  | ❌ | ❌ |
| `PATH` |  | ❌ | ❌ |
| `PERCENT_RANK` | ✅ | ❌ | ❌ |
| `PERSIST` |  | ❌ | ❌ |
| `PERSIST_ONLY` |  | ❌ | ❌ |
| `PHASE` |  | ❌ | ❌ |
| `PLUGIN` |  | ➖ | ❌ |
| `PLUGIN_DIR` |  | ❌ | ❌ |
| `PLUGINS` |  | ➖ | ❌ |
| `POINT` |  | ➖ | ❌ |
| `POLYGON` |  | ➖ | ❌ |
| `PORT` |  | ❌ | ❌ |
| `PRECEDES` |  | ❌ | ❌ |
| `PRECEDING` |  | ✅ | ✅ |
| `PRECISION` | ✅ | ❌ | ✅ |
| `PREPARE` |  | ❌ | ✅ |
| `PRESERVE` |  | ❌ | ❌ |
| `PREV` |  | ❌ | ❌ |
| `PRIMARY` | ✅ | ❌ | ✅ |
| `PRIVILEGE_CHECKS_USER` |  | ❌ | ❌ |
| `PRIVILEGES` |  | ➖ | ❌ |
| `PROCEDURE` | ✅ | ➖ | ❌ |
| `PROCESS` |  | ❌ | ❌ |
| `PROCESSLIST` |  | ❌ | ❌ |
| `PROFILE` |  | ❌ | ✅ |
| `PROFILES` |  | ❌ | ✅ |
| `PROXY` |  | ➖ | ❌ |
| `PURGE` | ✅ | ➖ | ❌ |
| `QUALIFY` | ✅ | ❌ | ✅ |
| `QUARTER` |  | ✅ | ✅ |
| `QUERY` |  | ❌ | ✅ |
| `QUICK` |  | ❌ | ❌ |
| `RANDOM` |  | ❌ | ❌ |
| `RANGE` | ✅ | ✅ | ✅ |
| `RANK` | ✅ | ❌ | ❌ |
| `READ` | ✅ | ❌ | ✅ |
| `READ_ONLY` |  | ❌ | ❌ |
| `READ_WRITE` | ✅ | ❌ | ❌ |
| `READS` | ✅ | ❌ | ❌ |
| `REAL` | ✅ | ❌ | ❌ |
| `REBUILD` |  | ❌ | ❌ |
| `RECOVER` |  | ❌ | ❌ |
| `RECURSIVE` | ✅ | ✅ | ✅ |
| `REDO_BUFFER_SIZE` |  | ❌ | ❌ |
| `REDUNDANT` |  | ❌ | ❌ |
| `REFERENCE` |  | ❌ | ❌ |
| `REFERENCES` | ✅ | ❌ | ✅ |
| `REGEXP` | ✅ | ✅ | ✅ |
| `REGISTRATION` |  | ❌ | ❌ |
| `RELAY` |  | ➖ | ❌ |
| `RELAY_LOG_FILE` |  | ➖ | ❌ |
| `RELAY_LOG_POS` |  | ➖ | ❌ |
| `RELAY_THREAD` |  | ➖ | ❌ |
| `RELAYLOG` |  | ➖ | ❌ |
| `RELEASE` | ✅ | ❌ | ❌ |
| `RELOAD` |  | ❌ | ❌ |
| `REMOVE` |  | ❌ | ✅ |
| `RENAME` | ✅ | ➖ | ✅ |
| `REORGANIZE` |  | ❌ | ❌ |
| `REPAIR` |  | ➖ | ❌ |
| `REPEAT` | ✅ | ❌ | ❌ |
| `REPEATABLE` |  | ❌ | ❌ |
| `REPLACE` | ✅ | ➖ | ✅ |
| `REPLICA` |  | ➖ | ❌ |
| `REPLICAS` |  | ➖ | ❌ |
| `REPLICATE_DO_DB` |  | ➖ | ❌ |
| `REPLICATE_DO_TABLE` |  | ➖ | ❌ |
| `REPLICATE_IGNORE_DB` |  | ➖ | ❌ |
| `REPLICATE_IGNORE_TABLE` |  | ➖ | ❌ |
| `REPLICATE_REWRITE_DB` |  | ➖ | ❌ |
| `REPLICATE_WILD_DO_TABLE` |  | ➖ | ❌ |
| `REPLICATE_WILD_IGNORE_TABLE` |  | ➖ | ❌ |
| `REPLICATION` |  | ➖ | ❌ |
| `REQUIRE` | ✅ | ❌ | ❌ |
| `REQUIRE_ROW_FORMAT` |  | ❌ | ❌ |
| `REQUIRE_TABLE_PRIMARY_KEY_CHECK` |  | ❌ | ❌ |
| `RESET` |  | ➖ | ❌ |
| `RESIGNAL` | ✅ | ❌ | ❌ |
| `RESOURCE` |  | ❌ | ✅ |
| `RESPECT` |  | ❌ | ❌ |
| `RESTART` |  | ❌ | ❌ |
| `RESTORE` |  | ➖ | ✅ |
| `RESTRICT` | ✅ | ❌ | ✅ |
| `RESUME` |  | ❌ | ✅ |
| `RETAIN` |  | ❌ | ❌ |
| `RETURN` | ✅ | ❌ | ❌ |
| `RETURNED_SQLSTATE` |  | ❌ | ❌ |
| `RETURNING` |  | ❌ | ❌ |
| `RETURNS` |  | ❌ | ❌ |
| `REUSE` |  | ❌ | ❌ |
| `REVERSE` |  | ❌ | ❌ |
| `REVOKE` | ✅ | ➖ | ✅ |
| `RIGHT` | ✅ | ✅ | ✅ |
| `RLIKE` | ✅ | ✅ | ❌ |
| `ROLE` |  | ➖ | ❌ |
| `ROLLBACK` |  | ➖ | ✅ |
| `ROLLUP` |  | ❌ | ✅ |
| `ROTATE` |  | ❌ | ❌ |
| `ROUTINE` |  | ➖ | ❌ |
| `ROW` | ✅ | ✅ | ✅ |
| `ROW_COUNT` |  | ❌ | ❌ |
| `ROW_FORMAT` |  | ❌ | ❌ |
| `ROW_NUMBER` | ✅ | ❌ | ❌ |
| `ROWS` | ✅ | ✅ | ✅ |
| `RTREE` |  | ❌ | ❌ |
| `S3` |  | ❌ | ✅ |
| `SAVEPOINT` |  | ➖ | ❌ |
| `SCHEDULE` |  | ❌ | ❌ |
| `SCHEMA` | ✅ | ❌ | ❌ |
| `SCHEMA_NAME` |  | ❌ | ❌ |
| `SCHEMAS` | ✅ | ❌ | ❌ |
| `SECOND` |  | ✅ | ✅ |
| `SECOND_MICROSECOND` | ✅ | ❌ | ❌ |
| `SECONDARY` |  | ❌ | ❌ |
| `SECONDARY_ENGINE` |  | ❌ | ❌ |
| `SECONDARY_ENGINE_ATTRIBUTE` |  | ❌ | ❌ |
| `SECONDARY_LOAD` |  | ❌ | ❌ |
| `SECONDARY_UNLOAD` |  | ❌ | ❌ |
| `SECURITY` |  | ❌ | ❌ |
| `SELECT` | ✅ | ✅ | ✅ |
| `SENSITIVE` | ✅ | ❌ | ❌ |
| `SEPARATOR` | ✅ | ✅ | ❌ |
| `SERIAL` |  | ❌ | ❌ |
| `SERIALIZABLE` |  | ❌ | ❌ |
| `SERVER` |  | ❌ | ✅ |
| `SESSION` |  | ❌ | ❌ |
| `SET` | ✅ | ✅ | ✅ |
| `SHARE` |  | ❌ | ❌ |
| `SHOW` | ✅ | ❌ | ✅ |
| `SHUTDOWN` |  | ➖ | ❌ |
| `SIGNAL` | ✅ | ❌ | ❌ |
| `SIGNED` |  | ✅ | ✅ |
| `SIMPLE` |  | ❌ | ✅ |
| `SKIP` |  | ❌ | ✅ |
| `SLAVE` |  | ➖ | ❌ |
| `SLOW` |  | ❌ | ❌ |
| `SMALLINT` | ✅ | ❌ | ❌ |
| `SNAPSHOT` |  | ❌ | ❌ |
| `SOCKET` |  | ❌ | ❌ |
| `SOME` |  | ❌ | ❌ |
| `SONAME` |  | ➖ | ❌ |
| `SOUNDS` |  | ❌ | ❌ |
| `SOURCE` |  | ➖ | ✅ |
| `SOURCE_AUTO_POSITION` |  | ➖ | ❌ |
| `SOURCE_BIND` |  | ➖ | ❌ |
| `SOURCE_COMPRESSION_ALGORITHMS` |  | ➖ | ❌ |
| `SOURCE_CONNECT_RETRY` |  | ➖ | ❌ |
| `SOURCE_CONNECTION_AUTO_FAILOVER` |  | ➖ | ❌ |
| `SOURCE_DELAY` |  | ➖ | ❌ |
| `SOURCE_HEARTBEAT_PERIOD` |  | ➖ | ❌ |
| `SOURCE_HOST` |  | ➖ | ❌ |
| `SOURCE_LOG_FILE` |  | ➖ | ❌ |
| `SOURCE_LOG_POS` |  | ➖ | ❌ |
| `SOURCE_PASSWORD` |  | ➖ | ❌ |
| `SOURCE_PORT` |  | ➖ | ❌ |
| `SOURCE_PUBLIC_KEY_PATH` |  | ➖ | ❌ |
| `SOURCE_RETRY_COUNT` |  | ➖ | ❌ |
| `SOURCE_SSL` |  | ➖ | ❌ |
| `SOURCE_SSL_CA` |  | ➖ | ❌ |
| `SOURCE_SSL_CAPATH` |  | ➖ | ❌ |
| `SOURCE_SSL_CERT` |  | ➖ | ❌ |
| `SOURCE_SSL_CIPHER` |  | ➖ | ❌ |
| `SOURCE_SSL_CRL` |  | ➖ | ❌ |
| `SOURCE_SSL_CRLPATH` |  | ➖ | ❌ |
| `SOURCE_SSL_KEY` |  | ➖ | ❌ |
| `SOURCE_SSL_VERIFY_SERVER_CERT` |  | ➖ | ❌ |
| `SOURCE_TLS_CIPHERSUITES` |  | ➖ | ❌ |
| `SOURCE_TLS_VERSION` |  | ➖ | ❌ |
| `SOURCE_USER` |  | ➖ | ❌ |
| `SOURCE_ZSTD_COMPRESSION_LEVEL` |  | ➖ | ❌ |
| `SPATIAL` | ✅ | ➖ | ✅ |
| `SPECIFIC` | ✅ | ❌ | ❌ |
| `SQL` | ✅ | ❌ | ❌ |
| `SQL_AFTER_GTIDS` |  | ❌ | ❌ |
| `SQL_AFTER_MTS_GAPS` |  | ❌ | ❌ |
| `SQL_BEFORE_GTIDS` |  | ❌ | ❌ |
| `SQL_BIG_RESULT` | ✅ | ❌ | ❌ |
| `SQL_BUFFER_RESULT` |  | ❌ | ❌ |
| `SQL_CALC_FOUND_ROWS` | ✅ | ❌ | ❌ |
| `SQL_NO_CACHE` |  | ❌ | ❌ |
| `SQL_SMALL_RESULT` | ✅ | ❌ | ❌ |
| `SQL_THREAD` |  | ❌ | ❌ |
| `SQL_TSI_DAY` |  | ❌ | ✅ |
| `SQL_TSI_HOUR` |  | ❌ | ✅ |
| `SQL_TSI_MINUTE` |  | ❌ | ✅ |
| `SQL_TSI_MONTH` |  | ❌ | ✅ |
| `SQL_TSI_QUARTER` |  | ❌ | ✅ |
| `SQL_TSI_SECOND` |  | ❌ | ✅ |
| `SQL_TSI_WEEK` |  | ❌ | ✅ |
| `SQL_TSI_YEAR` |  | ❌ | ✅ |
| `SQLEXCEPTION` | ✅ | ❌ | ❌ |
| `SQLSTATE` | ✅ | ❌ | ❌ |
| `SQLWARNING` | ✅ | ❌ | ❌ |
| `SRID` |  | ➖ | ❌ |
| `SSL` | ✅ | ❌ | ❌ |
| `STACKED` |  | ❌ | ❌ |
| `START` |  | ❌ | ❌ |
| `STARTING` | ✅ | ❌ | ❌ |
| `STARTS` |  | ❌ | ❌ |
| `STATS_AUTO_RECALC` |  | ❌ | ❌ |
| `STATS_PERSISTENT` |  | ❌ | ❌ |
| `STATS_SAMPLE_PAGES` |  | ❌ | ❌ |
| `STATUS` |  | ❌ | ❌ |
| `STOP` |  | ❌ | ❌ |
| `STORAGE` |  | ❌ | ✅ |
| `STORED` | ✅ | ❌ | ❌ |
| `STRAIGHT_JOIN` | ✅ | ✅ | ❌ |
| `STREAM` |  | ❌ | ❌ |
| `STRING` |  | ❌ | ❌ |
| `SUBCLASS_ORIGIN` |  | ❌ | ❌ |
| `SUBJECT` |  | ❌ | ❌ |
| `SUBPARTITION` |  | ➖ | ✅ |
| `SUBPARTITIONS` |  | ➖ | ✅ |
| `SUPER` |  | ❌ | ❌ |
| `SUSPEND` |  | ❌ | ✅ |
| `SWAPS` |  | ❌ | ❌ |
| `SWITCHES` |  | ❌ | ❌ |
| `SYSTEM` | ✅ | ❌ | ✅ |
| `TABLE` | ✅ | ✅ | ✅ |
| `TABLE_CHECKSUM` |  | ❌ | ❌ |
| `TABLE_NAME` |  | ❌ | ❌ |
| `TABLES` |  | ❌ | ✅ |
| `TABLESAMPLE` | ✅ | ❌ | ❌ |
| `TABLESPACE` |  | ➖ | ❌ |
| `TEMPORARY` |  | ❌ | ✅ |
| `TEMPTABLE` |  | ❌ | ❌ |
| `TERMINATED` | ✅ | ❌ | ❌ |
| `TEXT` |  | ❌ | ❌ |
| `THAN` |  | ❌ | ❌ |
| `THEN` | ✅ | ✅ | ✅ |
| `THREAD_PRIORITY` |  | ❌ | ❌ |
| `TIES` |  | ❌ | ❌ |
| `TIME` |  | ✅ | ❌ |
| `TIMESTAMP` |  | ❌ | ✅ |
| `TIMESTAMPADD` |  | ❌ | ❌ |
| `TIMESTAMPDIFF` |  | ❌ | ❌ |
| `TINYBLOB` | ✅ | ❌ | ❌ |
| `TINYINT` | ✅ | ❌ | ❌ |
| `TINYTEXT` | ✅ | ❌ | ❌ |
| `TLS` |  | ❌ | ❌ |
| `TO` | ✅ | ❌ | ✅ |
| `TRAILING` | ✅ | ❌ | ✅ |
| `TRANSACTION` |  | ➖ | ✅ |
| `TRIGGER` | ✅ | ➖ | ✅ |
| `TRIGGERS` |  | ➖ | ❌ |
| `TRUE` | ✅ | ✅ | ✅ |
| `TRUNCATE` |  | ➖ | ✅ |
| `TYPE` |  | ❌ | ✅ |
| `TYPES` |  | ❌ | ❌ |
| `UNBOUNDED` |  | ✅ | ✅ |
| `UNCOMMITTED` |  | ❌ | ❌ |
| `UNDEFINED` |  | ❌ | ❌ |
| `UNDO` | ✅ | ❌ | ❌ |
| `UNDO_BUFFER_SIZE` |  | ❌ | ❌ |
| `UNDOFILE` |  | ➖ | ❌ |
| `UNICODE` |  | ❌ | ❌ |
| `UNINSTALL` |  | ➖ | ❌ |
| `UNION` | ✅ | ✅ | ✅ |
| `UNIQUE` | ✅ | ❌ | ✅ |
| `UNKNOWN` |  | ✅ | ❌ |
| `UNLOCK` | ✅ | ➖ | ❌ |
| `UNREGISTER` |  | ❌ | ❌ |
| `UNSIGNED` | ✅ | ✅ | ✅ |
| `UNTIL` |  | ❌ | ❌ |
| `UPDATE` | ✅ | ➖ | ✅ |
| `UPGRADE` |  | ❌ | ❌ |
| `URL` |  | ❌ | ✅ |
| `USAGE` | ✅ | ❌ | ❌ |
| `USE` | ✅ | ❌ | ✅ |
| `USE_FRM` |  | ❌ | ❌ |
| `USER` |  | ➖ | ❌ |
| `USER_RESOURCES` |  | ❌ | ❌ |
| `USING` | ✅ | ✅ | ✅ |
| `UTC_DATE` | ✅ | ❌ | ❌ |
| `UTC_TIME` | ✅ | ❌ | ❌ |
| `UTC_TIMESTAMP` | ✅ | ❌ | ❌ |
| `VALIDATION` |  | ❌ | ❌ |
| `VALUE` |  | ❌ | ❌ |
| `VALUES` | ✅ | ✅ | ✅ |
| `VARBINARY` | ✅ | ❌ | ❌ |
| `VARCHAR` | ✅ | ❌ | ❌ |
| `VARCHARACTER` | ✅ | ❌ | ❌ |
| `VARIABLES` |  | ❌ | ❌ |
| `VARYING` | ✅ | ❌ | ✅ |
| `VCPU` |  | ❌ | ❌ |
| `VIEW` |  | ❌ | ✅ |
| `VIRTUAL` | ✅ | ❌ | ❌ |
| `VISIBLE` |  | ❌ | ✅ |
| `WAIT` |  | ❌ | ❌ |
| `WARNINGS` |  | ❌ | ❌ |
| `WEEK` |  | ✅ | ✅ |
| `WEIGHT_STRING` |  | ❌ | ❌ |
| `WHEN` | ✅ | ✅ | ✅ |
| `WHERE` | ✅ | ✅ | ✅ |
| `WHILE` | ✅ | ❌ | ❌ |
| `WINDOW` | ✅ | ✅ | ✅ |
| `WITH` | ✅ | ✅ | ✅ |
| `WITHOUT` |  | ❌ | ❌ |
| `WORK` |  | ❌ | ❌ |
| `WRAPPER` |  | ❌ | ❌ |
| `WRITE` | ✅ | ❌ | ✅ |
| `X509` |  | ❌ | ❌ |
| `XA` |  | ➖ | ❌ |
| `XID` |  | ❌ | ❌ |
| `XML` |  | ❌ | ❌ |
| `XOR` | ✅ | ✅ | ❌ |
| `YEAR` |  | ✅ | ✅ |
| `YEAR_MONTH` | ✅ | ❌ | ❌ |
| `ZEROFILL` | ✅ | ❌ | ❌ |
| `ZONE` |  | ❌ | ❌ |
