# Would another metadata store be faster than SQLite?

Assessment date: 2026-10-03. No engine switch accompanies this; it is a
decision record. The question was whether replacing SQLite (through
`rusqlite`) with Turso Database, the Rust rewrite of SQLite, would remove
a lot of overhead. Figures are from the 20-million-row benchmark replica on
an 8-core, 16 GB virtual machine, release build, result memo off, load
generated on the same machine.

## Answer

No. A statement on the wire does not touch the metadata store, and on
the replication path what the store costs is the wait for the disk, which
any engine with the same durability pays. The paths where it was large -
a statement over HTTP, above all with a session token, and a wire
connection per request - were large because of how the store was used,
not which store it was, and are fixed without changing it (below).
What a metadata store costing nothing at all could still save is listed
at the end; it is the upper bound for any engine.

## What the control plane asks of its store

- One writer process, which holds the data directory's lock. Several
  threads in it, each with its own connection, in write-ahead-log mode.
- Small transactions. The largest routine one is the replication
  checkpoint: one row upserted, one row updated per touched table, one row
  deleted.
- Every commit durable. `synchronous` is never set, so the bundled
  build's default applies: FULL, which in write-ahead-log mode
  synchronizes the log on every commit. The apply intent and the
  checkpoint are what make a replay after a kill exact, and both rely on
  this.
- A commit hook and a log hook: they move the write generation that lets a
  query prove its replica current without reading a file, and the log hook
  checkpoints the log.
- A file operators copy and open with the `sqlite3` command-line client
  while the server runs; an hourly integrity check and foreign-key check;
  a consistent copy every six hours (`VACUUM INTO`); 23 migrations, two of
  which rebuild a table with foreign keys switched off and verify them
  afterwards; a rename that defers foreign keys inside its transaction.
- The project's dependency policy names the embedded SQLite as the
  control plane's store (`GOAL.md` section 3, `docs/decisions.md`), and
  forbids `unsafe` in Pintail's own code.

## Where the store was on each path

Share is the share of the server's on-CPU samples whose stack holds a
metadata-store frame (`perf`, 499 Hz, whole process); microseconds are
that share of the process CPU per operation; log synchronizations are
counted with `strace` on the metadata file. Before the fixes below.

### Per statement

| Path | Server CPU per statement | Metadata share | Metadata per statement | Log syncs per statement |
|---|---|---|---|---|
| Wire, `SELECT 1+1`, 1 / 8 / 32 connections | 20 / 15 / 23 µs | 0.0 / 2.8 / 7.1% | 0 / 0.4 / 1.6 µs | 0 |
| Wire, key lookup, 1 / 8 / 32 | 115 / 73 / 71 µs | 0.0 / 0.2 / 0.4% | under 0.3 µs | 0 |
| Wire, grouped aggregate over 20M rows, 1 / 8 / 32 | 148 / 27 / 5.6 ms | 0.1 / 0.3 / 0.6% | 30 to 120 µs | 0 |
| HTTP with an API key, `SELECT 1+1`, 1 / 8 / 32 | 0.76 / 1.17 / 0.99 ms | 1.4 / 3.6 / 3.6% | 10 / 42 / 36 µs | 0 |
| HTTP with an API key, grouped aggregate, 1 / 8 / 32 | 204 / 25 / 8.3 ms | 0.2 / 1.0 / 1.3% | 100 to 400 µs | 0 |
| HTTP with a session token, `SELECT 1+1`, 1 / 8 / 32 | 1.6 / 2.4 / 3.3 ms | 57 / 83 / 87% | 0.9 / 2.0 / 2.9 ms | 1.0 to 1.1 |
| HTTP with a session token, key lookup, 1 / 8 / 32 | 1.5 / 2.2 / 2.5 ms | 42 / 85 / 81% | 0.6 / 1.8 / 2.0 ms | 0.8 to 1.1 |
| HTTP with a session token, grouped aggregate, 1 / 8 / 32 | 183 / 38 / 13 ms | 5 / 36 / 67% | 9 to 13 ms | 0.4 to 1.1 |
| Wire, connect + `SELECT 1+1` + close, 1 / 8 connections | 0.6 / 0.8 ms | 32 / 57% | 0.2 / 0.5 ms | 0 |

The wire figures above zero were not attributed further. The one user of
the store running beside those statements was the replication
supervisor's idle cycle, which committed three times every five seconds
and made the next statement re-read the replica signature.

The HTTP rows were taken with compressed responses, whose cost is most of
the CPU per statement in them and makes the store's share look small on
the API-key rows: one store open and one row read per statement, 10 to 50
µs, which is most of an uncompressed statement (see the fixes).

