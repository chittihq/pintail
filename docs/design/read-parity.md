# Read compatibility work

The starting local replay contains 23,118 SELECTs: 3,880 compared,
3,444 exact, 395 row mismatches and 41 name-only mismatches. Another
4,220 SELECTs could not execute. These are separate measures; fixing an
execution error can increase the comparison denominator without immediately
increasing agreement. Replica-mode evidence remains a separate workload.

## Implemented slices

- `STR_TO_DATE` parses ordinal dates, week years, character-class skipping,
  incomplete input and fractional times directly. Plans capture zero-date
  SQL modes; dynamic formats produce DATETIME(6), while literal formats
  retain their declared date/time shape. Typed partial dates survive date,
  time and datetime casts; month names and month ends accept zero days.
  Day-only clock formats form durations, while early day numbers retain
  the zero date. Calendar arithmetic still requires a complete date.

- Result labels keep the first adjacent literal, preserve regular-expression
  spelling, remove trailing comments, and truncate at a UTF-8 boundary within
  255 bytes. `IGNORE_SPACE`, including its handshake capability, controls
  trailing identifier whitespace. Prefixed strings concatenate as values.

- `default_week_format` is captured in plans and cache keys. All eight
  modes apply to `WEEK` and `EXTRACT(WEEK ...)`; explicit modes and
  `YEARWEEK` keep their own behavior.

- Calendar-name functions capture `lc_time_names` from the connection. All
  111 locale names and numeric identifiers select their full and abbreviated
  labels; worker execution and cached answers retain the chosen locale.

- Empty-search `REPLACE` leaves the input unchanged. String `INSERT` rejects
  positions beyond the byte length and applies character positions within
  that boundary, including the multibyte append case.
- `LEFT`, `RIGHT`, `REPLACE` and string `INSERT` preserve binary operands and
  declare binary results for binary subjects. Replacement functions convert
  replacement operands to the subject's charset, rejecting invalid UTF-8. Tests exercise literals, column expressions, and direct results.
- Aggregate-local integer ORDER BY positions resolve against `GROUP_CONCAT`
  arguments, with invalid positions rejected. DISTINCT without explicit
  ordering sorts by the original arguments, preserving numeric ordering.
- Runtime TIME casts interpret short compact digits as a duration rather
  than a calendar date. Fractional, signed and range-clamped inputs have
  regression coverage, including a subsequent decimal cast.
- Unix conversions capture the session time zone in the plan, preserve
  fractional precision, reject negative epoch inputs, and enforce the epoch
  range after microsecond rounding. Invalid datetime inputs to
  `UNIX_TIMESTAMP` return zero at the declared precision.

Each regression uses invented inputs and the parse/bind/plan/execute path.
Expectations were checked against a dedicated MySQL 8.4 instance. No source
schema or upstream test file is embedded in these reproductions.

## Evidence and diagnosis

MTR artifacts now live in invocation-specific directories under
`validate-out/mtr/runs/`. Each directory retains the selected/excluded file
inventory, replay mode, source revision and dirty status, executable hash,
report, exact statement identities and diagnostic samples. The source
revision describes the checkout; the executable hash identifies the binary
actually used. An override binary is not assumed to have been built from
that checkout. Samples are bounded; comparisons still use complete rows.

A focused replay of seven failing areas confirmed that existing scalar
tests do not explain every file-level mismatch. In particular:

| Area | Remaining causes to address |
|---|---|
| Character sets | Session result encoding, localized month/day names, expression charset metadata, unsupported introducers and weight strings |
| Casts | Single-precision floating-point semantics, typed-column temporal coercion, YEAR conversion, zero/partial dates and SQL modes |
| Temporal functions | Default week mode, duration intervals, partial-date parsing, fractional truncation mode and session timestamp overrides |
| Aggregates | Binary-width-aware bitwise folds; DISTINCT over multiple concatenated arguments requires tuple identity |
| Result names | Adjacent literals and preservation of expression-label whitespace |
| String functions | Search operand collation, FIELD coercion and CONV overflow boundaries |

Several `GROUP_CONCAT` differences have equal ordering keys. Those are not
evidence of an incorrect ordering between unequal keys. Keep them visible
in the raw comparison report and classify them explicitly; do not silently
change comparison rules or remove them to improve the percentage.

## Verified outcome

