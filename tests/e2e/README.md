# End-to-end differential gate

Boots the whole product the way production runs it and proves MySQL and
Pintail stay identical while the source is abused:

1. A real MySQL 8.4 source starts in Docker (ROW binlog, FULL image,
   GTID). The real `pintail` release binary starts locally, the database is
   registered through the HTTP API in CDC mode, and the snapshot runs.
2. Workload phases mutate MySQL: transactional CRUD with rollbacks, type
   edge cases (unicode, NULLs, decimal extremes, unsigned boundaries, JSON,
   binary, enum/set), live DDL (ADD/DROP COLUMN, CREATE TABLE mid-stream,
   TRUNCATE), 400 operations of seeded random churn, and a SIGKILL restart
   of the pintail process with writes while it is down.
3. After every phase the gate proves convergence — every MySQL base table
   reads back identically from Pintail, retried until the CDC supervisor
   catches up — and runs the differential corpus in `queries.ts` (joins,
   UNION ALL, aggregates, subqueries, CTEs, date/string functions) on both
   engines, comparing normalized results.

Operations Pintail documents as gaps (table RENAME quarantine, in-place
type changes) run in a final phase whose divergences report as WARN, not
FAIL — regressions in documented behavior stay visible, improvements flip
them to PASS.

Run the gate:

```sh
cd tests/e2e
bun install
bun run e2e
```

`E2E_PHASES=crud,ddl` runs a subset while iterating;
`PINTAIL_E2E_BINARY=/path/to/pintail` skips the release build. Results land
in `results.md` / `results.json`; the process exits non-zero on any FAIL.

The crate-level matrices under `crates/*/tests/` remain the deep gates for
each subsystem (snapshot key matrix, CDC compatibility/DDL/purge, polling,
wire compatibility, sqllogic oracle). This suite is the composition check:
the whole loop, one process each, no shortcuts.