A statement on the wire touches the store hardly or not at all, as
believed. The session-token path did:
each request read the account and its membership on the thread serving
the connection, read the database row, and then recorded an audit row from
a blocking thread of its own - a store open, a read, a wait for the write
lock, one committed row. Seventy percent of the server's CPU at eight
sessions was those audit threads, a third of that spinning on the write
lock, and throughput did not rise with sessions at all (about 2,000
statements a second at 1, 8 and 32). A connection to the wire port read
every database's row, probe report included, and every key of the one it
named.

### Replication

OLTP shape: eight writers, 1 to 10 rows a transaction, 9,300 transactions
and 51,000 rows a second on the source. Large shape: one writer, up to
5,000 rows a transaction, 45,000 rows a second. Trickle: one row, twenty
times a second.

| Measure | OLTP | Large | Trickle |
|---|---|---|---|
| Batches in the 36 s window | 196 | 52 | 10 |
| Apply busy time (read + decode + ingest + sync + checkpoint) | 11.2 s | 13.2 s | 1.5 s |
| of which the checkpoint commit | 0.72 s (3.7 ms a batch) | 0.71 s (13.6 ms a batch) | under 1 ms |
| of which the apply-intent commit (inside ingest, estimated from the log-sync time) | about 0.7 s | about 0.3 s | - |
| Metadata as a share of apply busy time | about 12% | about 8% | under 1% |
| Metadata frames on the apply thread's CPU | 1.2% | 0.8% | - |
| Metadata log syncs per batch | 2 | 2 | 2 |
| One log sync, under this write load | 3.4 ms | 3.8 ms | - |
| Table log syncs, same window | 205 at 3.5 ms | 181 at 3.9 ms | - |
| Commit to visible, median / worst | 3.0 / 3.7 s | 4.0 / 8.8 s | 4.8 / 4.8 s |

The metadata's part of an apply is two commits a batch - the intent before
any row is written, the checkpoint after the tables are synchronized - and
nearly all of it is the wait for the disk: 1.2% of the apply thread's CPU
is in the store. The applier was busy a third of the time at 9,300
transactions a second, so those waits cost no throughput here; at
saturation they bound it by about a tenth. Commit-to-visible is set by the
supervisor's five-second cadence, not by any commit: the two metadata
syncs are about 7 ms of 3,000 to 4,800.

An idle database commits three times per cycle (a run row started, the
same row finished, the replication state rewritten): three log syncs every
five seconds.

### Dashboard, activity over a long history, startup

With 150,000 run rows and 155,000 audit rows in a 116 MB metadata file:

| Request | Median | Metadata per request | Share |
|---|---|---|---|
| Activity feed, newest 200 | 0.63 ms | 190 µs | 21% |
| Audit log, newest 200 | 0.73 ms | 193 µs | 19% |
| Dead letters, newest 100 | 0.10 ms | 49 µs | 40% |
| Database status | 0.30 ms | 51 µs | 10% |
| Table list | 0.43 ms | 107 µs | 15% |
| Snapshot status | 0.35 ms | 140 µs | 24% |
| Session | 0.30 ms | 39 µs | 9% |
| Metrics scrape | 3.2 ms | 36 µs | 1.5% |

The dashboard polls these every eight seconds. Both feeds walk an index
and stop at the limit; nothing scans. Startup to the first health
response took 33 to 83 ms with that file and a cold page cache, the whole
of it; the full integrity check of the file took 0.28 s and runs off the
startup path.

## What was fixed without changing the store

Each is a commit of its own. Base and fixed binaries interleaved, each on
its own copy of the replica, six rounds, five seconds a cell; medians.

