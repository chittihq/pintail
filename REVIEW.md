# Read-only SQL compatibility review

## Executive summary

Pintail currently exposes **134 callable function names** and carries **794
MySQL 8.4 differential-oracle cases**. The count in `parity.md` is enforced by
a binder unit test. This is the oracle inventory, not a claim that the
Docker-backed oracle ran during this series. The eight bounded read-only SQL
tasks selected from the repository issues have all been implemented and
committed independently.

The delivered work improves metadata discovery, everyday functions, correlated
subqueries, join syntax, JSON and regex overloads, week calculation, exact
DECIMAL comparison, and named windows. A final standards/specification review
found five concrete compatibility defects in those changes. All five are fixed
in the follow-up review pass described below.

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

## Current JSON function support

The supported JSON names and operators are:

- construction and aggregation: `JSON_OBJECT`, `JSON_ARRAY`,
  `JSON_ARRAYAGG`, `JSON_OBJECTAGG`;
- reading: `JSON_EXTRACT` with one or several paths, `JSON_VALUE` including
  `RETURNING`, `JSON_UNQUOTE`, `->`, and `->>`;
- inspection: `JSON_VALID`, `JSON_TYPE`, `JSON_LENGTH`, `JSON_KEYS`;
- search: `JSON_CONTAINS`, `JSON_CONTAINS_PATH`, and `JSON_SEARCH`.

Important remaining JSON discrepancies:

- SQL values are carried through a text-backed JSON representation, so typed
  DECIMAL and temporal values cannot always retain MySQL's JSON scalar type.
- JSON comparison, grouping, DISTINCT, IN, hash-key, MIN/MAX, and collation
  behavior is not yet a complete MySQL JSON type-precedence model.
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
owned, reference-counted entries; the 256-entry cache evicts and drops programs
instead of leaking every compilation for the worker thread's lifetime.

Remaining regex discrepancies:

- `REGEXP_INSTR`, `REGEXP_SUBSTR`, and `REGEXP_REPLACE` do not yet accept the
  MySQL position, occurrence, return-option, and match-type overloads.
- ICU-only syntax, capture replacement details, binary operands, zero-width
  advancement, and collation-sensitive matching need a published compatibility
  matrix with explicit rejection tests.
- The current cache is thread-local and bounded by entry count, but compiled
  pattern memory is not integrated with the per-query memory tracker.

## Discrepancies in the other function families

| Area | Current state | Material remaining discrepancy |
|---|---|---|
| Aggregates | Everyday aggregates, variance/stddev aliases, bit folds, `ANY_VALUE`, and JSON aggregates are implemented | `group_concat_max_len` is fixed rather than session-configurable; warning behavior and complete collation semantics remain |
| Numeric | Exact and approximate paths are distinguished; common arithmetic and formatting helpers are broad | Result metadata/overflow rules still need exhaustive oracle coverage for every accepted overload |
| DECIMAL | Arithmetic, aggregation, comparison, and equality are exact on scaled integers; equality remains usable as a hash-join key | Complete grouping/DISTINCT/IN/MIN/MAX/hash semantics and source precision above 38 digits remain issue #9 work |
| Date/time | Statement time, session timezone, `DATE_FORMAT`, simple intervals, `EXTRACT`, and all `WEEK` modes are implemented | Compound intervals are blocked by the upstream parser; invalid/zero-date and SQL-mode behavior is intentionally narrower |
| Text | Default comparison is Unicode lowercase; optional accent folding exists | Charset/collation metadata, coercibility, `COLLATE`, pad-space rules, and a verified MySQL weight model remain issue #10 |
| Conditional/conversion | `IF`, `IFNULL`, `NULLIF`, `COALESCE`, `CASE`, `CAST`, `CONVERT`, and `CONV` bind | Cross-family type inference, byte-level charset conversion, and prepared-result metadata need systematic verification |
| Windows | Ranking, offset/positional functions, aggregate windows, ROWS frames, offsetless RANGE, and named windows work | RANGE offsets, GROUPS, DISTINCT windows, chained named definitions, and broad aggregate-window validation remain |

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
   The current measured surface is 134 callable names and 794 oracle cases.
2. It still called `MD5` missing, although `e9703ede` implemented it.
3. `docs/limitations.md` called `ANY_VALUE`, variance/stddev, and bit folds
   missing even though `parity.md` advertised them as supported.
4. The window limitations repeated the RANGE-offset explanation.
5. `parity.md` still listed already-delivered aggregate functions as ranked
   gaps.

Those contradictions are removed by this review pass. Historical counts remain
available in Git history; this file describes the present branch only.

## GitHub issue assessment and the next basic read-only SQL work

The open issues are epics, not synchronized checklists. For example, #8 still
shows basic regex operators, multiple JSON paths, and base JSON functions as
unchecked although those forms are implemented; #25 still shows named windows
unchecked; #17's prose still says `MD5` remains open. Future planning should
verify code and oracle coverage before treating an unchecked box as missing.

After the eight completed slices, the next basic read-only increments are:

1. **`SHOW INDEX` / `SHOW KEYS` (#14).** This is a small, high-value metadata
   command using facts already exposed through `information_schema.statistics`.
2. **Metadata aliases and simple joins (#14).** Support the narrow client
   discovery queries that join `tables`, `columns`, and constraints; keep the
   metadata interpreter deliberately smaller than the main executor.
3. **Regex positional overloads (#8).** Add `pos` and `occurrence` to
   `REGEXP_INSTR`/`SUBSTR`/`REPLACE`, then `return_option` and `match_type`, with
   Unicode character rather than byte indexing.
4. **JSON path wildcard collection (#8).** Complete wildcard/multi-match
   autowrapping and compile literal paths once per query before considering
   mutation functions.
5. **Dependent correlated-subquery fallback (#11).** Cover correct scalar and
   EXISTS shapes that cannot be decorrelated, including multi-row scalar errors.
6. **Nested outer-join groups (#16).** Replace the current left-deep bound join
   representation or add a tree so parenthesized outer joins preserve semantics.
7. **Collation fundamentals (#10).** Start with binary versus one explicitly
   supported utf8mb4 collation, trailing-space rules, and identical equality/
   hashing normalization.
8. **Window RANGE offsets (#25).** Implement typed numeric/interval bounds and
   peer semantics; do not approximate them as ROWS offsets.

The first four are the most "basic" client-compatibility work. Items 5–8 are
larger planner/type-system projects even though their SQL syntax looks small.
Compound interval qualifiers (#13) should wait for an upstream parser decision,
and production BI-query capture (#24) should be used to reorder this list when
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

All eight selected tasks are landed as separate commits. The final review
closed the regressions that could have turned supported syntax into wrong
answers or planner failures, and reconciled the compatibility documents with
the measured 134-function, 794-case surface. The remaining roadmap is still
substantial, but the next basic read-only SQL work is now separated from the
larger collation, join-tree, and dependent-execution projects.
