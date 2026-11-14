# Dependent subqueries: what an execution costs, and which shapes still run one per row

Measured 2026-11-14 on an 8-vCPU box (AMD Ryzen 9 9950X, 16 GB), release
build, in process:
`crates/pintail-exec/tests/integration/subquery_join_survey.rs`. Outer
table 20,000 rows, inner table 8,000 rows, 1,000 correlation keys
(`SURVEY_LEFT=20000 SURVEY_RIGHT=8000 SURVEY_KEYS=1000 SURVEY_ONLY=t1`).
The base and the changed build ran interleaved, three rounds each, order
alternating. The box moved between two speeds during the runs - the same
build read up to 2.7 times slower in a later round - so the tables give
the median and the fastest run, and inner executions, which the engine
counts, carry the claims.

## Planning is not what a dependent execution costs

`dependent-subquery-memo.md` (2026-09-05) left one question open: each
dependent execution plans its inner query from scratch, and a correlated
`IN` over a join read twenty times the per-execution cost of a scalar or
`EXISTS` shape. `plan_share` splits one execution of each inner query -
the outer value written as a literal, as the dependent path writes it -
into planning (logical plan, optimizer, physical plan), starting the
execution and running it, over 2,000 executions
(`SURVEY_RIGHT=8000 SURVEY_ROUNDS=2000`), six runs:

| inner query | plan µs | start µs | run µs | plan share |
|---|---:|---:|---:|---:|
| `IN` member list over a two-table join | 29-63 | 22-56 | 290-1,049 | 5.4-8.5% |
| first row of an ordering (`ORDER BY .. LIMIT 1`) | 5-12 | 7-17 | 142-247 | 3.4-4.5% |
| `EXISTS .. LIMIT 1` | 5-9 | 7-14 | 47-87 | 7.8-8.7% |
| aggregate under `HAVING`, its filter pruned to one row | 3-6 | 4-7 | 2-5 | 33-35% |

Planning the inner query once and rebinding its outer values would
remove at most the first column: 3 to 9 percent of an execution that
reads its table, and a third of one that costs ten microseconds in all.
The join shape costs about twice the single-table one per execution, not
twenty times, and the difference is its second scan and its build. That
change was not made.

## A DISTINCT subquery no longer runs per row

What kept the measured `IN` on the per-row path was its `DISTINCT`: the
set-at-a-time form, which answers every outer row of a batch in one
execution, refused any DISTINCT subquery, and one whose correlation sits
in its own join's ON has no other rewrite. The same statement without
the keyword already ran no inner execution. The form now takes a
DISTINCT subquery that is read row by row - distinct over the outer row
and the value together - when the value is a whole number, a decimal, a
date or a date-time, and takes a subquery with no WHERE whose inner
join's ON holds the correlation.

`o.k IN (SELECT DISTINCT i.k FROM b i JOIN b j ON j.id = i.id AND
i.<column> = o.<column> WHERE j.x > 5)`:

| outer tuples | before ms (median, min) | after ms (median, min) | inner executions before | after |
|---|---:|---:|---:|---:|
| 1,000 distinct (20 outer rows each) | 915, 447 | 28.3, 27.9 | 1,000 | 0 |
| 20,000 distinct | 10,428, 4,609 | 44.1, 40.5 | 20,000 | 0 |

A DISTINCT text or DOUBLE value stays per-row: DISTINCT keeps one of the
spellings a collation holds equal (or one of the two zeros), and which
one is not the per-row execution's choice in the batched form.

## What still runs once per distinct outer tuple

Same sizes, 20,000 distinct outer tuples, unchanged by this work (base
and changed builds agree within the noise):

| shape | fastest ms | inner executions | why (the engine's recorded reason) |
|---|---:|---:|---|
| `(SELECT i.x FROM b i WHERE i.x + 0 = a.x ORDER BY i.x DESC, i.id LIMIT 1)` | 3,167 | 20,000 | no hash index: its select list, ORDER BY or LIMIT is not one it reads; no set form: it is a first-row lookup over one table |
| `EXISTS (SELECT 1 FROM b i WHERE i.x + 0 = a.x AND i.k >= 0 LIMIT 1)` | 4,762 | 20,000 | no hash index: no equality keys its table by the outer row; no set form: its LIMIT is not the first row of an ORDER BY |
| first row of an ordering whose select list holds another subquery | 3,518 | 20,000 | no hash index: it joins (the nested subquery became a join); no set form: it is a first-row lookup over one table |
| `(SELECT SUM(i.x) FROM b i WHERE i.x = a.x HAVING COUNT(*) > 0)` | 761 | 20,000 | no hash index: it aggregates or has a window function; no set form: it has GROUP BY or HAVING |

A scalar subquery nested in another that is a plain keyed lookup
(`(SELECT (SELECT MAX(j.x) FROM b j WHERE j.k = i.k) FROM b i WHERE i.id =
a.id)`) ran no inner execution: 22 ms for 20,000 outer rows.