1. A commit that writes only journal rows (an audit event, a run row, a
   key's last use) no longer moves the write generation, so it no longer
   makes the next statement re-read the replica signature.
2. The audit rows of queries are written by one thread, as many to a
   commit as have queued: log syncs per statement fell from 1.0 to 0.16
   with one session and to 0.002 with eight or more, and nobody waits on
   the write lock.
3. A session's account and role, and the database a query names, are read
   once per write generation rather than per request, with a five-second
   lapse for changes made by another process.
4. A wire login's database and keys are read once per write generation.

| Cell | Before | After |
|---|---|---|
| HTTP session, `SELECT 1+1`, 1 connection | 1,907 /s, 1.01 ms CPU | 2,721 /s, 0.70 ms CPU |
| HTTP session, `SELECT 1+1`, 8 connections | 2,041 /s, p50 3.8 ms | 6,296 /s, p50 1.2 ms |
| HTTP session, `SELECT 1+1`, 32 connections | 2,034 /s, p50 15.6 ms | 8,359 /s, p50 3.6 ms |
| HTTP session, key lookup, 8 connections | 1,944 /s | 5,263 /s |
| HTTP session, grouped aggregate, 8 connections | 229 /s | 261 /s (ranges overlap) |
| Wire connect + statement + close, 1 / 8 / 32 connections, CPU per connection | 0.60 / 0.84 / 2.09 ms | 0.43 / 0.60 / 1.20 ms |
| Controls: HTTP with an API key, wire statements | unchanged within run-to-run noise | |

The load generator above asks for a compressed response, as a browser
does, and compressing a 300-byte body is then most of what an HTTP
statement costs (about 0.7 ms of CPU against 0.04 without). That floor
hides most of the change. With the client asking for an uncompressed
response, as a script or a command-line client does (three rounds,
interleaved, medians):

| Cell, uncompressed response | Before | After |
|---|---|---|
| HTTP session, `SELECT 1+1`, 1 connection | 2,660 /s, 656 µs CPU | 15,726 /s, 140 µs CPU |
| HTTP session, `SELECT 1+1`, 8 connections | 2,779 /s, p50 2.8 ms, 702 µs CPU | 60,796 /s, p50 0.12 ms, 50 µs CPU |
| HTTP session, `SELECT 1+1`, 32 connections | 2,712 /s, p50 11.6 ms | 73,333 /s, p50 0.37 ms |
| HTTP API key, `SELECT 1+1`, 8 connections | 32,096 /s, 128 µs CPU | 62,510 /s, 47 µs CPU |

The API-key row is the database check, which opened the store and read a
row on the thread serving the connection for every statement.

After them the store is absent from the request threads of the session
path (no sample under authentication or the database check). What is
left is the audit writer: 5 to 6% of the server's CPU with compressed
responses, and with one session and uncompressed responses the difference
between 140 µs and the 50 µs of a statement that shares its commit with
many others.

Not changed, deliberately: the two metadata commits of a replication
batch. The intent has to be durable before the first row is written and
the checkpoint after the last table is synchronized; merging them or
relaxing `synchronous` would trade the exact replay for a tenth of the
applier's busy time that it is not short of.

## The candidates

Facts are from the projects' own repositories and documentation as read on
2026-10-02 and 2026-10-03; anything not confirmed from a primary source is
marked.

### Turso Database (the Rust rewrite; formerly Limbo)

- **Release status.** v0.8.1, 2026-09-29 (crate `turso` 0.8.1 the same
  day; 0.8.2 pre-releases since). Not 1.0. The project's own FAQ says it
  runs production applications and also that it "has not yet reached 1.0",
  that some features are experimental, and to "keep independent backups".
  MIT licence.
- **API.** Async only (`Builder::new_local(..).build().await`). Its Rust
  binding describes itself as similar to `rusqlite` "apart from using
  async Rust"; there is no `rusqlite`-compatible layer. Every method of
  `pintail-meta` is synchronous and is called from blocking threads, the
  replication thread and, in places, from async tasks directly, so this is
  not a drop-in: it is a rewrite of the crate's surface and of each call
  site's threading.
- **Durability.** `PRAGMA synchronous` supports `OFF` and `FULL` (default
  FULL, a sync per transaction); `NORMAL` is not available. Whether the
  commit path synchronizes the log exactly as documented was not verified
  from its source. The experimental multi-version mode documents a case in
  which a write reported as successful is rolled back.
- **File format.** Documented as fully compatible with SQLite's, with the
  guarantee that a database can be taken back to SQLite. Only
  write-ahead-log mode exists. Mixed use of the two engines on one file
  from several processes is explicitly unsupported, and a second process
  opening the file is refused with a locking error by default (the
  shared-memory mode that would allow it is marked not production ready).
  So an operator could not open the live file with the `sqlite3` client,
  which is something operators do today; whether a read-only open of a
  live file works was not tested.
- **What Pintail uses that it lacks**, per its compatibility table:
  commit, rollback, update and log hooks (the write generation is built on
  two of them); the online backup interface; `PRAGMA foreign_key_check`
  and `PRAGMA defer_foreign_keys` (two migrations, the hourly check and
  the table rename use them); in-place `VACUUM` is experimental
  (`VACUUM INTO` is supported); `PRAGMA wal_checkpoint` is partial.
  Upsert, `ALTER TABLE`, indexes, triggers, savepoints, `user_version`,
  `busy_timeout` and `foreign_keys` are listed as supported. Partial and
  expression indexes (one of our indexes is partial) are not listed either
  way: not verified.
- **Concurrent writers.** `BEGIN CONCURRENT` under a multi-version mode
  with its own logical log. Pintail has one writer process and small
  transactions; after the audit rows moved to one writer nothing here
  waits on the write lock.
- **`unsafe`.** No `forbid(unsafe_code)`; several hundred `unsafe` blocks
  in its core by a text search of the main branch. Pintail forbids
  `unsafe` in its own code, not in dependencies, and the bundled SQLite is
  C - so this is not a rule broken, but it is not a safety gain either.
- **Testing.** A deterministic simulator, a third-party fault-injection
  service, and differential tests against SQLite. Its data-corruption
  bounty was retired in May 2026. Open issues in the last months include
  page-number corruption panics, an `ALTER` that left an index stale, and
  a checkpoint failure followed by a short read. No independent
  reliability audit was found.
- **Speed.** The vendor's 0.8 figures with `synchronous=FULL` on both
  engines put a single connection on par with SQLite, and show a large
  gain only with dozens of concurrent writers under the multi-version
  mode. The one independent comparison found (through its Node binding,
  version 0.7.1, without a sync per commit) had it 1.5 to 6 times slower
  than SQLite on reads, scans and batched inserts and about 4 times better
  on tail write latency. Nothing independent was found with a sync per
  commit or through the Rust interface. Pintail's shape - one writer, a
  sync per commit - is the single-connection case, where no gain is
  claimed even by the vendor.

### libSQL (the C fork)

Same file format and the same C core, at an older base (SQLite 3.47.0
against 3.53.4 upstream). Its own README says new features are being
developed in the rewrite and points new projects there. Its Rust crate is
built around its replication and remote features; a local-only build is
possible. For a local file it offers nothing Pintail would use, on an
engine version behind the one bundled today.

### SQLite, as now

`rusqlite` 0.37 with the bundled build is in the workspace; 0.40.2
(2026-08-08) bundles SQLite 3.53.2. FULL in write-ahead-log mode is the
documented durable setting; NORMAL "might roll back following a power
loss", which the checkpoint and the intent cannot accept. Concurrent
writers remain on a branch upstream, not in a release.

## What switching would take

Because the candidate is not a drop-in, no benchmark of it was built. A
switch would mean: an async surface for `pintail-meta` and every caller
(or a blocking bridge on every call); another way to learn that a commit
happened, in place of the commit and log hooks; rewriting the two
table-rebuild migrations, the rename and the hourly check without the
foreign-key pragmas; a different story for operators who open the live
file; and re-earning the recovery suite's confidence on an engine that
is pre-1.0. The storage it would be trusted with is the checkpoint and
the apply intent.

## Upper bound: what a metadata store that cost nothing would save

| Path | Most it could save, before the fixes | After the fixes |
|---|---|---|
| Statement on the wire | 0 to 0.6% (under 2 µs on a small statement) | the same or less |
| Statement over HTTP with an API key | 1.4 to 3.9% of a compressed response (10 to 50 µs); about 80 µs of the 128 µs of an uncompressed one | nothing measurable: the database check is kept |
| Statement over HTTP with a session token | 42 to 87% of server CPU | nothing on the request; the audit writer's commit, shared by however many statements queued behind the last one |
| Wire connection | 32 to 57% of the connect's CPU | 28 to 43% of that CPU already removed |
| Replication apply | about 12% of apply busy time, all but 1.2% of it disk waits that equal durability keeps | unchanged |
| Commit to visible | about 7 ms of 3,000 to 4,800 ms | unchanged |
| Dashboard request | 40 to 190 µs | unchanged |
| Startup | under 83 ms, the whole of startup | unchanged |

## Recommendation

Stay on SQLite. The store is not where the time goes; where it was, the
remedy was to ask it less. Revisit only if the applier becomes limited by
its two commits a batch, and then look first at whether the intent and the
previous checkpoint can share a commit - a change to the replay protocol,
to be proven by the recovery and chaos suites - before looking at another
engine. Worth doing independently: move `rusqlite` to the current
release for the newer bundled SQLite.

## Not verified

- The candidate's commit path was not read in source; its durability
  rests on its documentation.
- Whether the `sqlite3` client can read a file the candidate has open.
- The candidate's support for partial indexes.
- No measurement of the candidate was made, through any interface.
- Log-sync times are from a virtual disk that returned in 9 µs when idle
  and 3 to 4 ms under write load; on other storage the replication
  shares move with the sync time, and the two-per-batch count does not.
- The wire-connection comparison was noisy (throughput ranges overlap);
  the CPU per connection is the steadier evidence.
- Response compression was found to be the floor of every HTTP figure
  taken with a compressed response; it is outside this assessment and
  was not changed.
