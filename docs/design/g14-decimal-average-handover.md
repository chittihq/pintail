# G14: decimal AVG rounds its quotient twice

Implemented in `bbc456f` on 2026-09-08. The formerly ignored reproduction
is enabled, and all 15 tests in `decimal_average_exactness` pass. The full
RC profile passed (`2026-09-08T16-39-19-997Z-rc`), followed by two additional
complete e2e passes on each MySQL version. Each of the six e2e runs recorded
5,446 passes and zero failures, with the existing documented warnings and
skips. Final ledgers: [MySQL 8.4](../../tests/e2e/results.md) and
[MySQL 8.0](../../tests/e2e/results-mysql80.md).

The implementation carries declared-scale text and the scaled quotient in
a boxed value, preserving `Value`'s 32-byte layout. The rounding family and
decimal casts read internal fractional words; ordinary display and scalar
identity use the text. Spill retains the quotient, and typed repacking
falls back to the original values. Regressions also cover input scales
0/2/6, negative values, wide exact averages, nested aggregates, predicates,
grouping, and window range bounds.

The diagnosis and implementation brief below record the pre-fix state.

## The defect

`AVG` over a `DECIMAL` column can answer one unit in the last place above
`MySQL`, while `SUM(x) / COUNT(*)` over the same rows in the same statement
is exact. That asymmetry is the whole of it, and it is not an arithmetic
fault: the division rounds correctly, and it rounds twice.

The finished average is materialized at its declared scale - the column's
scale plus four - by `crates/pintail-exec/src/execution/aggregate.rs:1488`:

```rust
let average = pintail_types::div_decimal_round_half_up(units, i128::from(count))?;
Value::Utf8(pintail_types::format_decimal_scaled(average, scale))
```

`ROUND(_, 4)` then rounds that already-rounded value again. When the
intermediate is an exact half at the fourth place, half-up carries it
upward. `SUM(x) / COUNT(*)` escapes because that division is a computed
expression, and the rounding family reads a computed operand's internal
digits rather than its declared scale - see `internal_decimal_value` at
`crates/pintail-exec/src/expression/mod.rs:1081`, which returns `None` for a
`Column` on the grounds that "columns and literals already hold their
display value exactly". For a materialized average that premise is false.

## What MySQL does

Measured against MySQL 8.4, not inferred. `MySQL` renders an average at the
declared scale but carries the fraction in whole nine-digit words, and lets
`ROUND` and `CAST` read those digits.

| Input scale | Declared AVG scale | Digits held |
| ---: | ---: | ---: |
| 0 | 4 | 9 |
| 2 | 6 | 9 |
| 6 | 10 | 18 |

So the rule is `ceil(declared / 9) * 9`. `ROUND(avg, n)` caps its result
scale at the argument's declared scale - `ROUND(avg, 9)` over a scale-4
average returns four places - which the wire metadata already mirrors at
`crates/pintail-wire/src/presentation.rs:534`.

The worked case, which is the ignored test's fixture:

- 161 rows summing to `54001.34`
- exact mean `335.41204968944...`
- MySQL renders `335.412050`, holds `335.412049689`, and `ROUND(_, 4)` gives
  `335.4120`
- Pintail answers `335.4121`

## The reproduction

`crates/pintail-exec/tests/decimal_average_exactness.rs:463`,
`rounding_an_average_does_not_round_it_twice`, currently `#[ignore]`d.

Remove the `#[ignore]` as part of the fix. It fails today with
`["1", "335.4121", "335.4120"]` - the engine disagreeing with its own
`SUM / COUNT` in one statement.

This is the only reproduction outside the gate. Every earlier attempt used
invented values and found nothing, because the defect needs a quotient
sitting in the narrow band where one rounding and two disagree. That is also
why it moved between gate runs: e2e's later phases mutate `orders`, so
whether any group lands in the band changes run to run.

## What not to do

**Do not widen the stored value.** Emitting the average at the internal
scale was tried and reverted. A decimal is `Value::Utf8` text, so a value
cannot hold more digits than it renders, and the surplus reaches clients as
text the protocol tells them to trust. It renders `0.000050000` where MySQL
renders `0.000050`; three existing arms in the same file catch it.

**Do not read a green run as proof.** G14 was closed twice on 2026-09-08 on
fixes that had reproductions of their own and were not this. A fix is not a
fix for this until the ignored test passes and a run that would have failed
passes.

## The shape the fix should take

Let the average expose its exact quotient - it still holds `units` and
`count` - to the same chain the rounding family already consults, while
rendering exactly as it does today.

`Value::Enum` at `crates/pintail-types/src/value.rs:187` is the precedent
and worth reading first. It exists for the same shape of problem: a value
that displays one thing and orders by another, where storing only the
display made `ORDER BY` go alphabetical silently. It keeps its blast radius
small by reporting `DataType::Utf8`, so every site that has not learned
about it treats it as the string it displays as. A decimal average wants the
same bargain: carry the exact quotient, render the declared scale, and let
only the rounding family read the rest.

Points to settle while implementing:

- Where the exact part is read. `evaluate_decimal_chain`
  (`expression/mod.rs:986`) already handles a `Column` by building a
  `DecimalRational` from its value, so a variant carrying the quotient can
  be consumed there. `internal_decimal_value` returns early for anything
  that is not `Binary`/`Unary` and will need to admit this case.
- Rounding at the seam. `div_decimal_round_half_up`
  (`crates/pintail-types/src/canonical.rs:240`) is correct; the fix is about
  when it is applied, not how.
- Spill and memo. `AggregateValue::DecimalAverage` is serialized for spill
  and cached in the settled memo. Whatever the finished value becomes has to
  survive both.
- Every other consumer of an average: comparison, `ORDER BY`, `CAST`,
  further arithmetic, and the wire encoder. The `Value::Enum` bargain covers
  them by making the default behaviour the display string, but each wants a
  deliberate look rather than an assumption.

## Verifying

1. `cargo test -p pintail-exec --test decimal_average_exactness` with the
   `#[ignore]` removed. All ten arms must pass, and the three that pin
   rendering (`a_grouped_decimal_average_is_exact_to_its_result_scale` and
   its neighbours) are the ones that catch a value widened by mistake.
2. `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo fmt --check`.
3. The full unit suites for `pintail-exec` and `pintail-store`.
4. `bun run scripts/validate.ts --profile rc`. The e2e legs are where G14
   surfaced; a single green run is weak evidence because the defect appeared
   in roughly one run in three, so repeat the e2e legs several times before
   calling it closed.
5. Only then: close G14 in `docs/design/production-hardening-todo.md`,
   remove the entry from `docs/limitations.md`, and correct `CHANGELOG.md`.

## If it needs re-measuring

The live differential pair is what produced every number here, and it
settled in minutes what three rounds of inference from pass/fail could not.
`AGENTS.md` describes the loop. The short version: run the e2e harness
directly rather than through `scripts/validate.ts`, which hard-overrides
`PINTAIL_E2E_KEEP_MYSQL` to empty for the mysql80 stage, so the source
container is torn down and cannot be inspected:

```
cd tests/e2e && PINTAIL_E2E_KEEP_MYSQL=1 PINTAIL_E2E_MYSQL_IMAGE=mysql:8.0 bun run run.ts
```

Then query the surviving container directly for the group's real rows, and
ask MySQL what it thinks of them. Its answer is the oracle.
