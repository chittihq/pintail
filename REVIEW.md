# Read-only SQL compatibility review

## Executive summary

Pintail currently exposes **134 callable function names** and carries **802
MySQL 8.4 differential-oracle cases**. The name count and oracle inventory are
enforced by tests. The last Docker-backed run covered 794 byte-exact cases; the
eight newer JSON, temporal-parsing, and DECIMAL-chain cases remain inventoried
but unexecuted against Docker in this workstation session.

The delivered work covers substantial read-only portions of issues #8, #9,
#13, #17 and #25: metadata discovery, everyday scalar/aggregate behavior,
typed casts, exact DECIMAL operations, value-based RANGE frames, chained named
windows, temporal parsing policy, and richer MySQL result metadata. JSON
mutation/table functions and unsupported binary-JSON semantics remain explicit
scope exclusions rather than unfinished replica reads.

The repository issue checklists are not a reliable completion ledger by
themselves. Several open issues still show already-delivered items as unchecked.
This review therefore distinguishes:

- what the current code and tests prove;
- what the open issue text still requests; and
- what remains a useful next basic read-only SQL increment.

## Review method

The review covered:

- scalar, aggregate, window, comparison, and special-syntax binding;
- physical planning and execution paths used by joins and subqueries;
- `SHOW` and `information_schema` metadata generation;
- the generated function-surface count;
- `parity.md` and `docs/limitations.md`;
- the MySQL oracle inventory; and
- all open GitHub issues, with special attention to #8, #9, #10, #11, #13,
  #14, #16, #17, and #25.

The full workspace test suite and strict workspace clippy gate are green after
the final review corrections. Docker-backed oracle/E2E tests and the benchmark
gate were not run during this series; their performance and external-MySQL
results remain unmeasured here.

## Eight completed tasks

| # | Area | Delivered behavior | Commit |
|---:|---|---|---|
| 1 | Metadata and `SHOW` | `SHOW FULL TABLES`, `LIKE` filters, richer column facts, source defaults, key flags, auto-increment and generated-column metadata | `3fab0bbd`* |
| 2 | Everyday scalar function | MySQL-compatible one-argument `MD5`, including binary input, lowercase hexadecimal output, and NULL propagation | `e9703ede` |
| 3 | Correlated scalar lookup | Decorrelates a non-aggregate scalar lookup only when equality predicates cover a complete declared unique/primary key | `41024665` |
| 4 | Join syntax | Binds safe parenthesized root INNER/CROSS join groups while explicitly rejecting shapes whose tree cannot be preserved | `4e4a6142` |
| 5 | JSON and regex overloads | Multi-path `JSON_EXTRACT`; optional `REGEXP_LIKE` match flags `c`, `i`, `m`, `n`, `u` | `0d2ea099` |
| 6 | Date/time | Implements all literal `WEEK(date, mode)` modes 0–7 using the MySQL `calc_week` model | `bae676ef` |
| 7 | DECIMAL | Exact relational comparison on scaled integers rather than lossy `f64`, including values above 2^53 | `3ae515ae` |
| 8 | Named windows | Supports legal additive `OVER (w ORDER BY … ROWS …)` extensions and rejects clause redefinition | `0d797b4d` |

\* The metadata SQL changes share `3fab0bbd` with a concurrent dashboard
change. The SQL files themselves remain independently identifiable in that
commit. That commit also contains a wire-visible MySQL compatibility fix:
`DESCRIBE` now returns a real SQL `NULL` in its `Default` column when no default
exists, rather than a text placeholder. The mixed commit subject does not
describe either SQL change; correcting it requires a deliberate history split.

## Essential follow-up completed

The later parity pass finished the basic read-only work that the original
eight-task review left open. Each row is independently committed and gated.

| Area | Delivered behavior | Commit(s) |
|---|---|---|
| `SHOW` and views | `SHOW INDEX`/`KEYS`, explicit empty replicated-view surface, richer discovery metadata | `d5bb27e7` |
| Metadata queries | aliases, narrow INNER/LEFT/CROSS joins, grouped client aggregates, replayed discovery corpus | `1b5649fa`, `2c7e963a` |
| Conditional coercion | exact `NULLIF` comparison and verified lazy/coerced conditional expressions | `fd00af41` |
| Cast/conversion | interval-shaped `TIME`, validating JSON, real YEAR type, explicit unsupported charset rejection | `aed1bf62`, `900ff891`, `35967239`, `c825b19a` |
| Aggregates | session `group_concat_max_len`, warning 1260, VARCHAR/TEXT threshold, multi-expression DISTINCT | `f1faa73e`, `9bddcde5`, `84a81ba9` |
| DECIMAL | exact set/group/extreme/modulo paths and MySQL base-1e9 division intermediates | `6cd6b8ad`, `34d8c874` |
| RANGE frames | numeric, exact fractional DECIMAL, and simple temporal interval offsets by ordering-key value | `43f848cd`, `e9ac2ce0`, `5faf6e37` |
| Named windows | chained earlier definitions with forward/cycle and illegal inheritance rejection | `b8ebfa2c` |
| Temporal parsing | unsupported `STR_TO_DATE` directives reject instead of taking chrono's different meaning | `177bf7b5` |
| Wire metadata | type-derived length, session-aware utf8mb3/utf8mb4/binary charset, DECIMAL scale and temporal FSP through text/prepared results | `34379b7c` plus the final review fix |

