# Correlated-subquery repetition: what repeated outer keys cost

Measured 2026-09-05, release build, macOS/arm64, in-process
(`crates/pintail-exec/tests/dependent_subquery_ratio.rs`, `PINTAIL_RATIO_ROWS=20000`).

Outer table: 20,000 rows whose correlation key takes D distinct values. Inner
table: 8,000 rows over 1,000 keys, so every outer key matches exactly 8 inner
rows in every column - only the repetition varies. Inner executions are
counted by the engine, not inferred.

| shape | distinct keys D | repeats per key | inner executions | total ms | ms per outer row |
|---|---:|---:|---:|---:|---:|
| scalar | 1000 | 20 | 20000 | 6475 | 0.324 |
| scalar | 100 | 200 | 20000 | 6430 | 0.322 |
| scalar | 10 | 2000 | 20000 | 6277 | 0.314 |
| scalar | 1 | 20000 | 20000 | 6812 | 0.341 |
| exists | 1000 | 20 | 20000 | 3574 | 0.179 |
| exists | 100 | 200 | 20000 | 3444 | 0.172 |
| exists | 10 | 2000 | 20000 | 3489 | 0.174 |
| exists | 1 | 20000 | 20000 | 3461 | 0.173 |
| in | 1000 | 20 | 20000 | 130947 | 6.547 |
| in | 100 | 200 | 20000 | 126985 | 6.349 |
| in | 10 | 2000 | 20000 | 127459 | 6.373 |
| in | 1 | 20000 | 20000 | 127324 | 6.366 |

Shapes: scalar `(SELECT x ... WHERE i.k = o.k ORDER BY ... LIMIT 1)`,
`EXISTS (... WHERE i.k = o.k AND ... LIMIT 1)`, and `o.k IN (SELECT ... FROM
inner JOIN inner ... WHERE i.k = o.k ...)` - each written so the binder cannot
decorrelate it into a join.

## What it says

- The dependent path runs the inner query exactly once per outer row in every
  shape, and cost is flat in the repetition (within ±5%): one key repeated
  20,000 times costs the same as 1,000 distinct keys. Nothing is shared.
- Statement-local memoization keyed on the substituted outer tuple would take
  inner executions from N to D. At D = 1,000 that is 20×: scalar from ~6.5 s
  to ~0.3 s, EXISTS from ~3.5 s to ~0.2 s. Material for the shapes real data
  takes (orders per customer, events per session).
- Correlated IN is 20× the cost of the other two per row because its inner
  query holds a join, and that join is re-planned and re-executed per outer
  row. Memoization would mask that for repeated keys and leave it for
  distinct ones; the fix there is planning the inner query once and
  executing it per parameter tuple, which is a separate change.

Also found by this measurement: a forced-backtrace `eprintln!` in the
binder's `decorrelate_in` fallback fired on every correlated IN the rewrite
could not take, on the server's stderr, and was removed.
