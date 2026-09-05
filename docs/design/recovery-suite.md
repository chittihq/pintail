# Recovery suite: failure → detection → recovery → continued sync

Status: plan. Implements the extension proposed on 2026-09-05. Every
scenario ends in an exact value-and-multiplicity comparison against MySQL;
a return to `streaming` or `polling` state is never the pass condition.

## 0. Principles (apply to every scenario)

1. **The kill lands at a named point, and the harness proves it.** A
   SIGKILL "somewhere in the stream" is what `tests/e2e/run.ts` already
   does. This suite aborts at failpoints inside the binary and records
   `interrupts at <site>` as its own PASS/FAIL check. A failpoint that
   never fires fails the scenario; it never passes by accident.
2. **Automatic scenarios call no operator endpoint.** Forbidden inside an
   automatic scenario: `.../resync`, `.../tables/<t>/resync`,
   `.../reconcile`, `.../reset`, `.../snapshot` with `force`, `/api/dlq/*`
   retry, and `.../mode` (except the mode-transition area, where the mode
   switch IS the operation under test). The harness enforces this with two
   wrappers, `api()` and `operatorApi()`, and automatic scenarios receive
   only the first.
3. **Compare values and multiplicities, never counts.** The comparator
   is `tableDiff` from the e2e gate (ordered by key, multiset for
   keyless) plus, for keyless tables, a `GROUP BY <all columns>, COUNT(*)`
   diff, plus the `information_schema.columns` diff. Also asserted: the
   database state, every table's state, an empty DLQ for the scenario's
   database, and no `needs_resync` flag, unless the scenario names one.
4. **Every scenario follows the eight steps** from the proposal: seed and
   replicate; start the churn writer (commits and rollbacks); wait for the
   failpoint to fire; observe the expected error/quarantine/recovery
   event; restore the dependency and let the supervisor recover; stop
   writes and compare exactly; write again (insert, update, delete on
   every table) and compare; restart once more and compare. Steps 6 to 8
   are one shared function, `proveConverged()`, so no scenario can skip
   the second restart.
5. **Each scenario is isolated.** Fresh MySQL schema (`rec_<slug>_<nonce>`),
   fresh pintail data dir, fresh pintail process with that scenario's
   failpoint env. A broken database from one scenario cannot leak into the
   next. One MySQL container per run, named
   `pintail-recovery-mysql-<pid>-<nonce>`, removed at teardown; only that
   container is ever touched.
6. **Tests cite the promise they enforce.** Each scenario's doc comment
   names the line in `docs/limitations.md`, `GOAL.md`, or the crate
   docs it holds the code to. Where no promise exists (see §6), the
   scenario waits on a decision rather than freezing today's behaviour.
7. **Waits are on state, never on time.** Poll `/api/databases/<id>/status`
   and the events stream with a deadline; `Bun.sleep` only as the poll
   interval. `PINTAIL_SUPERVISOR_INTERVAL_MS` is set low, as the gate does.

## 1. Slice 1: failpoints (prerequisite for everything else)

New crate `crates/pintail-failpoint`, ~80 lines, no dependencies.

```rust
/// Fires the named site. Inert unless the `failpoints` feature is on AND
/// `PINTAIL_FAILPOINT` names the site.
pub fn hit(site: &'static str) -> Result<(), std::io::Error>;
```

- `PINTAIL_FAILPOINT` = comma-separated `site[@nth][=action]`.
  `nth` defaults to 1 (fire on the first hit); `action` is `abort`
  (default) or `error`.
- `abort` prints `failpoint <site> hit <n>: aborting` to stderr, then
  `std::process::abort()`. No destructors, no flush: the honest kill.
- `error` prints `failpoint <site> hit <n>: error` and returns
  `io::Error::new(ErrorKind::Other, "failpoint <site>")` exactly once
  (the nth hit), then goes inert. Callers propagate through their
  existing `Io` variants.
- Hit counters are per site, `AtomicU32` in a `OnceLock` table parsed on
  first call. With the feature off, `hit` is `#[inline] Ok(())` and the
  crate has no env access.