## Current JSON function support

The supported JSON names and operators are:

- construction and aggregation: `JSON_OBJECT`, `JSON_ARRAY`,
  `JSON_ARRAYAGG`, `JSON_OBJECTAGG`;
- reading: `JSON_EXTRACT` with one or several paths, `JSON_VALUE` including
  `RETURNING`, `JSON_UNQUOTE`, `->`, and `->>`;
- inspection: `JSON_VALID`, `JSON_TYPE`, `JSON_LENGTH`, `JSON_KEYS`;
- search: `JSON_CONTAINS`, `JSON_CONTAINS_PATH`, and `JSON_SEARCH`.

Important remaining JSON discrepancies:

- JSON-vs-VARCHAR identity now survives compiled scalar arguments and aggregate
  inputs. Constructors embed JSON documents, quote equal-looking text, retain
  SQL NULL versus JSON null, and expose JSON result metadata.
- Direct JSON comparison, ordering, grouping, DISTINCT/set duplicate handling,
  joins, IN/BETWEEN, window keys, and MIN/MAX now reject explicitly until a
  MySQL-compatible binary-JSON precedence/hash model exists.
- SQL DECIMAL and temporal values still cannot retain MySQL's typed JSON scalar
  categories through the text-backed physical carrier.
- The modification family (`JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`,
  `JSON_REMOVE`, `JSON_MERGE*`) and table-valued `JSON_TABLE` remain out of
  scope.
- Literal paths are not yet compiled once into the physical expression, and
  the complete MySQL wildcard/recursive-descent path surface is not advertised.

## Current regular-expression support

The supported regex surface is:

- `REGEXP_LIKE(text, pattern[, match_type])`;
- basic two-argument `REGEXP_SUBSTR` and `REGEXP_INSTR`;
- basic three-argument `REGEXP_REPLACE`;
- `REGEXP` and `RLIKE`, including their `NOT` forms.

`REGEXP_LIKE` accepts `c`, `i`, `m`, `n`, and `u`; the rightmost conflicting
`c`/`i` flag wins. The implementation uses Rust's linear-time Unicode regex
engine and translates selected POSIX classes to Unicode-aware equivalents.
The review pass made `u` meaningful: without it, CR, CRLF, NEL, line separator,
and paragraph separator are normalized to ICU-style line boundaries; with
`u`, only LF receives newline treatment. Compiled programs are now held by
query-owned, reference-counted entries for literal patterns and are reused for
every row and batch. Dynamic patterns are uncached and drop after the row.
Binary operands reject, patterns cap at 64 KiB, compiled programs cap at 1 MiB,
and both retained literal programs and replacement output are charged to the
query memory limit.

Remaining regex discrepancies:

- `REGEXP_INSTR`, `REGEXP_SUBSTR`, and `REGEXP_REPLACE` do not yet accept the
  MySQL position, occurrence, return-option, and match-type overloads.
- ICU-only syntax, capture replacement details, zero-width advancement, and
  collation-sensitive matching need a broader compatibility matrix.
- The Rust engine is an intentionally narrower ICU subset; patterns accepted by
  both engines still need continuing differential coverage for semantic edges.

## Discrepancies in the other function families

| Area | Current state | Material remaining discrepancy |
|---|---|---|
| Aggregates | Everyday aggregates, variance/stddev aliases, bit folds, `ANY_VALUE`, JSON aggregates, multi-expression DISTINCT, and configurable `GROUP_CONCAT` are implemented | Collation coercibility beyond Pintail's declared collation surface remains |
| Numeric | Exact and approximate paths are distinguished; common arithmetic and formatting helpers are broad | Result metadata/overflow rules still need exhaustive oracle coverage for every accepted overload |
| DECIMAL | Arithmetic chains, aggregation, comparison, grouping, DISTINCT, IN, MIN/MAX, and hash/join keys are exact; wire scale metadata is preserved | Source precision above 38 digits is deliberately retained as text and declines exact-expression semantics |
| Date/time | Statement time, session timezone, `DATE_FORMAT`, simple intervals, `EXTRACT`, and all `WEEK` modes are implemented | Compound intervals are blocked by the upstream parser; invalid/zero-date and SQL-mode behavior is intentionally narrower |
| Text | Default comparison is Unicode lowercase; optional accent folding exists | Charset/collation metadata, coercibility, `COLLATE`, pad-space rules, and a verified MySQL weight model remain issue #10 |
| Conditional/conversion | `IF`, `IFNULL`, `NULLIF`, `COALESCE`, and `CASE` coercion is verified; `CAST` covers JSON/YEAR/TIME/temporal/DECIMAL; unsupported charset transcoding rejects; prepared metadata is typed | Character sets outside UTF-8 and binary require a real transcoder and stay unsupported |
| Windows | Ranking, offset/positional functions, aggregate windows, ROWS frames, numeric/temporal RANGE offsets, and chained named windows work | Very wide bounded aggregate frames recompute over their width; this is a performance limitation, not a result discrepancy |

