# Joins with no equality key: what the sorted search reaches

Measured 2026-11-14 on an 8-vCPU box (AMD Ryzen 9 9950X, 16 GB), release
build, in process:
`crates/pintail-exec/tests/integration/subquery_join_survey.rs`, with
`SURVEY_LEFT=3000 SURVEY_RIGHT=3000 SURVEY_KEYS=50 SURVEY_ONLY=t3,t2_nokey`.
The base and the changed build ran interleaved, three rounds each, their
order alternating; the table gives the median and the fastest of the
three. The box moved between two speeds during the runs (the same build
read up to 2.7 times slower in a later round), so only differences far
past that are claims.

Both sides are one invented table of 3,000 rows, joined to itself: 9
million pairs, of which each condition keeps about three per left row.
"Pairs tested" is counted by the engine (`range_join_pairs_tested`), not
inferred from the time.

## The condition's inequalities

A join whose ON has no hashable equality sorts its right input by one
inequality's right side and searches it per left row. Before, that held
for integers, doubles, and dates or same-precision date-times when both
sides were plain columns; every other condition tested all 9 million
pairs.

| condition (`a` left, `b` right) | before ms (median, min) | after ms (median, min) | pairs tested before | after |
|---|---:|---:|---:|---:|
| `b.x BETWEEN a.x AND a.x + 2` (integers, unchanged) | 8.1, 5.8 | 6.4, 6.3 | 8,997 | 8,997 |
| `b.amt BETWEEN a.amt AND a.amt + 2` (DECIMAL) | 8,436, 7,512 | 12.4, 11.1 | 9,000,000 | 8,997 |
| `b.t0 >= a.t0 AND b.t0 < a.t0 + INTERVAL 3 MINUTE` (DATETIME, a computed bound) | 8,032, 6,818 | 24.4, 20.1 | 9,000,000 | 8,997 |
| `b.t3 >= a.t0 AND b.t3 < a.t0 + INTERVAL 3 MINUTE` (DATETIME(3) against DATETIME(0)) | 46,338, 41,448 | 56.5, 47.2 | 9,000,000 | 8,997 |
| `b.s >= a.s AND b.s <= a.s2` (text under the default collation) | 7,205, 6,193 | 17.0, 13.9 | 9,000,000 | 8,997 |

The pairs-tested figures before the change, here and below, are inferred
from the plan - the whole product, or the half one bound leaves; that
build did not count them.

## A second bounded right column

One sorted side leaves a prefix or a suffix when the condition bounds two
different right-side expressions: `a.x BETWEEN b.lo AND b.hi` sorted by
`b.lo` reaches half the right rows on average. A tree over the sorted
rows keeps each stretch's greatest `b.hi`, so a stretch no row of which
reaches the left value is skipped whole.

| condition | before ms (median, min) | after ms (median, min) | pairs tested before | after |
|---|---:|---:|---:|---:|
| `a.x BETWEEN b.lo AND b.hi` | 2,270, 2,034 | 8.5, 6.6 | about 4,500,000 | 8,998 |
| the same in a LEFT join | 2,271, 2,105 | 8.1, 7.8 | about 4,500,000 | 8,998 |
| the same under `COUNT(*)` alone | 1,973, 1,761 | 7.6, 7.1 | about 4,500,000 | 8,998 |
| `b.amt < a.amt AND b.x > a.x - 3` (a DECIMAL and an integer bound) | 3,512, 3,440 | 8.7, 8.4 | about 4,500,000 | 5,997 |
| `b.t3 > a.t0 AND b.x < a.x + 3` | 19,606, 18,356 | 31.3, 27.6 | about 4,500,000 | 8,997 |

## What it does not reach

- A condition with no `<`, `<=`, `>`, `>=` or `BETWEEN` between the two
  inputs (`<>`, an `OR`), and comparisons the engine does not order in
  one domain: a number with text, `TIME`, JSON, a DECIMAL with a DOUBLE.
- A value outside its domain gives the search up rather than risk an
  order the comparison does not have: a date-time whose text is not its
  declared width, a DECIMAL past what 38 digits hold at the comparison's
  scale, a number where text was declared. One such value on the right
  side tests every pair; one on the left tests that row's pairs.
- A right input that spills, or one whose sorted run the memory ceiling
  has no room for.
- More than one further right-side expression: the second inequality
  narrows, a third is left to the condition.
