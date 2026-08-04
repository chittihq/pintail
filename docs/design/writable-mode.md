# Writable local databases — design (issue #7)

Status: accepted design, pre-implementation. The durability ordering here
must land before any DML code does.

## Goal and boundary

Pintail gains a second database kind: **local** databases owned by Pintail
itself, accepting `CREATE TABLE`, `INSERT`, `UPDATE`, and `DELETE` over the
MySQL wire protocol and the HTTP API. Databases attached to MySQL/MariaDB
remain read-only replicas; nothing here introduces bidirectional
replication or source-of-truth ambiguity. Replicated databases keep the
existing read-only error for every mutating statement.

## What already exists

The storage layer was built database-first and needs no format change for
phase 1–3 semantics:

- one checksummed shared WAL per database (`database.wal`) spanning every
  registered table, with complete-record replay and atomic reset;
- versioned rows and tombstones merged by physical key, highest version
  wins, tombstones removed at read;
- immutable segments published before any manifest references them, so a
  row is always in either a complete WAL record or a manifest-listed
  segment;
- reader-pinned manifest generations: a query sees exactly one generation
  plus the complete WAL records newer than its flushed sequence;
- stable table/column IDs with schema history.

The missing layer is a user-facing mutation engine and its transaction
semantics — a `WriteEngine` that owns binding, constraint checks, commit
versions, and WAL commit ordering, so wire, HTTP, and tests share one
implementation.

## Database kinds

`databases.kind`: `replicated` (default, today's behavior) or `local`.
Local databases have no DSN, no probe, no supervisor cycle, and no
snapshot/CDC/poll machinery; the supervisor skips them entirely. They are
created through the control-plane API (and later `CREATE DATABASE` on the
wire — the API path lands first because auth and storage-root placement
already live there).

## Versions

Local commits allocate from a per-database monotonic **commit version**
counter, persisted in metadata and recovered as
`max(persisted, max version seen in WAL replay) + 1`. It reuses the
existing `u64 source_version` row field — replicated databases fill it
with binlog-derived versions, local databases with commit versions; the
storage layer is indifferent. Backups carry it unchanged.

## Durability ordering

The shared WAL gains one record type: **transaction commit**, carrying a
transaction ID and the count of row records it covers. Ordering for a
commit touching tables T1..Tn:

1. Row records for all tables append to the shared WAL, tagged with the
   transaction ID, **unfsynced**.
2. The commit record appends; the WAL is fsynced once.
3. Memtables apply the rows (visible to new readers).
4. Flush/compaction/manifest publication proceed exactly as today.

Recovery replays only row records covered by a commit record; a torn or
uncommitted tail is discarded exactly like today's incomplete-record rule.
This makes multi-statement, multi-table transactions all-or-nothing with
one fsync per commit, and autocommit statements are just one-row-batch
transactions. SIGKILL between steps 2 and 3 recovers the commit from the
WAL; SIGKILL before step 2 loses the transaction wholesale. Pinned readers
never observe a partial commit because visibility flips only at step 3,
atomically per table memtable, under the single-writer lock (see
isolation).

### Catalog changes (`CREATE TABLE`)

Catalog and data cannot share one WAL record today (the catalog lives in
SQLite metadata). Ordering:

1. Allocate stable IDs; write the table row to metadata with state
   `creating`.
2. Create the store directory and publish its empty manifest.
3. Flip metadata state to `ready`.

Recovery deletes `creating` leftovers (directory first, then row) — the
same side-by-side-then-flip pattern restore already uses. DDL is
autocommit-only in every phase (as in MySQL).

## Isolation and concurrency

**One serialized writer per local database** (phase 1–4). Transactions get
snapshot isolation trivially: a transaction pins a reader snapshot at
BEGIN, buffers its writes in a session overlay (read-your-own-writes reads
check the overlay first), and at COMMIT — since no concurrent writer
exists — appends and applies without conflict checks. Write-write
conflicts cannot occur; the isolation level is effectively serializable
for writes and snapshot for reads. `ROLLBACK` drops the overlay. The
single-writer lock is the existing store writer lock; wire sessions queue
on it per statement, transactions hold it from first write to
COMMIT/ROLLBACK with a lock timeout returning MySQL error 1205.

## Constraints

Phase 2 enforces primary-key uniqueness only: an INSERT probes the pinned
snapshot plus the session overlay for the key; duplicates return MySQL
error 1062. UNIQUE beyond the PK, foreign keys, CHECK, triggers, and
secondary indexes are explicit non-goals for the MVP. `UPDATE` writes
replacement rows at the new commit version; primary-key-changing UPDATE
is a tombstone plus insert in the same transaction. `DELETE` writes
tombstones. Mutations respect the query memory ceiling by planning the
predicate through the existing executor and applying in bounded chunks.

## Wire surface

`INSERT`/`UPDATE`/`DELETE` return affected-row counts and
`LAST_INSERT_ID()` (AUTO_INCREMENT allocates from the same persisted
counter mechanism as commit versions). `BEGIN`/`COMMIT`/`ROLLBACK` become
real session state for local databases; they stay compatibility no-ops
for replicated ones.

## Backup and restore

Local databases back up through the existing manifest-object machinery
unchanged; the commit-version counter rides in the control-plane payload.
Restore registers the database as `local` instead of `restored`-paused.

## Delivery phases and gates

1. Database kind + commit versions + WAL commit records + recovery — gated
   by SIGKILL matrix tests at every ordering point.
2. Autocommit CREATE TABLE / INSERT with PK enforcement — gated by a new
   e2e phase driving the wire (create, insert, restart, verify).
3. Autocommit UPDATE / DELETE — same gate, extended.
4. Explicit transactions with read-your-own-writes — gated by a
   multi-statement SIGKILL/rollback matrix.

Each phase must also pass the full existing battery (oracle, e2e,
benchmark: local-mode code must not touch replicated read paths — the
bench proves it).

## Resolved open decisions

- Isolation: single serialized writer ⇒ snapshot reads, serialized
  writes. Revisit concurrency only after phase 4 ships.
- Catalog vs data WAL: separate (SQLite catalog journal with
  creating→ready flips); data-only transactions in the shared WAL.
- UNIQUE beyond PK: out of MVP.
- Creation surface: control-plane API first, SQL later.
- Version field: reuse `source_version` as the local commit version.