## Final standards review

The standards-oriented review identified these implementation defects:

1. Exact DECIMAL equality had been lowered to a scalar function, which made it
   invisible to equi-join key extraction. Equality and inequality now cast
   both exact operands to a common DECIMAL scale and retain a binary comparison.
   A real DECIMAL hash-join regression proves planning and execution.
2. `OVER (w)` incorrectly inherited a frame from `w`. MySQL permits direct
   `OVER w`, but rejects parenthesized inheritance from a framed base. The
   binder now enforces that distinction.
3. `SHOW COLUMNS` labeled every selected physical key as `PRI`, even when the
   catalog selected a source unique key. Single-column unique keys now report
   `UNI`; composite unique-key members report `MUL`; related constraint/index
   metadata uses a unique index name rather than fabricating `PRIMARY`.
4. Regex `u` was accepted but did nothing. ICU-style non-Unix line endings are
   now normalized unless `u` is present, with `m` versus `mu` regressions.
5. Metadata `LIKE` matched UTF-8 bytes and ignored escapes. It now consumes
   Unicode characters and honors backslash-escaped `%` and `_`.

## Final specification/documentation review

The specification-oriented review identified documentation drift rather than
new engine defects:

1. The old review still described uncommitted JSON work and reported 133/786.
   The current measured surface is 134 callable names and 802 oracle cases.
2. It still called `MD5` missing, although `e9703ede` implemented it.
3. `docs/limitations.md` called `ANY_VALUE`, variance/stddev, and bit folds
   missing even though `parity.md` advertised them as supported.
4. The window limitations repeated the RANGE-offset explanation.
5. `parity.md` still listed already-delivered aggregate functions as ranked
   gaps.

Those contradictions are removed by this review pass. Historical counts remain
available in Git history; this file describes the present branch only.

## GitHub issue assessment and the next basic read-only SQL work

The issue bodies were epics rather than synchronized completion ledgers. Their
implemented read-only portions are summarized here, but #9, #13, #17, and #25
remain open until their differential and policy acceptance gates are complete.
The larger implementation work still includes:

1. **Differential gates (#9, #13, #17):** run the eight newer oracle cases
   against MySQL 8.4, extend overload coverage, and add the remaining wire/E2E
   cases before closing their epics.
2. **Wire type provenance (#17):** retain declared variable-width lengths and
   aggregate provenance through wrappers/derived projections; the current 1024
   fallback and direct-only `GROUP_CONCAT` marker are explicit limitations.
3. **Temporal policy (#13):** make invalid and zero-date behavior depend on an
   explicitly supported SQL-mode policy rather than merely storing `sql_mode`.
4. **Dependent correlated-subquery fallback (#11):** execute correct shapes
   that cannot be decorrelated and raise MySQL's multi-row scalar error.
5. **Nested outer-join groups (#16):** introduce a bound join tree capable of
   preserving parenthesized outer joins.
6. **Collation fundamentals (#10):** coercibility, trailing-space rules,
   `COLLATE`, and one verified MySQL weight model.
7. **Demand-led JSON/regex extensions:** wildcard/recursive JSON paths and
   longer regex positional overloads only when captured read workloads need
   them; mutation functions and `JSON_TABLE` remain out of scope.
8. **Compound temporal intervals:** wait for sqlparser to accept MySQL's
   compound qualifier syntax, or take a separately reviewed parser fork.

Production BI-query capture (#24) should reorder these larger projects when
real workload evidence is available.

## Verification expectations

Every future read-only SQL increment should continue the same gate:

- binder acceptance and explicit negative tests;
- executor tests across constant, batch, and storage-backed paths as relevant;
- MySQL 8.4 differential-oracle cases for every advertised overload;
- wire metadata checks when result types or metadata commands change;
- memory accounting for retained/cached state; and
- `parity.md`, `docs/limitations.md`, and issue checklists updated together.

## Conclusion

The read-only parity pass is landed in bounded commits. The current state has
134 callable names, an 802-case oracle inventory, richer typed wire metadata,
exact implemented DECIMAL paths, and explicit rejection for deliberate
boundaries. Differential execution and remaining policy work are still gates
for the open epics.
