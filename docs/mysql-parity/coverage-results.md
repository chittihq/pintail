# Expanded parity coverage results

The expanded differential coverage is implemented and gating. It does not
claim full MySQL compatibility: case counts measure the corpus, not the
feature surface.

The fixed corpus has 1,895 cases: typed result comparison, a boundary
fixture across integer, decimal, float, string, binary and temporal limits,
seven reviewed candidate queries and 15 seed-minimized regressions. All
data and SQL are invented test fixtures. Each run writes
`validate-out/oracle-outcomes.json` with the MySQL image digest, fixture
hash, session settings and code commit.

| Run | Matching | Known failures | Other |
|---|---:|---:|---|
| Fixed corpus, MySQL 8.4 | 1,895 | 0 | none |
| Fixed corpus, MySQL 8.0 | 1,889 | 5 | 1 version difference, below |
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

`SELECT note FROM events WHERE id <= 6 INTERSECT ALL SELECT note FROM
events WHERE id >= 3` intersects values that are equal only under a
case-insensitive collation ('Alpha' and 'alpha'). MySQL 8.4 returns
'Alpha' for the pair and 8.0 returns 'alpha'; Pintail returns what 8.4
returns. Which member of an equal pair comes back is implementation-defined,
so this is recorded rather than put on the ledger.

## Not re-run for this result

`bun run tests/e2e/parity-replay.ts` (sessions, wire protocol, snapshot,
CDC, DDL and restart) is an explicit command outside the validation
profiles and was not re-run here. The candidate validator is described in
[generated-cases.md](generated-cases.md).
