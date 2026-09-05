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
   catches up — and runs the differential corpus in `queries.ts` (147 unique
   shapes: joins up to five tables, set ops, aggregates, subqueries, CTEs,
   windows, JSON, temporal grains, regex, SET/geometry byte contracts, and
   the 21 BI-tool compilation shapes) on both engines, comparing normalized
   results. The banked headline counts checks across phases — the corpus
   replays after every settled phase — not independent behaviors.

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

For production BI dogfooding, use `bi-dogfood.ts` as documented in
`../corpus/bi-captured/README.md`. Exact captures and raw replay reports stay
local; only a manually reviewed, literal-redacted report may be shared.

## Recovery suite

`bun run test:recovery` builds a separate debug binary with the `failpoints`
feature and runs isolated CDC, polling, mode handoff, interrupted snapshot,
schema drift, source outage and operator repair scenarios. Each scenario
compares every value, keyless duplicate multiplicity and column metadata
against MySQL, checks health and dead letters, applies later writes and a
rollback, and repeats the comparison after another restart.

For one scenario: `bun run recovery/run.ts --only mode-handoff-abort`.
`--only 'cdc-*,poll-*'` selects a union; `--list` lists matching contracts.
Subset results go to `results-recovery-partial.md` and cannot overwrite the
full `results-recovery.md` ledger. Process logs, events and metadata captured
while Pintail is down remain private under `validate-out/recovery/`. The ledger
records checkout HEAD, working-tree cleanliness, binary SHA-256 and toolchains.
A full run builds from the checkout; `PINTAIL_RECOVERY_BINARY` is accepted
only with `--only` for development runs.

The binary accepts `PINTAIL_FAILPOINT=site[@nth][=abort|error]` only when
built with the feature. Multiple sites are comma-separated. A failpoint
must emit its stderr witness or its scenario fails. Normal builds ignore
the environment variable. Process crashes test replay of recoverable WAL;
they do not simulate loss of the OS page cache or hardware power failure.

Automatic scenarios cannot call resync, reconcile, reset, forced snapshot
or DLQ retry endpoints. Only mode scenarios may switch modes. Explicit
operator repairs live in `recovery/scenarios/operator.ts`. Outages affect
only the victim's source connection; source writes and the other database
remain live. Retries assert visible failure and catch-up, without a retry cap.

Recovery is a stable-profile gate, after both MySQL E2E legs and before the
browser stage. It shares the remote Docker host: never run it concurrently
with oracle, E2E, browser or benchmark. Only its own temporary MySQL
container, schemas and local data directories are removed at teardown.
