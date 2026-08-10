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
| Callable functions | 134 — `bun run scripts/function-surface.ts` reads them from the binder, and a unit test holds this number to what it prints |
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

**The ClickHouse column measures the name, not the capability.** ClickHouse
implements much of this surface under different spellings: it answers `no`
to `JSON_EXTRACT` while shipping 28 `JSONExtract*` functions, and `no` to
`DATE_ADD` while shipping `date_diff` and the `toYear`/`toMonth` family. A
`no` here means "not callable by the MySQL name", which is what matters for
pointing an existing MySQL client at it - not "cannot do this".

`n/a` marks a keyword a read-only analytical replica cannot encounter by
design — DDL, DML writes, replication and administration. Those are out of
scope rather than missing, and counting them as gaps would make this table
read as far worse than the engine is.

**Functions:** 392 MySQL functions — Pintail 130, ClickHouse 151.

**Keywords:** 734 MySQL keywords — Pintail 95 supported and 123 out of scope, ClickHouse 208.

### Functions

| Function | Pintail | ClickHouse |
|---|---|---|
| `ABS` | yes | yes |
| `ACOS` | no | yes |
| `ADDDATE` | no | yes |
| `ADDTIME` | no | no |
| `AES_DECRYPT` | no | no |
| `AES_ENCRYPT` | no | no |
| `AND` | no | yes |
| `ANY_VALUE` | yes | yes |
| `ASCII` | yes | yes |
| `ASIN` | no | yes |
| `ASYMMETRIC_DECRYPT` | no | no |
| `ASYMMETRIC_ENCRYPT` | no | no |
| `ASYMMETRIC_SIGN` | no | no |
| `ASYMMETRIC_VERIFY` | no | no |
| `ATAN` | no | yes |
| `ATAN2` | no | yes |
| `AVG` | yes | yes |
| `BENCHMARK` | no | no |
| `BIN` | no | yes |
| `BIN_TO_UUID` | no | no |
| `BIT_AND` | yes | yes |
| `BIT_COUNT` | no | no |
| `BIT_LENGTH` | no | no |
| `BIT_OR` | yes | yes |
| `BIT_XOR` | yes | yes |
| `CAN_ACCESS_COLUMN` | no | no |
| `CAN_ACCESS_DATABASE` | no | no |
| `CAN_ACCESS_TABLE` | no | no |
| `CAN_ACCESS_USER` | no | no |
| `CAN_ACCESS_VIEW` | no | no |
| `CAST` | no | yes |
| `CEIL` | yes | yes |
| `CEILING` | yes | yes |
| `CHARACTER_LENGTH` | yes | yes |
| `CHARSET` | no | no |
| `CHAR_LENGTH` | yes | yes |
| `COALESCE` | yes | yes |
| `COERCIBILITY` | no | no |
| `COLLATION` | no | no |
| `COMPRESS` | no | no |
| `CONCAT` | yes | yes |
| `CONCAT_WS` | yes | yes |
| `CONNECTION_ID` | no | yes |
| `CONV` | yes | no |
| `CONVERT` | no | no |
| `CONVERT_TZ` | yes | no |
| `COS` | no | yes |
| `COT` | no | no |
| `COUNT` | yes | yes |
| `CRC32` | no | yes |
| `CREATE_ASYMMETRIC_PRIV_KEY` | no | no |
| `CREATE_ASYMMETRIC_PUB_KEY` | no | no |
| `CREATE_DIGEST` | no | no |
| `CUME_DIST` | no | no |
| `CURDATE` | yes | yes |
| `CURRENT_DATE` | no | yes |
| `CURRENT_ROLE` | no | no |
| `CURRENT_TIME` | yes | no |
| `CURRENT_TIMESTAMP` | no | yes |
| `CURRENT_USER` | no | yes |
| `CURTIME` | yes | no |
| `DATABASE` | no | yes |
| `DATEDIFF` | yes | yes |
| `DATE_ADD` | yes | no |
| `DATE_FORMAT` | yes | yes |
| `DATE_SUB` | yes | no |
| `DAY` | yes | yes |
| `DAYNAME` | yes | no |
| `DAYOFMONTH` | yes | yes |
| `DAYOFWEEK` | yes | yes |
| `DAYOFYEAR` | yes | yes |
| `DEFAULT` | no | no |
| `DEGREES` | no | yes |
| `DENSE_RANK` | yes | yes |
| `DIV` | no | no |
| `ELT` | yes | no |
| `EXISTS` | no | no |
| `EXP` | yes | yes |
| `EXPORT_SET` | no | no |
| `EXTRACT` | no | yes |
| `FIELD` | yes | no |
| `FIND_IN_SET` | yes | no |
| `FIRST_VALUE` | no | yes |
| `FLOOR` | yes | yes |
| `FORMAT` | yes | yes |
| `FORMAT_BYTES` | no | yes |
| `FORMAT_PICO_TIME` | no | no |
| `FOUND_ROWS` | no | no |
| `FROM_BASE64` | yes | yes |
| `FROM_DAYS` | yes | yes |
| `FROM_UNIXTIME` | yes | yes |
| `GET_DD_COLUMN_PRIVILEGES` | no | no |
| `GET_DD_CREATE_OPTIONS` | no | no |
| `GET_DD_INDEX_SUB_PART_LENGTH` | no | no |
| `GET_FORMAT` | no | no |
| `GET_LOCK` | no | no |
| `GREATEST` | yes | yes |
| `GROUPING` | no | no |
| `GROUP_CONCAT` | yes | yes |
| `HEX` | yes | yes |
| `HOUR` | yes | yes |
| `ICU_VERSION` | no | no |
| `IFNULL` | yes | yes |
| `IN` | no | yes |
| `INET6_ATON` | no | yes |
| `INET6_NTOA` | no | yes |
| `INET_ATON` | no | yes |
| `INET_NTOA` | no | yes |
| `INSTR` | yes | yes |
| `INTERNAL_AUTO_INCREMENT` | no | no |
| `INTERNAL_AVG_ROW_LENGTH` | no | no |
| `INTERNAL_CHECKSUM` | no | no |
| `INTERNAL_CHECK_TIME` | no | no |
| `INTERNAL_DATA_FREE` | no | no |
| `INTERNAL_DATA_LENGTH` | no | no |
| `INTERNAL_DD_CHAR_LENGTH` | no | no |
| `INTERNAL_GET_COMMENT_OR_ERROR` | no | no |
| `INTERNAL_GET_ENABLED_ROLE_JSON` | no | no |
| `INTERNAL_GET_HOSTNAME` | no | no |
| `INTERNAL_GET_USERNAME` | no | no |
| `INTERNAL_GET_VIEW_WARNING_OR_ERROR` | no | no |
| `INTERNAL_INDEX_COLUMN_CARDINALITY` | no | no |
| `INTERNAL_INDEX_LENGTH` | no | no |
| `INTERNAL_IS_ENABLED_ROLE` | no | no |
| `INTERNAL_IS_MANDATORY_ROLE` | no | no |
| `INTERNAL_KEYS_DISABLED` | no | no |
| `INTERNAL_MAX_DATA_LENGTH` | no | no |
| `INTERNAL_TABLE_ROWS` | no | no |
| `INTERNAL_UPDATE_TIME` | no | no |
| `INTERVAL` | no | no |
| `IS` | no | no |
| `ISNULL` | no | yes |
| `IS_FREE_LOCK` | no | no |
| `IS_IPV4` | no | no |
| `IS_IPV4_COMPAT` | no | no |
| `IS_IPV4_MAPPED` | no | no |
| `IS_IPV6` | no | no |
| `IS_USED_LOCK` | no | no |
| `IS_UUID` | no | no |
| `IS_VISIBLE_DD_OBJECT` | no | no |
| `JSON_ARRAY` | yes | no |
| `JSON_ARRAYAGG` | yes | no |
| `JSON_ARRAY_APPEND` | no | no |
| `JSON_ARRAY_INSERT` | no | no |
| `JSON_CONTAINS` | yes | no |
| `JSON_CONTAINS_PATH` | yes | no |
| `JSON_DEPTH` | no | no |
| `JSON_EXTRACT` | yes | no |
| `JSON_INSERT` | no | no |
| `JSON_KEYS` | yes | no |
| `JSON_LENGTH` | yes | no |
| `JSON_MERGE` | no | no |
| `JSON_OBJECT` | yes | no |
| `JSON_OBJECTAGG` | yes | no |
| `JSON_OVERLAPS` | no | no |
| `JSON_PRETTY` | no | no |
| `JSON_QUOTE` | no | no |
| `JSON_REMOVE` | no | no |
| `JSON_REPLACE` | no | no |
| `JSON_SCHEMA_VALID` | no | no |
| `JSON_SCHEMA_VALIDATION_REPORT` | no | no |
| `JSON_SEARCH` | yes | no |
| `JSON_SET` | no | no |
| `JSON_STORAGE_FREE` | no | no |
| `JSON_STORAGE_SIZE` | no | no |
| `JSON_TABLE` | no | no |
| `JSON_TYPE` | yes | no |
| `JSON_UNQUOTE` | yes | no |
| `JSON_VALID` | yes | no |
| `JSON_VALUE` | yes | yes |
| `LAG` | no | yes |
| `LAST_DAY` | yes | yes |
| `LAST_INSERT_ID` | no | no |
| `LAST_VALUE` | no | yes |
| `LCASE` | yes | yes |
| `LEAD` | no | yes |
| `LEAST` | yes | yes |
| `LEFT` | yes | yes |
| `LENGTH` | yes | yes |
| `LIKE` | no | yes |
| `LN` | yes | yes |
| `LOAD_FILE` | no | no |
| `LOCALTIME` | no | no |
| `LOCALTIMESTAMP` | no | no |
| `LOCATE` | yes | yes |
| `LOG` | yes | yes |
| `LOG10` | yes | yes |
| `LOG2` | yes | yes |
| `LOWER` | yes | yes |
| `LPAD` | yes | yes |
| `LTRIM` | no | yes |
| `MAKEDATE` | yes | yes |
| `MAKETIME` | yes | no |
| `MAKE_SET` | no | no |
| `MAX` | yes | yes |
| `MBRCONTAINS` | no | no |
| `MBRCOVEREDBY` | no | no |
| `MBRCOVERS` | no | no |
| `MBRDISJOINT` | no | no |
| `MBREQUALS` | no | no |
| `MBRINTERSECTS` | no | no |
| `MBROVERLAPS` | no | no |
| `MBRTOUCHES` | no | no |
| `MBRWITHIN` | no | no |
| `MD5` | yes | yes |
| `MICROSECOND` | no | no |
| `MID` | no | yes |
| `MIN` | yes | yes |
| `MINUTE` | yes | yes |
| `MOD` | yes | yes |
| `MONTH` | yes | yes |
| `MONTHNAME` | yes | yes |
| `NAME_CONST` | no | no |
| `NOW` | yes | yes |
| `NTH_VALUE` | no | yes |
| `NTILE` | no | yes |
| `NULLIF` | yes | yes |
| `OCT` | no | no |
| `OCTET_LENGTH` | no | yes |
| `OR` | no | yes |
| `ORD` | yes | no |
| `PERCENT_RANK` | no | yes |
| `PERIOD_ADD` | no | no |
| `PERIOD_DIFF` | no | no |
| `PI` | no | yes |
| `POSITION` | no | yes |
| `POW` | yes | yes |
| `POWER` | yes | yes |
| `PS_CURRENT_THREAD_ID` | no | no |
| `PS_THREAD_ID` | no | no |
| `QUARTER` | yes | yes |
| `QUOTE` | no | no |
| `RADIANS` | no | yes |
| `RAND` | yes | yes |
| `RANDOM_BYTES` | no | no |
| `RANK` | yes | yes |
| `REGEXP` | no | no |
| `REGEXP_INSTR` | yes | no |
| `REGEXP_LIKE` | yes | no |
| `REGEXP_REPLACE` | yes | yes |
| `REGEXP_SUBSTR` | yes | no |
| `RELEASE_ALL_LOCKS` | no | no |
| `RELEASE_LOCK` | no | no |
| `REVERSE` | yes | yes |
| `RIGHT` | yes | yes |
| `ROLES_GRAPHML` | no | no |
| `ROUND` | yes | yes |
| `ROW_COUNT` | no | no |
| `ROW_NUMBER` | yes | yes |
| `RPAD` | yes | yes |
| `RTRIM` | no | yes |
| `SCHEMA` | no | yes |
| `SECOND` | yes | yes |
| `SEC_TO_TIME` | yes | no |
| `SESSION_USER` | no | no |
| `SHA1` | no | yes |
| `SHA2` | no | no |
| `SIGN` | yes | yes |
| `SIN` | no | yes |
| `SLEEP` | no | yes |
| `SOUNDEX` | no | yes |
| `SPACE` | yes | yes |
| `SQRT` | yes | yes |
| `STATEMENT_DIGEST` | no | no |
| `STATEMENT_DIGEST_TEXT` | no | no |
| `STD` | yes | yes |
| `STDDEV` | yes | no |
| `STDDEV_POP` | yes | yes |
| `STDDEV_SAMP` | yes | yes |
| `STRCMP` | no | no |
| `STR_TO_DATE` | yes | yes |
| `ST_AREA` | no | no |
| `ST_ASBINARY` | no | no |
| `ST_ASGEOJSON` | no | no |
| `ST_ASTEXT` | no | no |
| `ST_BUFFER` | no | no |
| `ST_BUFFER_STRATEGY` | no | no |
| `ST_CENTROID` | no | no |
| `ST_COLLECT` | no | no |
| `ST_CONTAINS` | no | no |
| `ST_CONVEXHULL` | no | no |
| `ST_CROSSES` | no | no |
| `ST_DIFFERENCE` | no | no |
| `ST_DIMENSION` | no | no |
| `ST_DISJOINT` | no | no |
| `ST_DISTANCE` | no | no |
| `ST_DISTANCE_SPHERE` | no | no |
| `ST_ENDPOINT` | no | no |
| `ST_ENVELOPE` | no | no |
| `ST_EQUALS` | no | no |
| `ST_EXTERIORRING` | no | no |
| `ST_FRECHETDISTANCE` | no | no |
| `ST_GEOHASH` | no | no |
| `ST_GEOMCOLLFROMTEXT` | no | no |
| `ST_GEOMCOLLFROMWKB` | no | no |
| `ST_GEOMETRYN` | no | no |
| `ST_GEOMETRYTYPE` | no | no |
| `ST_GEOMFROMGEOJSON` | no | no |
| `ST_GEOMFROMTEXT` | no | no |
| `ST_GEOMFROMWKB` | no | no |
| `ST_HAUSDORFFDISTANCE` | no | no |
| `ST_INTERIORRINGN` | no | no |
| `ST_INTERSECTION` | no | no |
| `ST_INTERSECTS` | no | no |
| `ST_ISCLOSED` | no | no |
| `ST_ISEMPTY` | no | no |
| `ST_ISSIMPLE` | no | no |
| `ST_ISVALID` | no | no |
| `ST_LATFROMGEOHASH` | no | no |
| `ST_LATITUDE` | no | no |
| `ST_LENGTH` | no | no |
| `ST_LINEFROMTEXT` | no | no |
| `ST_LINEFROMWKB` | no | yes |
| `ST_LINEINTERPOLATEPOINT` | no | no |
| `ST_LINEINTERPOLATEPOINTS` | no | no |
| `ST_LONGFROMGEOHASH` | no | no |
| `ST_LONGITUDE` | no | no |
| `ST_MAKEENVELOPE` | no | no |
| `ST_MLINEFROMTEXT` | no | no |
| `ST_MLINEFROMWKB` | no | yes |
| `ST_MPOINTFROMTEXT` | no | no |
| `ST_MPOINTFROMWKB` | no | no |
| `ST_MPOLYFROMTEXT` | no | no |
| `ST_MPOLYFROMWKB` | no | yes |
| `ST_NUMGEOMETRIES` | no | no |
| `ST_NUMINTERIORRINGS` | no | no |
| `ST_NUMPOINTS` | no | no |
| `ST_OVERLAPS` | no | no |
| `ST_POINTATDISTANCE` | no | no |
| `ST_POINTFROMGEOHASH` | no | no |
| `ST_POINTFROMTEXT` | no | no |
| `ST_POINTFROMWKB` | no | yes |
| `ST_POINTN` | no | no |
| `ST_POLYFROMTEXT` | no | no |
| `ST_POLYFROMWKB` | no | yes |
| `ST_SIMPLIFY` | no | no |
| `ST_SRID` | no | no |
| `ST_STARTPOINT` | no | no |
| `ST_SWAPXY` | no | no |
| `ST_SYMDIFFERENCE` | no | no |
| `ST_TOUCHES` | no | no |
| `ST_TRANSFORM` | no | no |
| `ST_UNION` | no | no |
| `ST_VALIDATE` | no | no |
| `ST_WITHIN` | no | no |
| `ST_X` | no | no |
| `ST_Y` | no | no |
| `SUBDATE` | no | yes |
| `SUBSTR` | yes | yes |
| `SUBSTRING` | yes | yes |
| `SUBSTRING_INDEX` | yes | yes |
| `SUBTIME` | no | no |
| `SUM` | yes | yes |
| `SYSDATE` | no | no |
| `SYSTEM_USER` | no | no |
| `TAN` | no | yes |
| `TIMEDIFF` | no | yes |
| `TIMESTAMPADD` | yes | no |
| `TIMESTAMPDIFF` | yes | yes |
| `TIME_FORMAT` | no | no |
| `TIME_TO_SEC` | yes | no |
| `TO_BASE64` | yes | yes |
| `TO_DAYS` | yes | yes |
| `TO_SECONDS` | no | no |
| `TRIM` | yes | yes |
| `TRUNCATE` | yes | yes |
| `UCASE` | yes | yes |
| `UNCOMPRESS` | no | no |
| `UNCOMPRESSED_LENGTH` | no | no |
| `UNHEX` | yes | yes |
| `UNIX_TIMESTAMP` | yes | no |
| `UPPER` | yes | yes |
| `USER` | no | yes |
| `UTC_DATE` | no | no |
| `UTC_TIME` | no | no |
| `UTC_TIMESTAMP` | no | yes |
| `UUID` | no | no |
| `UUID_SHORT` | no | no |
| `UUID_TO_BIN` | no | no |
| `VALIDATE_PASSWORD_STRENGTH` | no | no |
| `VALUES` | no | no |
| `VARIANCE` | yes | no |
| `VAR_POP` | yes | yes |
| `VAR_SAMP` | yes | yes |
| `VERSION` | no | yes |
| `WEEK` | yes | yes |
| `WEEKDAY` | yes | no |
| `WEEKOFYEAR` | yes | no |
| `WEIGHT_STRING` | no | no |
| `XOR` | no | yes |
| `YEAR` | yes | yes |
| `YEARWEEK` | yes | yes |

