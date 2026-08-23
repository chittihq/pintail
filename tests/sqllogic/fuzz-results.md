# High-volume MySQL differential corpus — 2026-08-24

Every run below used a fresh, uniquely named real MySQL container and Pintail's
in-process parse → bind → plan → execute path over the identical typed fixture.
Generated SQL was totally ordered, MySQL-rejected statements were forbidden,
and no case was skipped. The SQL corpus is reproducible from the seeds and
command documented in `docs/verification.md`; the multi-megabyte expansion is
not committed.

| Oracle | Generated cases | Unique SQL within run | Families | Result |
|---|---:|---:|---:|---|
| MySQL 8.4 | 50,000 | 32,712 | 10 typed/core families | byte-exact PASS |
| MySQL 8.4 | 25,000 | 15,054 | 16 widened families | byte-exact PASS |
| MySQL 8.0 | 25,000 | 15,054 | 16 widened families | byte-exact PASS |

The fixed 1,081-case oracle also passed separately on MySQL 8.4 and 8.0 after
the generated sweep. Aggregate generated executions: **100,000**, with zero
invalid or skipped SQL statements. Unique counts are per run and are not added
together because the deterministic seed sets overlap.

## Findings

The widened DECIMAL family found one real Pintail incompatibility:

```sql
SELECT ROUND(CAST(50.00 AS DECIMAL(4,2)) + 0.00, -2);
```

MySQL returns `100`; Pintail returned floating-point `0`. SQL represents `-2`
as unary minus over literal `2`, but the binder recognized only a bare positive
literal as a fixed digit count. Exact DECIMAL therefore fell through to the
approximate nearest-even path. Commit `dd440ba` recognizes signed constant
expressions, retains exact half-away-from-zero rounding, and adds positive and
negative tie regressions plus exact TRUNCATE coverage.

The MySQL 8.0 fixed-corpus run also exposed an invalid oracle assumption:
case-insensitive `INTERSECT` chose `alpha` as its representative where 8.4 and
Pintail chose `Alpha`. Both are collated-equal and MySQL does not define the
representative spelling. Commit `981e0d1` folds the projected key with `UPPER`,
so the case now tests set membership rather than version-specific scan order.

## Boundary

This evidence covers the grammar's supported read surface and the fixed oracle
fixture. It does not claim exhaustive MySQL compatibility, production data
distribution, MySQL 5.7 window support, wire metadata parity, or CDC behavior;
those belong to their separate gates.
