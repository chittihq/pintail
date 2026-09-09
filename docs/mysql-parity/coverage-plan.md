# Next MySQL parity coverage plan

Proposed on 2026-09-09 after reviewing the MySQL 8.4 Reference Manual and the
existing corpus, parity ledger, and limitations. This is a testing plan, not
an expansion of the product's supported scope.

The corpus at `2794303` contains 1,373 MySQL-valid queries. The latest run
matched 1,368 and failed five cases: empty-subquery membership with NULL,
decimal BETWEEN, enum numeric comparison, enum casts/arithmetic, and a string
boundary query containing both SUBSTRING and LPAD differences. These are
five failing queries, not necessarily five independent engine defects.

## 1. Make coverage evidence reliable first

- Replace source-regex inventory with an export from the actual case generator.
  The current script inventories 1,088 cases against the generator's 1,373.
  Emit stable case IDs, SQL, fixture ID, explicit session settings, family,
  ordering contract, and links to existing feature/function ledger entries.
  Do not create another independent compatibility ledger.
- Preserve SQL NULL separately from the string `NULL`. The current
  `canonical_value` maps both to the same exact string. Preserve binary bytes
  rather than using lossy UTF-8 conversion. Use a structured MySQL client result
  or an equivalently unambiguous encoding; cover tabs, newlines, NUL bytes,
  invalid UTF-8, empty strings, and strings resembling case markers.
- Compare exact values exactly. Apply float tolerance only when the expected
  MySQL type and the Pintail type permit approximation; never let an accidental
  Pintail float result relax an expected exact-decimal comparison. Check result
  metadata separately through the wire harness.
- Pin the MySQL image digest and record its actual server version, SQL mode,
  time zone, connection charset/collation, fixture hash, and code commit.
  Configure both sessions explicitly. A parse-mode setting alone does not
  exercise session-dependent evaluation.
- Export a failure artifact even on a red run: all case outcomes, expected and
  actual typed values, errors, family totals, and skipped cases. Keep the
  console summary bounded. Retain all known failures as red checks.

Acceptance: runtime inventory equals executed inventory; comparator tests
distinguish NULL/text/binary and detect incorrect numeric type changes; every
case has a recorded outcome. Verify fixture schema and enum/decimal carriers
before assigning an engine cause to a mismatch.

## 2. Expand documented semantic boundaries

The following are coverage priorities inferred from the docs and current
tests. Many families already have examples; the missing work is systematic
coverage across operand types, boundaries, and contexts.