### Keywords

| Keyword | MySQL reserved | Pintail | ClickHouse |
|---|---|---|---|
| `ACCESSIBLE` | reserved | no | no |
| `ACCOUNT` |  | no | no |
| `ACTION` |  | no | no |
| `ACTIVE` |  | no | no |
| `ADD` | reserved | no | yes |
| `ADMIN` |  | no | no |
| `AFTER` |  | no | yes |
| `AGAINST` |  | no | no |
| `AGGREGATE` |  | no | no |
| `ALGORITHM` |  | no | yes |
| `ALL` | reserved | yes | yes |
| `ALTER` | reserved | n/a | yes |
| `ALWAYS` |  | no | no |
| `ANALYZE` | reserved | n/a | no |
| `AND` | reserved | yes | yes |
| `ANY` |  | no | yes |
| `ARRAY` |  | no | no |
| `AS` | reserved | yes | yes |
| `ASC` | reserved | yes | yes |
| `ASCII` |  | no | no |
| `ASENSITIVE` | reserved | no | no |
| `ASSIGN_GTIDS_TO_ANONYMOUS_TRANSACTIONS` |  | no | no |
| `AT` |  | no | no |
| `ATTRIBUTE` |  | n/a | no |
| `AUTHENTICATION` |  | n/a | no |
| `AUTO` |  | no | no |
| `AUTO_INCREMENT` |  | no | yes |
| `AUTOEXTEND_SIZE` |  | no | no |
| `AVG` |  | yes | no |
| `AVG_ROW_LENGTH` |  | no | no |
| `BACKUP` |  | n/a | yes |
| `BEFORE` | reserved | no | no |
| `BEGIN` |  | no | no |
| `BERNOULLI` |  | no | no |
| `BETWEEN` | reserved | yes | yes |
| `BIGINT` | reserved | no | no |
| `BINARY` | reserved | yes | no |
| `BINLOG` |  | n/a | no |
| `BIT` |  | no | no |
| `BLOB` | reserved | no | no |
| `BLOCK` |  | no | no |
| `BOOL` |  | no | no |
| `BOOLEAN` |  | no | no |
| `BOTH` | reserved | no | yes |
| `BTREE` |  | no | no |
| `BUCKETS` |  | no | no |
| `BULK` |  | no | no |
| `BY` | reserved | yes | yes |
| `BYTE` |  | no | no |
| `CACHE` |  | n/a | no |
| `CALL` | reserved | no | no |
| `CASCADE` | reserved | no | yes |
| `CASCADED` |  | no | no |
| `CASE` | reserved | yes | yes |
| `CATALOG_NAME` |  | no | no |
| `CHAIN` |  | no | no |
| `CHALLENGE_RESPONSE` |  | no | no |
| `CHANGE` | reserved | no | yes |
| `CHANGED` |  | no | yes |
| `CHANNEL` |  | n/a | no |
| `CHAR` | reserved | yes | yes |
| `CHARACTER` | reserved | yes | yes |
| `CHARSET` |  | no | no |
| `CHECK` | reserved | no | yes |
| `CHECKSUM` |  | n/a | no |
| `CIPHER` |  | no | no |
| `CLASS_ORIGIN` |  | no | no |
| `CLIENT` |  | no | no |
| `CLONE` |  | n/a | no |
| `CLOSE` |  | no | no |
| `COALESCE` |  | yes | no |
| `CODE` |  | no | no |
| `COLLATE` | reserved | yes | yes |
| `COLLATION` |  | no | no |
| `COLUMN` | reserved | no | yes |
| `COLUMN_FORMAT` |  | no | no |
| `COLUMN_NAME` |  | no | no |
| `COLUMNS` |  | no | yes |
| `COMMENT` |  | no | yes |
| `COMMIT` |  | n/a | yes |
| `COMMITTED` |  | no | no |
| `COMPACT` |  | no | no |
| `COMPLETION` |  | no | no |
| `COMPONENT` |  | n/a | no |
| `COMPRESSED` |  | no | no |
| `COMPRESSION` |  | no | yes |
| `CONCURRENT` |  | no | no |
| `CONDITION` | reserved | no | no |
| `CONNECTION` |  | no | no |
| `CONSISTENT` |  | no | no |
| `CONSTRAINT` | reserved | no | yes |
| `CONSTRAINT_CATALOG` |  | no | no |
| `CONSTRAINT_NAME` |  | no | no |
| `CONSTRAINT_SCHEMA` |  | no | no |
| `CONTAINS` |  | no | no |
| `CONTEXT` |  | no | no |
| `CONTINUE` | reserved | no | no |
| `CONVERT` | reserved | yes | no |
| `CPU` |  | no | no |
| `CREATE` | reserved | n/a | yes |
| `CROSS` | reserved | yes | yes |
| `CUBE` | reserved | no | yes |
| `CUME_DIST` | reserved | no | no |
| `CURRENT` |  | yes | no |
| `CURRENT_DATE` | reserved | no | no |
| `CURRENT_TIME` | reserved | no | no |
| `CURRENT_TIMESTAMP` | reserved | no | no |
| `CURRENT_USER` | reserved | no | yes |
| `CURSOR` | reserved | n/a | no |
| `CURSOR_NAME` |  | no | no |
| `DATA` |  | no | yes |
| `DATABASE` | reserved | no | yes |
| `DATABASES` | reserved | no | yes |
| `DATAFILE` |  | n/a | no |
| `DATE` |  | yes | yes |
| `DATETIME` |  | yes | no |
| `DAY` |  | yes | yes |
| `DAY_HOUR` | reserved | no | no |
| `DAY_MICROSECOND` | reserved | no | no |
| `DAY_MINUTE` | reserved | no | no |
| `DAY_SECOND` | reserved | no | no |
| `DEALLOCATE` |  | no | yes |
| `DEC` | reserved | no | no |
| `DECIMAL` | reserved | yes | no |
| `DECLARE` | reserved | no | no |
| `DEFAULT` | reserved | yes | yes |
| `DEFAULT_AUTH` |  | no | no |
| `DEFINER` |  | no | yes |
| `DEFINITION` |  | no | no |
| `DELAY_KEY_WRITE` |  | no | no |
| `DELAYED` | reserved | no | no |
| `DELETE` | reserved | n/a | yes |
| `DENSE_RANK` | reserved | no | no |
| `DESC` | reserved | yes | yes |
| `DESCRIBE` | reserved | no | yes |
| `DESCRIPTION` |  | no | no |
| `DETERMINISTIC` | reserved | no | no |
| `DIAGNOSTICS` |  | no | no |
| `DIRECTORY` |  | no | no |
| `DISABLE` |  | no | no |
| `DISCARD` |  | no | no |
| `DISK` |  | no | yes |
| `DISTINCT` | reserved | yes | yes |
| `DISTINCTROW` | reserved | yes | no |
| `DIV` | reserved | no | yes |
| `DO` |  | no | no |
| `DOUBLE` | reserved | no | no |
| `DROP` | reserved | n/a | yes |
| `DUAL` | reserved | yes | no |
| `DUMPFILE` |  | no | no |
| `DUPLICATE` |  | no | no |
| `DYNAMIC` |  | no | no |
| `EACH` | reserved | no | no |
| `ELSE` | reserved | yes | yes |
| `ELSEIF` | reserved | yes | no |
| `EMPTY` | reserved | no | yes |
| `ENABLE` |  | no | no |
| `ENCLOSED` | reserved | no | no |
| `ENCRYPTION` |  | no | no |
| `END` |  | yes | yes |
| `ENDS` |  | no | no |
| `ENFORCED` |  | no | yes |
| `ENGINE` |  | n/a | yes |
| `ENGINE_ATTRIBUTE` |  | n/a | no |
| `ENGINES` |  | n/a | no |
| `ENUM` |  | no | no |
| `ERROR` |  | no | no |
| `ERRORS` |  | no | no |
| `ESCAPE` |  | yes | no |
| `ESCAPED` | reserved | no | no |
| `EVENT` |  | n/a | yes |
| `EVENTS` |  | n/a | yes |
| `EVERY` |  | no | yes |
| `EXCEPT` | reserved | yes | yes |
| `EXCHANGE` |  | no | no |
| `EXCLUDE` |  | no | no |
| `EXECUTE` |  | no | yes |
| `EXISTS` | reserved | yes | yes |
| `EXIT` | reserved | no | no |
| `EXPANSION` |  | no | no |
| `EXPIRE` |  | no | no |
| `EXPLAIN` | reserved | no | yes |
| `EXPORT` |  | no | no |
| `EXTENDED` |  | no | yes |
| `EXTENT_SIZE` |  | no | no |
| `FACTOR` |  | no | no |
| `FAILED_LOGIN_ATTEMPTS` |  | no | no |
| `FALSE` | reserved | yes | yes |
| `FAST` |  | no | no |
| `FAULTS` |  | no | no |
| `FETCH` | reserved | no | yes |
| `FIELDS` |  | no | yes |
| `FILE` |  | no | yes |
| `FILE_BLOCK_SIZE` |  | no | no |
| `FILTER` |  | no | yes |
| `FINISH` |  | no | no |
| `FIRST` |  | no | yes |
| `FIRST_VALUE` | reserved | no | no |
| `FIXED` |  | no | no |
| `FLOAT` | reserved | no | no |
| `FLOAT4` | reserved | no | no |
| `FLOAT8` | reserved | no | no |
| `FLUSH` |  | n/a | no |
| `FOLLOWING` |  | yes | yes |
| `FOLLOWS` |  | no | no |
| `FOR` | reserved | no | yes |
| `FORCE` | reserved | no | yes |
| `FOREIGN` | reserved | no | yes |
| `FORMAT` |  | no | yes |
| `FOUND` |  | no | no |
| `FROM` | reserved | yes | yes |
| `FULL` |  | no | yes |
| `FULLTEXT` | reserved | no | yes |
| `FUNCTION` | reserved | n/a | yes |
| `GENERAL` |  | no | no |
| `GENERATE` |  | no | no |
| `GENERATED` | reserved | no | no |
| `GEOMCOLLECTION` |  | n/a | no |
| `GEOMETRY` |  | n/a | no |
| `GEOMETRYCOLLECTION` |  | n/a | no |
| `GET` | reserved | no | no |
| `GET_FORMAT` |  | no | no |
| `GET_SOURCE_PUBLIC_KEY` |  | no | no |
| `GLOBAL` |  | no | yes |
| `GRANT` | reserved | n/a | yes |
| `GRANTS` |  | no | no |
| `GROUP` | reserved | yes | no |
| `GROUP_REPLICATION` |  | no | no |
| `GROUPING` | reserved | no | no |
| `GROUPS` | reserved | yes | yes |
| `GTID_ONLY` |  | n/a | no |
| `GTIDS` |  | n/a | no |
| `HANDLER` |  | n/a | no |
| `HASH` |  | no | yes |
| `HAVING` | reserved | yes | yes |
| `HELP` |  | no | no |
| `HIGH_PRIORITY` | reserved | no | no |
| `HISTOGRAM` |  | no | no |
| `HISTORY` |  | no | no |
| `HOST` |  | no | yes |
| `HOSTS` |  | no | no |
| `HOUR` |  | yes | yes |
| `HOUR_MICROSECOND` | reserved | no | no |
| `HOUR_MINUTE` | reserved | no | no |
| `HOUR_SECOND` | reserved | no | no |
| `IDENTIFIED` |  | no | yes |
| `IF` | reserved | yes | no |
| `IGNORE` | reserved | no | no |
| `IGNORE_SERVER_IDS` |  | no | no |
| `IMPORT` |  | no | no |
| `IN` | reserved | yes | yes |
| `INACTIVE` |  | no | no |
| `INDEX` | reserved | no | yes |
| `INDEXES` |  | no | yes |
| `INFILE` | reserved | no | no |
| `INITIAL` |  | no | no |
| `INITIAL_SIZE` |  | no | no |
| `INITIATE` |  | no | no |
| `INNER` | reserved | yes | yes |
| `INOUT` | reserved | no | no |
| `INSENSITIVE` | reserved | no | no |
| `INSERT` | reserved | n/a | no |
| `INSERT_METHOD` |  | no | no |
| `INSTALL` |  | n/a | no |
| `INSTANCE` |  | no | no |
| `INT` | reserved | no | no |
| `INT1` | reserved | no | no |
| `INT2` | reserved | no | no |
| `INT3` | reserved | no | no |
| `INT4` | reserved | no | no |
| `INT8` | reserved | no | no |
| `INTEGER` | reserved | no | no |
| `INTERSECT` | reserved | yes | yes |
| `INTERVAL` | reserved | yes | yes |
| `INTO` | reserved | no | no |
| `INVISIBLE` |  | no | yes |
| `INVOKER` |  | no | yes |
| `IO` |  | no | no |
| `IO_AFTER_GTIDS` | reserved | no | no |
| `IO_BEFORE_GTIDS` | reserved | no | no |
| `IO_THREAD` |  | no | no |
| `IPC` |  | no | no |
| `IS` | reserved | yes | no |
| `ISOLATION` |  | no | no |
| `ISSUER` |  | no | no |
| `ITERATE` | reserved | no | no |
| `JOIN` | reserved | yes | yes |
| `JSON` |  | yes | no |
| `JSON_TABLE` | reserved | no | no |
| `JSON_VALUE` |  | no | no |
| `KEY` | reserved | no | yes |
| `KEY_BLOCK_SIZE` |  | no | no |
| `KEYRING` |  | no | no |
| `KEYS` | reserved | no | yes |
| `KILL` | reserved | no | yes |
| `LAG` | reserved | no | no |
| `LANGUAGE` |  | no | no |
| `LAST` |  | no | yes |
| `LAST_VALUE` | reserved | no | no |
| `LATERAL` | reserved | no | no |
| `LEAD` | reserved | no | no |
| `LEADING` | reserved | no | yes |
| `LEAVE` | reserved | no | no |
| `LEAVES` |  | no | no |
| `LEFT` | reserved | yes | yes |
| `LESS` |  | no | no |
| `LEVEL` |  | no | yes |
| `LIKE` | reserved | yes | yes |
| `LIMIT` | reserved | yes | yes |
| `LINEAR` | reserved | no | yes |
| `LINES` | reserved | no | no |
| `LINESTRING` |  | n/a | no |
| `LIST` |  | no | yes |
| `LOAD` | reserved | no | no |
| `LOCAL` |  | no | yes |
| `LOCALTIME` | reserved | no | no |
| `LOCALTIMESTAMP` | reserved | no | no |
| `LOCK` | reserved | n/a | no |
| `LOCKED` |  | no | no |
| `LOCKS` |  | no | no |
| `LOG` |  | no | no |
| `LOGFILE` |  | no | no |
| `LOGS` |  | no | no |
| `LONG` | reserved | no | no |
| `LONGBLOB` | reserved | no | no |
| `LONGTEXT` | reserved | no | no |
| `LOOP` | reserved | no | no |
| `LOW_PRIORITY` | reserved | no | no |
| `MANUAL` |  | no | no |
| `MASTER` |  | n/a | no |
| `MATCH` | reserved | no | yes |
| `MAX_CONNECTIONS_PER_HOUR` |  | no | no |
| `MAX_QUERIES_PER_HOUR` |  | no | no |
| `MAX_ROWS` |  | no | no |
| `MAX_SIZE` |  | no | no |
| `MAX_UPDATES_PER_HOUR` |  | no | no |
| `MAX_USER_CONNECTIONS` |  | no | no |
| `MAXVALUE` | reserved | no | no |
| `MEDIUM` |  | no | no |
| `MEDIUMBLOB` | reserved | no | no |
| `MEDIUMINT` | reserved | no | no |
| `MEDIUMTEXT` | reserved | no | no |
| `MEMBER` |  | no | no |
| `MEMORY` |  | no | yes |
| `MERGE` |  | n/a | no |
| `MESSAGE_TEXT` |  | no | no |
| `MICROSECOND` |  | yes | yes |
| `MIDDLEINT` | reserved | no | no |
| `MIGRATE` |  | no | no |
| `MIN_ROWS` |  | no | no |
| `MINUTE` |  | yes | yes |
| `MINUTE_MICROSECOND` | reserved | no | no |
| `MINUTE_SECOND` | reserved | no | no |
| `MOD` | reserved | no | yes |
| `MODE` |  | no | no |
| `MODIFIES` | reserved | no | no |
| `MODIFY` |  | no | yes |
| `MONTH` |  | yes | yes |
| `MULTILINESTRING` |  | n/a | no |
| `MULTIPOINT` |  | n/a | no |
| `MULTIPOLYGON` |  | n/a | no |
| `MUTEX` |  | no | no |
| `MYSQL_ERRNO` |  | no | no |
| `NAME` |  | no | yes |
| `NAMES` |  | no | no |
| `NATIONAL` |  | no | no |
| `NATURAL` | reserved | yes | no |
| `NCHAR` |  | yes | no |
| `NDB` |  | no | no |
| `NDBCLUSTER` |  | no | no |
| `NESTED` |  | no | no |
| `NETWORK_NAMESPACE` |  | no | no |
| `NEVER` |  | no | no |
| `NEW` |  | no | no |
| `NEXT` |  | no | yes |
| `NO` |  | no | no |
| `NO_WAIT` |  | no | no |
| `NO_WRITE_TO_BINLOG` | reserved | no | no |
| `NODEGROUP` |  | no | no |
| `NONE` |  | no | yes |
| `NOT` | reserved | yes | yes |
| `NOWAIT` |  | no | no |
| `NTH_VALUE` | reserved | no | no |
| `NTILE` | reserved | no | no |
| `NULL` | reserved | yes | yes |
| `NULLS` |  | no | yes |
| `NUMBER` |  | no | no |
| `NUMERIC` | reserved | no | no |
| `NVARCHAR` |  | no | no |
| `OF` | reserved | no | no |
| `OFF` |  | no | no |
| `OFFSET` |  | yes | yes |
| `OJ` |  | no | no |
| `OLD` |  | no | no |
| `ON` | reserved | yes | yes |
| `ONE` |  | no | no |
| `ONLY` |  | no | yes |
| `OPEN` |  | no | no |
| `OPTIMIZE` | reserved | n/a | no |
| `OPTIMIZER_COSTS` | reserved | no | no |
| `OPTION` | reserved | no | no |
| `OPTIONAL` |  | no | no |
| `OPTIONALLY` | reserved | no | no |
| `OPTIONS` |  | no | no |
| `OR` | reserved | yes | yes |
| `ORDER` | reserved | yes | no |
| `ORDINALITY` |  | no | no |
| `ORGANIZATION` |  | no | no |
| `OTHERS` |  | no | no |
| `OUT` | reserved | no | no |
| `OUTER` | reserved | yes | yes |
| `OUTFILE` | reserved | no | no |
| `OVER` | reserved | yes | yes |
| `OWNER` |  | no | no |
| `PACK_KEYS` |  | no | no |
| `PAGE` |  | no | no |
| `PARALLEL` |  | no | no |
| `PARSE_TREE` |  | no | no |
| `PARSER` |  | no | no |
| `PARTIAL` |  | no | yes |
| `PARTITION` | reserved | yes | yes |
| `PARTITIONING` |  | n/a | no |
| `PARTITIONS` |  | n/a | yes |
| `PASSWORD` |  | n/a | no |
| `PASSWORD_LOCK_TIME` |  | no | no |
| `PATH` |  | no | no |
| `PERCENT_RANK` | reserved | no | no |
| `PERSIST` |  | no | no |
| `PERSIST_ONLY` |  | no | no |
| `PHASE` |  | no | no |
| `PLUGIN` |  | n/a | no |
| `PLUGIN_DIR` |  | no | no |
| `PLUGINS` |  | n/a | no |
| `POINT` |  | n/a | no |
| `POLYGON` |  | n/a | no |
| `PORT` |  | no | no |
| `PRECEDES` |  | no | no |
| `PRECEDING` |  | yes | yes |
| `PRECISION` | reserved | no | yes |
| `PREPARE` |  | no | yes |
| `PRESERVE` |  | no | no |
| `PREV` |  | no | no |
| `PRIMARY` | reserved | no | yes |
| `PRIVILEGE_CHECKS_USER` |  | no | no |
| `PRIVILEGES` |  | n/a | no |
| `PROCEDURE` | reserved | n/a | no |
| `PROCESS` |  | no | no |
| `PROCESSLIST` |  | no | no |
| `PROFILE` |  | no | yes |
| `PROFILES` |  | no | yes |
| `PROXY` |  | n/a | no |
| `PURGE` | reserved | n/a | no |
| `QUALIFY` | reserved | no | yes |
| `QUARTER` |  | yes | yes |
| `QUERY` |  | no | yes |
| `QUICK` |  | no | no |
| `RANDOM` |  | no | no |
| `RANGE` | reserved | yes | yes |
| `RANK` | reserved | no | no |
| `READ` | reserved | no | yes |
| `READ_ONLY` |  | no | no |
| `READ_WRITE` | reserved | no | no |
| `READS` | reserved | no | no |
| `REAL` | reserved | no | no |
| `REBUILD` |  | no | no |
| `RECOVER` |  | no | no |
| `RECURSIVE` | reserved | yes | yes |
| `REDO_BUFFER_SIZE` |  | no | no |
| `REDUNDANT` |  | no | no |
| `REFERENCE` |  | no | no |
| `REFERENCES` | reserved | no | yes |
| `REGEXP` | reserved | yes | yes |
| `REGISTRATION` |  | no | no |
| `RELAY` |  | n/a | no |
| `RELAY_LOG_FILE` |  | n/a | no |
| `RELAY_LOG_POS` |  | n/a | no |
| `RELAY_THREAD` |  | n/a | no |
| `RELAYLOG` |  | n/a | no |
| `RELEASE` | reserved | no | no |
| `RELOAD` |  | no | no |
| `REMOVE` |  | no | yes |
| `RENAME` | reserved | n/a | yes |
| `REORGANIZE` |  | no | no |
| `REPAIR` |  | n/a | no |
| `REPEAT` | reserved | no | no |
| `REPEATABLE` |  | no | no |
| `REPLACE` | reserved | n/a | yes |
| `REPLICA` |  | n/a | no |
| `REPLICAS` |  | n/a | no |
| `REPLICATE_DO_DB` |  | n/a | no |
| `REPLICATE_DO_TABLE` |  | n/a | no |
| `REPLICATE_IGNORE_DB` |  | n/a | no |
| `REPLICATE_IGNORE_TABLE` |  | n/a | no |
| `REPLICATE_REWRITE_DB` |  | n/a | no |
| `REPLICATE_WILD_DO_TABLE` |  | n/a | no |
| `REPLICATE_WILD_IGNORE_TABLE` |  | n/a | no |
| `REPLICATION` |  | n/a | no |
| `REQUIRE` | reserved | no | no |
| `REQUIRE_ROW_FORMAT` |  | no | no |
| `REQUIRE_TABLE_PRIMARY_KEY_CHECK` |  | no | no |
| `RESET` |  | n/a | no |
| `RESIGNAL` | reserved | no | no |
| `RESOURCE` |  | no | yes |
| `RESPECT` |  | no | no |
| `RESTART` |  | no | no |
| `RESTORE` |  | n/a | yes |
| `RESTRICT` | reserved | no | yes |
| `RESUME` |  | no | yes |
| `RETAIN` |  | no | no |
| `RETURN` | reserved | no | no |
| `RETURNED_SQLSTATE` |  | no | no |
| `RETURNING` |  | no | no |
| `RETURNS` |  | no | no |
| `REUSE` |  | no | no |
| `REVERSE` |  | no | no |
| `REVOKE` | reserved | n/a | yes |
| `RIGHT` | reserved | yes | yes |
| `RLIKE` | reserved | yes | no |
| `ROLE` |  | n/a | no |
| `ROLLBACK` |  | n/a | yes |
| `ROLLUP` |  | no | yes |
| `ROTATE` |  | no | no |
| `ROUTINE` |  | n/a | no |
| `ROW` | reserved | yes | yes |
| `ROW_COUNT` |  | no | no |
| `ROW_FORMAT` |  | no | no |
| `ROW_NUMBER` | reserved | no | no |
| `ROWS` | reserved | yes | yes |
| `RTREE` |  | no | no |
| `S3` |  | no | yes |
| `SAVEPOINT` |  | n/a | no |
| `SCHEDULE` |  | no | no |
| `SCHEMA` | reserved | no | no |
| `SCHEMA_NAME` |  | no | no |
| `SCHEMAS` | reserved | no | no |
| `SECOND` |  | yes | yes |
| `SECOND_MICROSECOND` | reserved | no | no |
| `SECONDARY` |  | no | no |
| `SECONDARY_ENGINE` |  | no | no |
| `SECONDARY_ENGINE_ATTRIBUTE` |  | no | no |
| `SECONDARY_LOAD` |  | no | no |
| `SECONDARY_UNLOAD` |  | no | no |
| `SECURITY` |  | no | no |
| `SELECT` | reserved | yes | yes |
| `SENSITIVE` | reserved | no | no |
| `SEPARATOR` | reserved | yes | no |
| `SERIAL` |  | no | no |
| `SERIALIZABLE` |  | no | no |
| `SERVER` |  | no | yes |
| `SESSION` |  | no | no |
| `SET` | reserved | yes | yes |
| `SHARE` |  | no | no |
| `SHOW` | reserved | no | yes |
| `SHUTDOWN` |  | n/a | no |
| `SIGNAL` | reserved | no | no |
| `SIGNED` |  | yes | yes |
| `SIMPLE` |  | no | yes |
| `SKIP` |  | no | yes |
| `SLAVE` |  | n/a | no |
| `SLOW` |  | no | no |
| `SMALLINT` | reserved | no | no |
| `SNAPSHOT` |  | no | no |
| `SOCKET` |  | no | no |
| `SOME` |  | no | no |
| `SONAME` |  | n/a | no |
| `SOUNDS` |  | no | no |
| `SOURCE` |  | n/a | yes |
| `SOURCE_AUTO_POSITION` |  | n/a | no |
| `SOURCE_BIND` |  | n/a | no |
| `SOURCE_COMPRESSION_ALGORITHMS` |  | n/a | no |
| `SOURCE_CONNECT_RETRY` |  | n/a | no |
| `SOURCE_CONNECTION_AUTO_FAILOVER` |  | n/a | no |
| `SOURCE_DELAY` |  | n/a | no |
| `SOURCE_HEARTBEAT_PERIOD` |  | n/a | no |
| `SOURCE_HOST` |  | n/a | no |
| `SOURCE_LOG_FILE` |  | n/a | no |
| `SOURCE_LOG_POS` |  | n/a | no |
| `SOURCE_PASSWORD` |  | n/a | no |
| `SOURCE_PORT` |  | n/a | no |
| `SOURCE_PUBLIC_KEY_PATH` |  | n/a | no |
| `SOURCE_RETRY_COUNT` |  | n/a | no |
| `SOURCE_SSL` |  | n/a | no |
| `SOURCE_SSL_CA` |  | n/a | no |
| `SOURCE_SSL_CAPATH` |  | n/a | no |
| `SOURCE_SSL_CERT` |  | n/a | no |
| `SOURCE_SSL_CIPHER` |  | n/a | no |
| `SOURCE_SSL_CRL` |  | n/a | no |
| `SOURCE_SSL_CRLPATH` |  | n/a | no |
| `SOURCE_SSL_KEY` |  | n/a | no |
| `SOURCE_SSL_VERIFY_SERVER_CERT` |  | n/a | no |
| `SOURCE_TLS_CIPHERSUITES` |  | n/a | no |
| `SOURCE_TLS_VERSION` |  | n/a | no |
| `SOURCE_USER` |  | n/a | no |
| `SOURCE_ZSTD_COMPRESSION_LEVEL` |  | n/a | no |
| `SPATIAL` | reserved | n/a | yes |
| `SPECIFIC` | reserved | no | no |
| `SQL` | reserved | no | no |
| `SQL_AFTER_GTIDS` |  | no | no |
| `SQL_AFTER_MTS_GAPS` |  | no | no |
| `SQL_BEFORE_GTIDS` |  | no | no |
| `SQL_BIG_RESULT` | reserved | no | no |
| `SQL_BUFFER_RESULT` |  | no | no |
| `SQL_CALC_FOUND_ROWS` | reserved | no | no |
| `SQL_NO_CACHE` |  | no | no |
| `SQL_SMALL_RESULT` | reserved | no | no |
| `SQL_THREAD` |  | no | no |
| `SQL_TSI_DAY` |  | no | yes |
| `SQL_TSI_HOUR` |  | no | yes |
| `SQL_TSI_MINUTE` |  | no | yes |
| `SQL_TSI_MONTH` |  | no | yes |
| `SQL_TSI_QUARTER` |  | no | yes |
| `SQL_TSI_SECOND` |  | no | yes |
| `SQL_TSI_WEEK` |  | no | yes |
| `SQL_TSI_YEAR` |  | no | yes |
| `SQLEXCEPTION` | reserved | no | no |
| `SQLSTATE` | reserved | no | no |
| `SQLWARNING` | reserved | no | no |
| `SRID` |  | n/a | no |
| `SSL` | reserved | no | no |
| `STACKED` |  | no | no |
| `START` |  | no | no |
| `STARTING` | reserved | no | no |
| `STARTS` |  | no | no |
| `STATS_AUTO_RECALC` |  | no | no |
| `STATS_PERSISTENT` |  | no | no |
| `STATS_SAMPLE_PAGES` |  | no | no |
| `STATUS` |  | no | no |
| `STOP` |  | no | no |
| `STORAGE` |  | no | yes |
| `STORED` | reserved | no | no |
| `STRAIGHT_JOIN` | reserved | yes | no |
| `STREAM` |  | no | no |
| `STRING` |  | no | no |
| `SUBCLASS_ORIGIN` |  | no | no |
| `SUBJECT` |  | no | no |
| `SUBPARTITION` |  | n/a | yes |
| `SUBPARTITIONS` |  | n/a | yes |
| `SUPER` |  | no | no |
| `SUSPEND` |  | no | yes |
| `SWAPS` |  | no | no |
| `SWITCHES` |  | no | no |
| `SYSTEM` | reserved | no | yes |
| `TABLE` | reserved | yes | yes |
| `TABLE_CHECKSUM` |  | no | no |
| `TABLE_NAME` |  | no | no |
| `TABLES` |  | no | yes |
| `TABLESAMPLE` | reserved | no | no |
| `TABLESPACE` |  | n/a | no |
| `TEMPORARY` |  | no | yes |
| `TEMPTABLE` |  | no | no |
| `TERMINATED` | reserved | no | no |
| `TEXT` |  | no | no |
| `THAN` |  | no | no |
| `THEN` | reserved | yes | yes |
| `THREAD_PRIORITY` |  | no | no |
| `TIES` |  | no | no |
| `TIME` |  | yes | no |
| `TIMESTAMP` |  | no | yes |
| `TIMESTAMPADD` |  | no | no |
| `TIMESTAMPDIFF` |  | no | no |
| `TINYBLOB` | reserved | no | no |
| `TINYINT` | reserved | no | no |
| `TINYTEXT` | reserved | no | no |
| `TLS` |  | no | no |
| `TO` | reserved | no | yes |
| `TRAILING` | reserved | no | yes |
| `TRANSACTION` |  | n/a | yes |
| `TRIGGER` | reserved | n/a | yes |
| `TRIGGERS` |  | n/a | no |
| `TRUE` | reserved | yes | yes |
| `TRUNCATE` |  | n/a | yes |
| `TYPE` |  | no | yes |
| `TYPES` |  | no | no |
| `UNBOUNDED` |  | yes | yes |
| `UNCOMMITTED` |  | no | no |
| `UNDEFINED` |  | no | no |
| `UNDO` | reserved | no | no |
| `UNDO_BUFFER_SIZE` |  | no | no |
| `UNDOFILE` |  | n/a | no |
| `UNICODE` |  | no | no |
| `UNINSTALL` |  | n/a | no |
| `UNION` | reserved | yes | yes |
| `UNIQUE` | reserved | no | yes |
| `UNKNOWN` |  | yes | no |
| `UNLOCK` | reserved | n/a | no |
| `UNREGISTER` |  | no | no |
| `UNSIGNED` | reserved | yes | yes |
| `UNTIL` |  | no | no |
| `UPDATE` | reserved | n/a | yes |
| `UPGRADE` |  | no | no |
| `URL` |  | no | yes |
| `USAGE` | reserved | no | no |
| `USE` | reserved | no | yes |
| `USE_FRM` |  | no | no |
| `USER` |  | n/a | no |
| `USER_RESOURCES` |  | no | no |
| `USING` | reserved | yes | yes |
| `UTC_DATE` | reserved | no | no |
| `UTC_TIME` | reserved | no | no |
| `UTC_TIMESTAMP` | reserved | no | no |
| `VALIDATION` |  | no | no |
| `VALUE` |  | no | no |
| `VALUES` | reserved | yes | yes |
| `VARBINARY` | reserved | no | no |
| `VARCHAR` | reserved | no | no |
| `VARCHARACTER` | reserved | no | no |
| `VARIABLES` |  | no | no |
| `VARYING` | reserved | no | yes |
| `VCPU` |  | no | no |
| `VIEW` |  | no | yes |
| `VIRTUAL` | reserved | no | no |
| `VISIBLE` |  | no | yes |
| `WAIT` |  | no | no |
| `WARNINGS` |  | no | no |
| `WEEK` |  | yes | yes |
| `WEIGHT_STRING` |  | no | no |
| `WHEN` | reserved | yes | yes |
| `WHERE` | reserved | yes | yes |
| `WHILE` | reserved | no | no |
| `WINDOW` | reserved | yes | yes |
| `WITH` | reserved | yes | yes |
| `WITHOUT` |  | no | no |
| `WORK` |  | no | no |
| `WRAPPER` |  | no | no |
| `WRITE` | reserved | no | yes |
| `X509` |  | no | no |
| `XA` |  | n/a | no |
| `XID` |  | no | no |
| `XML` |  | no | no |
| `XOR` | reserved | yes | no |
| `YEAR` |  | yes | yes |
| `YEAR_MONTH` | reserved | no | no |
| `ZEROFILL` | reserved | no | no |
| `ZONE` |  | no | no |