- Feature `failpoints` on the `pintail` binary crate, forwarded to
  `pintail-store`, `pintail-cdc`, `pintail-poll`, `pintail-snapshot`,
  `pintail-meta`, `pintail-api`. Release builds and the compose image
  never enable it. Clippy already runs `--all-features`, so the sites are
  lint-checked in the gate.
- Unit tests in the crate: parse table, nth semantics, error-once
  semantics, unknown site inert. Aborting is not unit-tested; the harness
  proves it.

### Sites (name, file, exact placement)

| Site | Where | Between |
|---|---|---|
| `store.wal.before_sync` | `pintail-store/src/wal.rs` `sync()` | append done, fsync not yet |
| `store.wal.append` | `wal.rs` `append()` | replaces the test-only `fail_append_after_bytes` path for error injection (keep the field's torn-prefix test, route it through the failpoint) |
| `cdc.after_ingest` | `pintail-cdc/src/lib.rs` `commit_pending` | after the `ingest_cdc` loop, before the `checkpoint()` loop |
| `cdc.after_first_table_sync` | `commit_pending` | inside the `checkpoint()` loop after index 0, only when `touched.len() > 1` |
| `cdc.before_checkpoint_commit` | `commit_pending` | after all `checkpoint()` calls, before `commit_cdc_checkpoint` |
| `cdc.after_checkpoint_commit` | `commit_pending` | after `commit_cdc_checkpoint`, before `*pending = default` |
| `cdc.resnapshot.after_targets` | `ResnapshotContext::recover` | after `resnapshot_targets`, before the checkpoint read |
| `poll.after_ingest` | `pintail-poll/src/lib.rs` `poll_table` | before `target.store.checkpoint()` |
| `poll.before_state_commit` | `poll_table` | after `checkpoint()`, before `commit_poll_state*` |
| `poll.append.after_reset` | `sync_append_table` | between `reset_for_resnapshot` and `ingest_scan` |
| `poll.checksum.before_chunk_commit` | `poll_table`, KeyedChecksum branch | before `commit_poll_state_with_chunks` |
| `snapshot.chunk.after_ingest` | `pintail-snapshot/src/lib.rs` `snapshot_table` | between `bulk_ingest_snapshot` and `complete_snapshot_chunk` |
| `snapshot.table.before_complete` | `snapshot_worker` | before `complete_snapshot_table` |
| `meta.before_commit` | `pintail-meta/src/lib.rs` | before `transaction.commit()` in `commit_cdc_checkpoint`, `commit_poll_state`, `commit_poll_state_with_chunks`, `complete_snapshot_chunk` (error action only makes sense here) |
| `supervisor.handoff.after_begin` | `pintail-api/src/supervisor.rs` | after `begin_snapshot_job` returns `Ok` in the polling→cdc handoff |

Verify per slice: `clippy --all-targets --all-features -- -D warnings`,
`cargo test -p pintail-failpoint -p pintail-store`. Commit:
`feat(failpoint): test-only abort and error injection sites`.

Existing store tests to extend in the same slice (unit layer, fast):

- `recovery.rs`: `store.wal.before_sync` abort simulated by writing an
  unsynced append then reopening: rows before the last `checkpoint()` are
  present, the torn tail is discarded (this generalises
  `a_torn_final_wal_record_is_discarded_without_losing_prior_batches`).
- `meta.before_commit=error` on `commit_cdc_checkpoint`: the SQLite
  transaction rolls back, the previous checkpoint is intact, a retry
  commits. Same for `commit_poll_state_with_chunks`: no chunk row is
  half-written.

## 2. Slice 2: harness skeleton

Layout:

```
tests/e2e/lib.ts            pure helpers moved out of run.ts:
                            command, docker, dockerHost, publishedPort,
                            freePort, waitForMysql, diffRows
tests/e2e/run.ts            imports them; no behaviour change
tests/e2e/recovery/run.ts   the suite entry: container, scenario list,
                            ledger
tests/e2e/recovery/harness.ts  Scenario, Churn, comparator, api wrappers,
                            pintail process control, meta.db reader
tests/e2e/recovery/scenarios/*.ts  one file per area
tests/e2e/results-recovery.md      banked ledger (like results.md)
```

Moving helpers out of `run.ts` is a mechanical import change. The gate
is not re-run for it; the next rc run covers it. Do not move anything
that touches module-level state (`sql`, `api`, `pintailQuery`,
`tableDiff` depend on globals); the recovery harness owns its own
versions, parameterised by `{ schema, httpPort, wirePort }` instead of
globals, so scenarios can run against a fresh process each.

### `harness.ts` contracts

```ts
interface Scenario {
  slug: string                       // rec_<slug>_<nonce> schema
  area: Area                         // for the ledger grouping
  promise: string                    // the doc line enforced
  mode?: 'cdc' | 'polling'           // registration mode, default cdc
  failpoint?: string                 // PINTAIL_FAILPOINT for the first process
  seed(sql: Sql): Promise<void>      // tables + initial rows
  churn?: ChurnSpec                  // default: the standard 3-table churn
  inject(ctx: Ctx): Promise<void>    // step 3-4: wait for failpoint, break the dependency
  restore(ctx: Ctx): Promise<void>   // step 5: restore dependency, restart if aborted
  expect: {
    tableStates?: Record<string, string>   // e.g. { audit: 'needs_resync' }
    events?: RegExp[]                      // activity feed must contain
    noEvents?: RegExp[]                    // must NOT contain (e.g. /resync\.manual/)
  }
}
```

`runScenario(s)` does, in order: create schema and seed; start pintail
with `s.failpoint`; register the database (`api`); wait `streaming` or
`polling`; start churn; `s.inject`; assert the failpoint fired (stderr
line captured, `interrupts at <site>` check); read `pintail-meta.db`
while the process is down and record the durable state (`checkpoints`,
`tables.state`, poll state, snapshot chunks) as `durable-before-restart`
detail on the ledger; `s.restore`; wait for the database to leave
`error`/`snapshotting`; stop churn; `proveConverged()`; teardown (kill
pintail, drop schema, delete data dir).

`proveConverged()`:

1. wait until `compareExact()` returns no diff, deadline 180 s; the diff
   text is the FAIL detail.
2. `liveWrites()`: one INSERT, one UPDATE, one DELETE per base table,
   inside one transaction, then a second transaction that is rolled back;
   wait for exact convergence again.
3. SIGKILL pintail (a plain kill here: the failpoint env is not set on
   this process), restart, wait for exact convergence a third time.
4. record `converged:after-recovery`, `converged:after-live-writes`,
   `converged:after-second-restart` as three checks.

`Churn`: a seeded PRNG (`seed` logged in the ledger so a failure replays)
issuing transactions in a loop against the standard schema:

```sql
accounts(id PK, owner VARCHAR, balance DECIMAL(12,2), updated_at DATETIME(6))
ledger(id PK AUTO_INCREMENT, account_id, amount DECIMAL(12,2), note VARCHAR, created_at DATETIME(6))
audit(kind VARCHAR, payload VARCHAR)            -- keyless, duplicates on purpose
```

Each transaction touches at least two tables (transfer: two `accounts`
UPDATEs plus two `ledger` INSERTs plus one `audit` INSERT), and every
tenth is `ROLLBACK`ed. The writer runs on its own connection and
records nothing: MySQL is the oracle. `stop()` awaits the in-flight
transaction so step 6 compares a quiescent source.

`api()` refuses forbidden paths (principle 2) by throwing before the
request; `operatorApi()` allows them and is only imported by the
operator-repair scenario file.

`metaDb(dataDir)` opens `pintail-meta.db` read-only with `bun:sqlite`
(the gate already does this in the activity-history phase).

Docker actions used by the outage area, all against the run's own
container name: `docker pause` / `docker unpause` (connection hangs, the
realistic "source froze" shape) and `docker network disconnect bridge` /
`connect` (connection refused, the "source is gone" shape).

Commit: `test(recovery): harness, churn writer, and exact comparator`.
Include one smoke scenario (`baseline`: no failpoint, no injection) so
the skeleton proves itself before any failure is added.

## 3. Scenarios

Runtime per scenario is roughly one to two minutes (two restarts plus
three convergences). Ordered by expected discovery value; implement in
this order and commit each area as its own slice
(`test(recovery): <area>`).

### 3.1 Mode transitions (highest value: the handoff has a bug trail)

Promise: supervisor.rs handoff comment ("the only honest handoff is a
fresh snapshot") and the e2e check "mode switches to polling and back".

| Slug | Failpoint | Inject | Assert |
|---|---|---|---|
| `mode-cdc-poll-cdc` | none | churn running: `mode=polling`, wait one poll cycle with `changed`, `mode=cdc`, wait `streaming` | `resync.auto` event with "CDC handoff"; no `resync.manual`; exact |
| `mode-handoff-abort` | `supervisor.handoff.after_begin` | as above; the process dies after the handoff snapshot job begins | after restart the handoff completes on its own; the polling checkpoint is gone (`checkpoints.kind='gtid'` in meta.db); exact |
| `mode-handoff-snapshot-abort` | `snapshot.chunk.after_ingest@3` | as above, but the kill lands inside the handoff snapshot | restart resumes the snapshot from the journal; no table shows `streaming` before its copy is complete; exact |
| `mode-poll-during-cdc-lag` | none | `docker pause` the source 20 s under churn, unpause, immediately `mode=polling`, then `mode=cdc` | no lost writes from the paused interval; exact |

Deletes matter here: `liveWrites()` already deletes on every table, and
the churn includes DELETEs on `ledger`, so a handoff that snapshots and
then replays an older binlog position resurrects rows and fails the
diff.

### 3.2 CDC durability

Promise: `pintail-cdc/src/lib.rs` crate doc: "synchronizes every touched
table WAL, and only then advances the SQLite source checkpoint. A crash
therefore..." and `docs/limitations.md` CDC section on cross-table
visibility.

| Slug | Failpoint | Assert after restart |
|---|---|---|
| `cdc-after-ingest` | `cdc.after_ingest@5` | the transaction replays; every row exactly once (version identity dedups); checkpoint in meta.db before restart is the PREVIOUS transaction's |
| `cdc-after-first-table-sync` | `cdc.after_first_table_sync@5` | table 0's rows are in its WAL, table 1's are not, checkpoint is old: replay makes both exact; no duplicate in table 0 |
| `cdc-before-checkpoint-commit` | `cdc.before_checkpoint_commit@5` | all WALs synced, checkpoint old: replay is idempotent; exact |
| `cdc-after-checkpoint-commit` | `cdc.after_checkpoint_commit@5` | checkpoint new: restart continues from it, does NOT replay; exact; DLQ empty |
| `cdc-wal-before-sync` | `store.wal.before_sync@5` | the unsynced tail is discarded on reopen (the store's torn-record test, now end-to-end); checkpoint never past it; exact |
| `cdc-meta-commit-error` | `meta.before_commit=error` | the commit fails once, `sync_runs` records the error, the next cadence retries and commits; NO restart in `restore` (this is the "errors are visible, retry recovers" storage case); exact |

Each of these uses the standard churn so every transaction is
multi-table with a rollback every tenth. `@5` keeps the kill past the
initial snapshot's first CDC transactions so the checkpoint is already a
real GTID position.

Invariant recorded from meta.db before restart, per scenario:
`checkpoints.gtid_set` and `tables.state`. The check
`checkpoint:not-past-durable` asserts that after restart the first
transaction the stream applies has a GTID greater than the recorded set
(from the activity feed / `sync_runs`), i.e. the checkpoint pointed at
data the replica already held.

### 3.3 Interrupted recovery (purge → auto resnapshot → kill → kill)

Promise: `docs/limitations.md` CDC section: "Automatic purge recovery is
database-wide and attempted once per runner invocation."

Purge recipe (from the gate's drift check): pause the database via
`mode=paused` is an operator action and is FORBIDDEN here; instead stop
the source's visibility with `docker pause`, run churn for a bit, then
`FLUSH BINARY LOGS` twice and `PURGE BINARY LOGS TO <last>` on the
source while pintail is paused... pintail cannot be paused without an
operator action, so the honest shape is: SIGKILL pintail (a crash), write
and purge on the source, restart. The restart's runner hits
`NeedsResync` and recovers automatically.

| Slug | Failpoint on restart process | Assert |
|---|---|---|
| `purge-auto-resnapshot` | none | `resync.auto` event; every table passes through `snapshotting` and comes back `streaming`; exact |
| `purge-resnapshot-abort-once` | `snapshot.chunk.after_ingest@2` | second restart (no failpoint) completes the resnapshot from the chunk journal; no table shows `streaming` while its copy is partial; exact |
| `purge-resnapshot-abort-twice` | `snapshot.chunk.after_ingest@2`, then on the next restart `snapshot.table.before_complete` | third restart completes; the "once per invocation" rule means each restart is a fresh attempt: assert the ledger shows exactly three `resync.auto` events; exact |
| `purge-resnapshot-position-abort` | `cdc.resnapshot.after_targets` | the copy finished but the position was never adopted: restart must resnapshot AGAIN rather than stream from a stale checkpoint; exact |

"Partial copies never appear healthy": after every kill, read meta.db and
assert no table with an incomplete chunk journal has `state='streaming'`,
and `/api/databases/<id>` does not report `streaming` while any table is
`snapshotting`.

### 3.4 Schema changes during repair

Promise: `docs/limitations.md` DDL section: table RENAME is a resnapshot
boundary because the on-disk identity derives from the name; ALTER
during offline is quarantined if the final shape mismatches.

All four start from a big table (300k rows, as the gate's
restart-during-snapshot phase does) so the resnapshot is in flight for
long enough to inject.

| Slug | While | Do on the source | Assert |
|---|---|---|---|
| `repair-alter-add-column` | auto resnapshot after purge (3.3 recipe), table `snapshotting` | `ALTER TABLE big ADD COLUMN flag TINYINT DEFAULT 0`; INSERT rows using it | the finished copy has the new column for all rows; `information_schema.columns` diff empty; exact |
| `repair-truncate` | same | `TRUNCATE big`; INSERT 100 new rows | zero old rows reappear (assert `SELECT COUNT(*) WHERE id < 300000` is 0 on the replica); exact |
| `repair-drop-recreate` | same | `DROP TABLE big; CREATE TABLE big(...different columns...)`; INSERT | the replica adopts the new identity; no column of the old shape; no old row; exact |
| `repair-rename` | same | `RENAME TABLE big TO big2` | documented gap: `big2` never appears, `big` is `needs_resync` or orphaned per the doc; WARN with signature, not FAIL, exactly as the gate handles documented gaps |
| `reconcile-alter` | scheduled reconciliation running on a polling database (`reconcile_interval_seconds` set low) | `ALTER ... ADD COLUMN` mid-reconcile | reconciliation completes or restarts against the new schema; exact |

### 3.5 Polling durability

Promise: `pintail-poll/src/lib.rs` `run_poll_cycle` doc: "Every changed
table WAL is synchronized before its cursor/version is committed."

Register with `mode: 'polling'`. Seed three tables so all three
strategies are exercised: `accounts` with `updated_at` (Cursor),
`ledger` with a PK but no safe cursor column (KeyedChecksum), `audit`
keyless (append rebuild). Confirm the strategy chosen via
`/api/databases/<id>/tables` before injecting; a wrong strategy is a
setup FAIL, not a silent pass.

| Slug | Failpoint | Assert |
|---|---|---|
| `poll-after-ingest` | `poll.after_ingest@3` | page rows may or may not be in the WAL; cursor is old; the next cycle re-reads the inclusive boundary; exact, no duplicate keyed rows |
| `poll-before-state-commit` | `poll.before_state_commit@3` | WAL synced, cursor old (meta.db shows the previous `version`); re-read is idempotent; exact |
| `poll-append-after-reset` | `poll.append.after_reset@2` | the keyless table is EMPTY on disk with a completed-looking state? Assert that it is not: the state must still be the old version, and the next cycle rebuilds; exact multiplicity of duplicates |
| `poll-checksum-before-chunk-commit` | `poll.checksum.before_chunk_commit@2` | no chunk row describes a chunk whose rows are absent; next cycle re-dumps; exact |
| `poll-meta-commit-error` | `meta.before_commit=error` (polling) | the cycle errors visibly in `sync_runs`, the next one succeeds; no restart; exact |

### 3.6 Polling boundaries

Promise: `docs/limitations.md` DDL and polling section: inclusive
cursor-boundary reread; hard deletes visible until scheduled
reconciliation; keyless tables are append-generation.

Register with `mode: 'polling'` and `reconcile_interval_seconds` at the
lowest value the API accepts (§6 item 2). No failpoint; these are
semantic boundaries.

| Slug | Mutation | Assert |
|---|---|---|
| `poll-timestamp-ties` | 5,000 rows inserted with one identical `updated_at` across three transactions, straddling a poll cycle | every row present once |
| `poll-update-no-timestamp` | `UPDATE accounts SET balance = balance + 1` with `updated_at` unchanged (explicit assignment) | invisible until reconciliation; exact after the scheduled reconciliation runs (wait on the `reconciled` field in table status, never call the endpoint) |
| `poll-backdated-update` | `UPDATE ... SET updated_at = updated_at - INTERVAL 1 DAY` | same |
| `poll-delete-insert-neutral` | in one transaction: DELETE 500 rows and INSERT 500 new rows (count unchanged, MAX may change or not) during pagination | the count/MAX token is unchanged by design; the inclusive reread and the scheduled reconciliation converge; exact |
| `poll-keyless-dup-churn` | keyless `audit` receives duplicate rows and deletes of one copy | exact multiplicity after the append rebuild |

### 3.7 Source outages

Promise to be written first (§6 item 1). Until then these scenarios
assert only what is documented: the database reports `error` with the
connection failure, the other database keeps streaming, and the
restored source catches up exactly.

Two databases per scenario, `victim` and `bystander`, on the same
container, so "healthy databases continue receiving new writes" is
literal: the bystander's churn never stops and its exact diff is part of
the pass.

| Slug | During | Outage | Assert |
|---|---|---|---|
| `outage-during-snapshot` | initial snapshot of a 300k-row table | `docker pause` 30 s | snapshot resumes from the chunk journal after unpause; no chunk is journaled complete twice; exact |
| `outage-during-cdc` | churn | `docker network disconnect` 30 s, reconnect | `error` state visible with the MySQL error text; catch-up exact; bystander exact throughout |
| `outage-during-reconcile` | scheduled reconciliation on a polling database | `docker pause` 30 s | reconciliation restarts or resumes; exact |
| `outage-repeated` | churn | three outages of 15 s, 10 s apart | each recovers; `sync_runs` shows a bounded number of failed runs (assert the count against the policy once written); exact |

The pause/disconnect target is always the run's own container name.

### 3.8 Storage failures

Kept at two layers, on purpose:

- Store unit layer (`crates/pintail-store/tests/suite/recovery.rs`) via
  `store.wal.append=error` and `store.wal.sync=error` (add a `sync` site
  in the same slice as §1): a failed sync leaves the WAL recoverable to
  the previous checkpoint; a retry after the error succeeds.
- End-to-end via `meta.before_commit=error` (already in 3.2 and 3.5).

Filesystem-level injection (full disk, EIO) on the remote host is out of
scope; the failpoint is the isolated storage.

### 3.9 Operator-triggered repair (separate file, uses `operatorApi`)

Not automatic recovery, and kept apart so principle 2 stays enforceable.

| Slug | Flow | Assert |
|---|---|---|
| `operator-resync-table-abort` | quarantine `audit` (keyless UPDATE under `quarantine` policy), operator posts table resync, kill at `snapshot.chunk.after_ingest` | after restart the table is `needs_resync` again or the resync continues; it is never `streaming` with a partial copy; the operator resync completes on a second post; exact |
| `operator-reset-abort` | operator `reset`, kill at `snapshot.chunk.after_ingest@2` | restart resumes the reset snapshot; exact |
| `operator-reconcile-abort` | operator `reconcile`, kill at `poll.before_state_commit` | reconciliation state is not marked done; exact after the next scheduled one |

## 4. Validate stage, ledger, docs

- `scripts/validate.ts`: new stage `recovery`, `remote: true`, `cwd:
  tests/e2e`, `command: ['bun', 'run', 'recovery/run.ts']`,
  `timeoutMinutes: 75`, `stallMinutes: 15`. In the `stable` profile
  only, after `e2e-mysql80` and before `browser`. Not in `rc`: the rc
  gate is already ~65 min per leg. Document the reason in the stage
  comment, as the other stages do.
- The suite builds the binary itself with
  `CARGO_TARGET_DIR=target cargo build --features failpoints` into
  `target/debug/pintail` (same pattern the gate uses for its binary).
- Ledger `tests/e2e/results-recovery.md`, same shape as `results.md`:
  environment header, one row per check, WARN rows carry the documented
  gap signature. Bank on PASS with `test(recovery): bank the ledger for
  <version>`.
- `tests/e2e/README.md`: a "Recovery suite" section: what it proves,
  the failpoint env, how to run one scenario
  (`bun run recovery/run.ts --only mode-handoff-abort`), and the forbidden
  endpoint rule.
- `docs/verification.md`: one line in the gate sequence.
- `CHANGELOG.md` `[Unreleased]`: "Added: recovery suite (failpoints,
  N scenarios across 8 areas)".
- `docs/limitations.md` gets a line ONLY if a scenario surfaces a real
  gap that ships as WARN. No capability notes.

## 5. Slice order and commits

| # | Slice | Verify before commit |
|---|---|---|
| 1 | `pintail-failpoint` crate, feature wiring, all sites, store unit tests extended | clippy `--all-features`, `cargo test -p pintail-failpoint -p pintail-store -p pintail-meta` |
| 2 | `tests/e2e/lib.ts` extraction; recovery harness; `baseline` scenario | `bun run recovery/run.ts --only baseline` against the docker host |
| 3 | 3.1 mode transitions | `--only mode-*` |
| 4 | 3.2 CDC durability | `--only cdc-*` |
| 5 | 3.3 interrupted recovery | `--only purge-*` |
| 6 | 3.4 schema changes during repair | `--only repair-*,reconcile-alter` |
| 7 | 3.5 + 3.6 polling | `--only poll-*` |
| 8 | 3.7 source outages (after the §6 decision) | `--only outage-*` |
| 9 | 3.9 operator repair | `--only operator-*` |
| 10 | validate stage, ledger, README, verification.md, CHANGELOG | one full `bun run scripts/validate.ts --stages=fmt,typecheck,unit,recovery` |

Never run the recovery suite concurrently with oracle/e2e/bench: it
shares the docker host. Runs longer than 15 minutes need the
`nohup caffeinate` + Monitor pattern from the project notes.

A scenario that finds a real bug: fix it in its own `fix(...)` commit
before the `test(recovery)` commit that proves it, so the test never
lands red.

## 6. Decisions needed before the marked scenarios

1. **Source-outage retry policy.** The supervisor is a flat cadence with
   no backoff and no cap. "Retries are bounded" needs a written promise
   (in `docs/limitations.md` or the API docs) before `outage-repeated`
   can assert a count. Options: keep unbounded retries and assert only
   visibility and eventual catch-up, or add exponential backoff with a
   documented ceiling. Recommendation: the former for this suite; the
   latter is a feature, not a test.
2. **Minimum `reconcile_interval_seconds`.** The default is 600 and the
   API does not validate a lower bound today. The polling-boundary and
   reconcile scenarios need something like 5. Confirm that a low value is
   accepted and that the supervisor honours it at test cadence; add
   validation if a floor is wanted, and then the test uses the floor.
3. **WAL sync policy.** The server opens stores with `WalSync::Checkpoint`
   (sync only at `checkpoint()`), so `cdc.after_ingest` and
   `store.wal.before_sync` are distinct points and both are worth
   testing. If that policy ever changes to `Always`, the two collapse and
   one scenario should be dropped rather than duplicated.
4. **Pause without an operator action.** 3.3 needs the replica to fall
   behind a purge. Crashing pintail (SIGKILL) is the automatic-safe
   shape; `mode=paused` is an operator action and stays out. Confirm
   that this is acceptable as the scenario's "downtime".

## 7. Review checklist (what the review will hold each scenario to)

- The failpoint fired and the ledger records `interrupts at <site>`.
- No forbidden endpoint reachable from the automatic scenario files
  (grep `operatorApi` imports: only `operator.ts`).
- `proveConverged()` ran: three convergence checks per scenario, the
  third after a second restart.
- Comparator is value-level with keyless multiplicity; no `COUNT(*)`
  equality anywhere as a pass condition.
- Waits are on state with a deadline; no bare sleeps as synchronisation.
- Churn ran through the injection (assert the writer's transaction
  counter advanced during the failure window), and included rollbacks.
- The doc line each scenario enforces is in its comment and still exists.
- Teardown removes only the run's own container, schema, and data dir.
- No hostnames, IPs, or context names in code, ledger, or comments.
- The full-suite runtime is under the stage timeout with margin.


## 8. Implementation clarifications (2026-09-05)

- Source outages assert visible failures and eventual catch-up, not a retry
  count: the current supervisor has no total retry cap. A five-second
  reconciliation interval is the test setting; the harness must verify
  an actual completed scheduled reconciliation at that cadence.
- Aborting a process is not a power failure. Complete unsynchronized WAL
  records can survive in the OS page cache. Tests permit that and require
  exact replay; only the explicit torn-tail test requires truncation.
  After syncing the first touched table, the other tables can already have
  complete WAL records, so absence of their bytes is not an assertion.
- A MySQL GTID set is not a scalar counter. The checkpoint assertions compare
  membership against a specific source transaction captured by the harness,
  plus exact recovered values; they never order whole GTID strings.
- Faults are armed on a restart after a proven baseline, unless the initial
  snapshot itself is the scenario. Global hit counts otherwise fire during
  setup rather than the operation the test claims to interrupt.
- A bystander on a paused/disconnected MySQL container is not healthy.
  Source-isolation cases need a separately reachable source, or a
  victim-specific connection fault. Container-wide faults cannot prove
  isolation between schemas on that container.
- Timed pauses alone do not establish overlap. Faults are released after
  observable stalled/error progress or a named boundary, with a deadline.
- `store.wal.sync` is the sixteenth site (error injection), next to
  `store.wal.before_sync`. Existing torn-prefix append tests remain:
  a pre-write injected error does not model a partially written record.
- Recovery event counts and journal reuse are asserted only when they are
  part of the implementation's promise. An interrupted consistent snapshot
  may need a fresh source snapshot; convergence and absence of falsely
  healthy partial data remain mandatory regardless of reuse.

- `poll.reconcile.before_state_commit` is an additional, reconciliation-only
  boundary immediately after `poll.before_state_commit`, guarded by
  `reconcile_requested`. This makes the schema-during-reconciliation case
  prove a scheduled reconciliation was interrupted, rather than an ordinary
  polling cycle that happened to run near the schedule.

- The checksum fixture deliberately gives `ledger` an explicit integer PK
  without AUTO_INCREMENT or a timestamp cursor. Either of those would select
  Cursor and let a purported KeyedChecksum scenario test the wrong path.
- Polling crash tests arm the append/chunk sites on their first hit after
  baseline. This permits comparison against the captured pre-crash version
  and complete chunk journal, rather than guessing what a prior cycle wrote.

- Internal CDC purge recovery now emits `cdc.resnapshot` with the unavailable
  source-position error. Unlike supervisor-triggered repair it does not emit
  the API's `resync.auto` event. The suite requires this diagnostic from the
  newly started process; a snapshot event left over from baseline cannot
  satisfy the recovery witness.

- Observed rename behavior is narrower than the planned gap: `big2` is copied
  and must pass every exact row, column and later-write check. The leftover
  `big` progress entry remains `snapshotting` with `copy_complete=0`. Only
  that named state gets WARN; no table's data or metadata comparison is skipped.