| Order | Coverage slice | Concrete cases and documentation |
|---|---|---|
| 1 | Conversion and exact arithmetic | Cross signed/unsigned integers, decimal scales, doubles, numeric text, enum labels/indices, and NULL. Exercise `=`, `<=>`, inequalities, `BETWEEN`, `IN`, `CASE`, `COALESCE`, casts, joins, and aggregates. Include integers around 2^53 and integer limits, rounding ties, and precision 38 boundaries. Test literals, columns, and scalar-subquery operands separately: temporal conversion differs by context. [Type conversion](https://dev.mysql.com/doc/refman/8.4/en/type-conversion.html), [rounding](https://dev.mysql.com/doc/refman/8.4/en/precision-math-rounding.html), [ENUM](https://dev.mysql.com/doc/refman/8.4/en/enum.html). |
| 2 | Subquery truth and cardinality | RHS states: empty, one NULL, one matching value, nonmatching values, duplicates, and mixed NULL/non-NULL. Cross nullable/nonnullable LHS with `IN`, `NOT IN`, `ANY`, `ALL`, and correlated forms. Distinguish an empty subquery from an aggregate over empty input, which can return one NULL row. Add scalar zero/one/multiple-row behavior to the appropriate success/error corpus. [ALL](https://dev.mysql.com/doc/refman/8.4/en/all-subqueries.html), [EXISTS](https://dev.mysql.com/doc/refman/8.4/en/exists-and-not-exists-subqueries.html), [subquery errors](https://dev.mysql.com/doc/refman/8.4/en/subquery-errors.html). |
| 3 | String storage and collation | Add CHAR, VARCHAR, and binary fixtures with empty strings, trailing spaces, no-break spaces, accents, combining marks, multibyte characters, and embedded NUL. Compare equality, LIKE, joins, grouping, DISTINCT, sets, windows, and explicit COLLATE separately. Exercise column/literal/expression coercibility. Add substring and padding arguments below, at, and beyond valid boundaries. Do not assume GROUP BY and equality necessarily share MySQL's behavior. [CHAR/VARCHAR](https://dev.mysql.com/doc/refman/8.4/en/char.html), [coercibility](https://dev.mysql.com/doc/refman/8.4/en/charset-collation-coercibility.html). |
| 4 | Temporal storage and sessions | Add DATE, TIME, DATETIME, and TIMESTAMP fixtures at fractional precisions 0/3/6. Cover negative and three-digit-hour TIME, leap/calendar boundaries, zero dates, fractional carry, and the TIMESTAMP range. Compare timestamps under UTC, fixed offsets, and named zones across DST transitions, separately from DATETIME. Exercise source insertion rounding versus truncation and subsequent replication. [Temporal types](https://dev.mysql.com/doc/refman/8.4/en/datetime.html), [fractional seconds](https://dev.mysql.com/doc/refman/8.4/en/fractional-seconds.html). |
| 5 | Grouping, windows, and relational composition | Cover primary-key and unique-key functional dependence under ONLY_FULL_GROUP_BY; aliases and HAVING; default versus explicit ROWS/RANGE frames; tied and NULL ordering keys; descending temporal ranges; empty/singleton partitions; and window output filtered in a derived query. Preserve peer groups when making output deterministic: add outer ordering, not an extra window key. Combine with outer joins and set multiplicities. [GROUP BY](https://dev.mysql.com/doc/refman/8.4/en/group-by-handling.html), [frames](https://dev.mysql.com/doc/refman/8.4/en/window-functions-frames.html). |
| 6 | JSON representation and paths | Distinguish SQL NULL, JSON null, missing paths, JSON strings, and SQL strings containing JSON. Cover large integers, numeric spellings/scales, duplicate object keys, array autowrap, last/range/wildcard paths, constructors followed by extraction, and equality/grouping/set contexts. Test typed SQL values embedded in JSON. Avoid asserting an unspecified ordering between unequal objects. [JSON data type](https://dev.mysql.com/doc/refman/8.4/en/json.html). |
| 7 | Session, diagnostics, and client contracts | Exercise supported SQL-mode combinations and connection reset; arithmetic modes, grouping modes, and diagnostic lifetime where claimed. Compare success, NULL, warning, and error outcomes separately. Run parameterized equivalents through text and binary protocols with NULL, unsigned values, decimals, binary bytes, and temporal parameters. Check field type/scale/charset/nullability, repeated execution, and prepare-before-DDL behavior. [SQL modes](https://dev.mysql.com/doc/refman/8.4/en/sql-mode.html), [prepared statements](https://dev.mysql.com/doc/refman/8.4/en/sql-prepared-statements.html), [parameter/result types](https://dev.mysql.com/doc/c-api/8.4/en/c-api-prepared-statement-type-codes.html). |

For the first implementation tranche, start with approximately 120 distinct
conversion/subquery shapes, then about 60 each for strings and temporal
boundaries. These are planning sizes, not acceptance thresholds. Give every
selected rule a normal, boundary, NULL/empty, and composed-expression case
where meaningful. Split multi-expression findings into minimal reproductions.

Use small new typed fixtures rather than extending every test over the same
positive integers and one decimal scale. Keep fixture values invented.

## 3. Repeat representative cases through real data paths

For each high-risk type family, select a small differential query pack and run
it after initial snapshot, CDC insert/update/delete, an ALTER that creates
schema history, flush/compaction, and restart. Compare memtable-only,
persisted-only, and mixed rows. Include updates changing NULL state, enum
declarations, decimal scale, and key values within supported DDL contracts.

Exercise supported binlog row images and metadata settings separately; record
unsupported partial-JSON configurations as scope boundaries rather than
claiming they passed. The binlog can carry full rows or partial information,
so testing only a hand-built TableStore cannot establish replication parity.
[Binary logging settings](https://dev.mysql.com/doc/refman/8.4/en/replication-options-binary-log.html).

Run selected joins, aggregates, sorts, and windows at memory ceilings that
exercise both in-memory and spill paths. Where execution settings exist,
compare alternate plans against MySQL rather than only against each other.

## 4. Grow the existing generators and keep findings reproducible

Extend the existing grammar fuzzer with the new fixture domains, operator/type
pairings, and session profiles. Use fixed regression seeds plus a bounded
additional-seed run; save and minimize every mismatch into the deterministic
corpus. Track unique shapes and covered rule/context combinations separately
from repeated random cases. Validate proposed metamorphic rewrites against
MySQL first, especially around NULL, decimal conversion, and collation.

Keep three outcomes distinct: MySQL-valid supported queries that must match;
documented Pintail scope boundaries; and queries MySQL itself rejects. Compare
error codes/SQLSTATE in a separate negative corpus. Do not add nondeterministic
group representatives or un-ordered LIMIT subsets as byte-exact assertions.

The documented limitations already include decimal precision above 38, some
collation coercibility, temporal cases, and diagnostics. Test those boundaries
honestly; do not silently turn this plan into implementation of all MySQL
features. Maintain separate patch-pinned evidence for MySQL 8.0 and 8.4.

Implementation is sequential: evidence harness, conversion/subqueries,
strings/temporal fixtures, relational/JSON composition, session/wire/CDC
replays, then generator expansion. Commit each slice locally; build and test
on the configured build server. Use touched-crate lint/unit checks and the
targeted differential loop, then one appropriate full validation profile at
the end. No PASS claim until all required cases pass; known failures remain
visible and are never counted as successful coverage.