The complete rc profile passed at `b0018f48`: all twelve stages, including
both MySQL versions, schema migrations, browser, compose and BI clients.
The workspace test stage passed 1,245 tests with 56 skipped. E2E stages now
install locked dependencies and generate their ORM client on a fresh tree.
Each MySQL e2e leg recorded 7,015 passes, zero failures, 29 documented-gap
warnings and 44 skips. The existing dropped-table polling limitation
reproduced: a failed table can interrupt the cycle for surviving tables.
Benchmark evidence was not regenerated by this correctness profile.

The local MySQL replay now compares 3,882 of 23,118 SELECTs, with 3,476 exact
(89.5%). All 3,444 previously banked cases remain exact; the baseline gains
32 cases. MariaDB's suite retains all 6,823 banked cases and gains 26, for
6,849 exact out of 7,946 comparisons. These baselines were banked from the
retained exact-case artifacts without replaying or removing old cases.

Remaining MySQL comparisons contain 365 row mismatches and 41 name-only
mismatches. The largest individual row-mismatch files are `ctype_ucs`
(127), `cast` (34), `func_time` (14), `ctype_utf8` (13),
`func_bitwise_ops` (13), and `date_formats` (12). File counts guide
investigation; they are not counts of distinct engine defects.

The 19,236 un-compared SELECTs comprise 10,118 tainted by preceding setup or
table state, 4,218 Pintail execution errors, 2,902 MySQL errors and 1,998
volatile queries. Extending execution support can change the comparison
denominator, so track these buckets alongside agreement.

## Next implementation boundaries

1. Preserve charset and coercibility metadata through expressions before
   adding legacy encodings. Decoding to UTF-8 alone cannot implement source
   byte lengths, HEX, or collation weights correctly. Record the expansion
   beyond the currently deferred collation matrix in the decisions log.
2. Carry binary result width through binding before implementing bitwise
   aggregates: empty and all-NULL groups still require a correctly sized
   identity. Cover grouped, global, window, merge and spill paths.
3. Treat single-precision casts as a type/formatting change, not only a
   numeric rounding operation. Comparison and nested casts must see the
   narrowed value, while result metadata and rendering preserve its type.
4. Resolve session-dependent failures with session-aware regressions.
   Supplying a fixed value to a scalar test does not validate session
   propagation through the wire server or across execution workers.

For each slice, run touched-crate unit tests and clippy on the build host,
then commit. Finish the batch with the complete rc profile, retain its
artifacts, and bank changed ledgers only after reviewing the outcome.
Neither a focused replay nor an increased agreement percentage is a release
gate.

### Unicode encoding boundary

Expressions retain their SQL encoding independently of the Unicode value
carrier. Introducers decode explicitly; byte consumers encode explicitly,
with direct byte preservation for an introduced literal. String functions,
conditional branches and aggregate outputs retain encoding identity. The
connection encoding is captured during binding and participates in the
shared-query key. UCS-2, UTF-16 (both byte orders) and UTF-32 have explicit
codecs; this does not enable wide client/result wire encodings or wide local
column declarations. Full coercibility and encoded aggregate truncation
remain separate work.

Encoding regression checks also cross constant discovery and comparison
binding: temporal literals remain recognizable behind encoding operations,
source-column collations outrank encoded literals, and a binary comparison
observes the text operand's encoded bytes. Binary-to-text conversions pad
partial wide units and return NULL for invalid encoded input; UCS-2 raw
code units remain available to byte consumers.

### Statement clocks and temporal casts

`SET timestamp` captures a fixed clock alongside the session time zone.
Planning captures the calendar year for TIME-to-YEAR casts before execution
moves to workers. Query sharing includes the fixed clock and calendar year,
so both changing a session override and crossing a real year boundary
separate answers. Compact numeric `TIME_TO_SEC` inputs share the TIME parser;
YEAR casts apply their domain range and JSON's integer conversion rules.

### Floating-point casts

FLOAT casts narrow the numeric value to single precision while retaining
that logical type through planning and result metadata. Numeric consumers
read the narrowed value; string consumers use six significant digits and
the FLOAT display width. Text-protocol result cells and binary-protocol
values have separate rendering tests, including a stored FLOAT column and
NULL. Floating casts join the decimal guard-digit path in both scalar and
vector execution so division is not rounded prematurely.
