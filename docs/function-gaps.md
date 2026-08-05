# Ranked function gaps

Output of `scripts/function-surface.ts` against two corpora, plus a manual
pass over the semantic gaps a name-level diff structurally cannot see.

## What the harness measured

The supported surface is read out of `crates/pintail-sql/src/binder.rs`, which
is the only place that decides whether a name resolves. **110 names are
callable**, each with the arity guards the binder enforces.

Against the repo's own analytical corpus (10 benchmark queries + the e2e set):
**nothing missing**. That is not a result — those queries were authored against
this engine, so they can only ever come back clean. It does confirm the
harness runs, and it says the benchmark workload is not a source of demand
evidence.

Against the BI corpus: the list below.

## Provenance, stated plainly

The BI corpus is **reconstructed, not captured**. It encodes how Metabase,
Superset, Looker and Tableau are documented to compile a time series, a
cohort, a filtered dimension and a symmetric aggregate. That is enough to say
*which* functions those tools need. It is **not** enough to claim a frequency
ranking — the counts below reflect how many example queries I wrote, not how
often anything occurs in production. Feed a real captured log to the same
harness and replace this ranking with its output rather than merging the two.

## Missing names, grouped by the workload that needs them

| Function | Needed by | Note |
| --- | --- | --- |
| `LAG`, `LEAD` | Superset, Metabase trends | period-over-period is a default chart type |
| `FIRST_VALUE`, `LAST_VALUE`, `NTILE` | Superset, Looker | cohort first/last touch, quartile buckets |
| `STDDEV`, `STDDEV_POP`, `STDDEV_SAMP` | Tableau | statistical measures are built-in aggregations |
| `VARIANCE`, `VAR_POP`, `VAR_SAMP` | Tableau | same |
| `ANY_VALUE` | Looker, Metabase | emitted to satisfy `ONLY_FULL_GROUP_BY` |
| `BIT_AND`, `BIT_OR`, `BIT_XOR` | Tableau | flag rollups |
| `SUBSTRING_INDEX` | all four | URL/UTM splitting, the most common string cleanup |
| `MD5` + `CONV` | Looker | **see below — these travel together** |
| `JSON_CONTAINS`, `JSON_LENGTH`, `JSON_KEYS`, `JSON_TYPE`, `JSON_VALID` | all four | filtering on a JSON dimension |
| `MAKETIME` | Superset | time-of-day reconstruction |

### `MD5` + `CONV` is one unit, not two tail items

Looker's documented technique for a correct `SUM` across a fanned-out join —
its *symmetric aggregate* — hashes the primary key with `MD5`, takes a
substring, converts it from base 16 with `CONV`, and reassembles the value
through `CAST(... AS DECIMAL(38,0))`. This is not an occasional function; it
is how Looker compiles essentially every measure over a join. `CONV` was not
in the plan at all, and `MD5` was filed in the low-priority tail. Together
with wide-precision `DECIMAL` casts they form a single high-value unit.

`CONVERT` appeared here in the first run and was wrong: it binds through
`Expr::Convert` (`binder.rs:1897`) in both its CAST-equivalent and
`USING charset` forms. The harness parses only call syntax, so a function
spelled as a dedicated AST node reads as missing — `CONVERT` is now in the
harness's syntax-form list alongside `CAST` and `EXTRACT`.

## What a name-level diff cannot see

Three of the gaps are inside functions that are already callable, so the
harness reports them as supported. These need binding the corpus, not
regexing it.

- **Compound intervals.** `DATE_ADD` resolves; `INTERVAL '1-2' YEAR_MONTH`
  and `INTERVAL '3 4:00:00' DAY_SECOND` do not. Superset's time grains use
  them.
- **`EXTRACT` compound units.** `EXTRACT` resolves; `EXTRACT(YEAR_MONTH FROM …)`
  needs the same unit table.
- **`DATE_FORMAT` directives.** `DATE_FORMAT` resolves. What it does with an
  unmapped directive is the subject of the next section, and it is worse than
  a gap.

## Correctness defect: `DATE_FORMAT` silently returns wrong output

`mysql_date_format` (`crates/pintail-exec/src/expression.rs:1979`) rewrites
MySQL directives into a chrono format string. It maps `%c %e %M %k %l %i %s
%f %%` explicitly. **Every other directive falls through unchanged** into
chrono, where the same letter often means something else:

Measured against chrono 0.4.45 (the locked version) formatting
`2024-02-29 12:34:56`, a Thursday:

| Directive | MySQL gives | Pintail gives | |
| --- | --- | --- | --- |
| `%W` | `Thursday` | `09` | wrong |
| `%D` | `29th` | `02/29/24` | wrong |
| `%v` | `09` | `29-Feb-2024` | wrong |
| `%u` | `09` | `4` | wrong |
| `%X` | `2024` | `12:34:56` | wrong |
| `%x` | `2024` | `02/29/24` | wrong |
| `%U`, `%V` | `08`, `09` | `08`, `09` | agrees here, conventions differ in general |
| `%a %b %j %p %r %T` | — | — | correct by coincidence |

Every one of these returns a plausible-looking string. None raises an error.
`%v` is the sharpest example: asked for a week number, it returns a full
formatted date.

This breaks the project's stated policy that unsupported semantics fail
explicitly rather than returning a plausible but incompatible result.
`docs/limitations.md:110` records the directive inventory as "not
implemented", which reads as *absent* — it is actually *silently wrong*, and
`%W` is common enough in dashboard date labels that this is likely to be hit.

The oracle does not catch it: the only two `DATE_FORMAT` cases
(`tests/sqllogic/tests/mysql_oracle.rs:471,759`) use `%Y %m %d %H %i %s %c %e
%k` — precisely the directives that are mapped or coincide. The gap in
coverage and the gap in implementation have the same shape, which is why
neither was noticed.

## Effect on the plan

- Task #134 is no longer "add directives". It is "stop returning wrong
  answers, then add the rest" — and it should move ahead of the additive
  work, because a wrong answer is worse than a missing function.
- Task #138 gains `CONV` and promotes `MD5` out of the tail, as one unit with
  wide-precision `DECIMAL` casts in #136.
- Everything else in #131–#137 is confirmed by the corpus. Nothing in the
  plan turned out to be unwanted.
