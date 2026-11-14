# Expanded parity coverage results

The expanded differential coverage is implemented and gating. It does not
claim full MySQL compatibility: case counts measure the corpus, not the
feature surface.

The fixed corpus has 4,313 cases: typed result comparison, a boundary
fixture across integer, decimal, float, string, binary and temporal limits,
seven reviewed candidate queries and 15 seed-minimized regressions. All
data and SQL are invented test fixtures. Each run writes
`validate-out/oracle-outcomes.json` with the MySQL image digest, fixture
hash, session settings and code commit.

| Run | Matching | Known failures | Other |
|---|---:|---:|---|
| Fixed corpus, MySQL 8.4 | 4,313 | 0 | none |
| Fixed corpus, MySQL 8.0 | 4,312 | 0 | 1 version difference, below |
| Seeded sweep, MySQL 8.4 | 400 | 0 | 383 unique queries, default seed |
| Storage layouts and spill | all | 0 warnings | memtable, persisted, mixed, compacted, reopened |

The oracle stage of `scripts/validate.ts` runs the fixed corpus, the seeded
sweep and the storage-layout replay against MySQL 8.4.

## Known failures

None. The reviewed ledger
(`tests/sqllogic/tests/support/oracle_known_failures.json`) is empty: the
five cases it held now match MySQL 8.4. Three decimal results that need
39 to 43 significant digits compute exactly up to 65 digits, an integer
`CASE` branch keeps its own scale (`0` where the result type is
DECIMAL(20,3)), and `GROUP BY` under `utf8mb4_unicode_ci` keeps a trailing
space and a trailing no-break space in separate groups. The ledger still
fails a run the moment an entry is added and then starts to match.

## MySQL 8.0 difference

`SELECT id FROM users WHERE id IN (VALUES ROW(1), ROW(3), ROW(NULL))`
selects the rows numbered 1 and 3 on MySQL 8.4 and on Pintail. MySQL 8.0
returns every row of the table for it. Pintail follows 8.4, so this is
recorded rather than put on the ledger.

## Not re-run for this result

The seeded sweep was not re-run for this result. The candidate validator
is described in [generated-cases.md](generated-cases.md).
