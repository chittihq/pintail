# Architecture

Pintail is one Rust process with two read-only query surfaces and a supervised
replication worker per source database. MySQL remains the source of truth;
Pintail owns a local columnar replica, its query engine, and its operational
metadata. No external CDC or analytical database sits in the data path.

```text
MySQL / MariaDB source
        │
        ├── capability probe
        ├── consistent snapshot ── captured binlog position
        └── row binlog CDC or polling + reconciliation
                              │
                              ▼
 SQLite control metadata ── PTWAL ── PTSEG files + manifest
          │                              │
          ├── supervisor/metrics         └── reader-pinned snapshots
          ├── backup/restore                         │
          └── dashboard/API          SQL binder → planner → executor
                                                   │
                                      HTTP JSON and MySQL wire
```

## Process layout

The `pintail` binary loads CLI, environment, TOML, and default configuration
in that order. It creates first-boot secrets, opens the SQLite metadata store,
then starts:

- an Axum HTTP server for the embedded dashboard, authenticated REST API,
  WebSocket/SSE events, health, and Prometheus metrics;
- a read-only MySQL wire server using the same query engine;
- a five-second supervisor cadence that gives every eligible source its own
  finite replication task and failure boundary.

A source failure changes only that database's durable state and activity
stream. Other databases, query listeners, backups, and the dashboard continue
running.

## Crate responsibilities

| Crate | Responsibility |
|---|---|
| `pintail-types` | Logical types, values, schemas, keys, and versioned rows |
| `pintail-meta` | SQLite migrations and durable control-plane records |
| `pintail-store` | WAL, memtable, immutable PTSEG files, manifests, compaction |
| `pintail-catalog` | Query-visible databases, tables, and schema versions |
| `pintail-sql` | MySQL-dialect parsing, binding, and metadata statements |
| `pintail-exec` | Logical/physical planning, optimization, vectorized execution |
| `pintail-probe` | Source capabilities, keys, types, and replication mode |
| `pintail-snapshot` | Parallel consistent snapshots and resumable chunk journal |
| `pintail-cdc` | Native row-binlog decoding, transaction buffering, checkpoints |
| `pintail-poll` | Cursor sync, checksums, uniqueness audit, reconciliation |
| `pintail-wire` | Shared replica query service and MySQL protocol server |
| `pintail-api` | Authenticated control plane, dashboard, supervision, metrics |
| `pintail-backup` | Full/incremental S3-compatible backup and restore |

Dependencies point inward toward types, metadata, and storage. The binary is
the composition root; replication libraries do not start global background
services by themselves.

## Replication lifecycle

1. The probe reads server settings, grants, table engines, keys, columns,
   foreign-key cascades, and binlog capabilities. It recommends native CDC
   only for ROW/FULL binlogs with a usable replication account; otherwise it
   selects polling.
2. Snapshot workers share a MySQL lock-and-coordinate handoff, begin
   repeatable-read transactions, and bulk-publish independently resumable
   chunks. The captured GTID or file/position is written before the snapshot
   is handed to CDC.
3. CDC decodes FULL before/after row images into versioned rows. One source
   transaction remains invisible until its complete row batch is decoded.
   Transactions above the 256 MiB in-memory threshold spill to an anonymous
   temporary file and are reconstructed only at commit.
4. Each touched table WAL is synchronized before SQLite advances the source
   checkpoint. A crash can replay a committed source transaction, but stable
   source versions and merge-on-read make that replay idempotent.
5. Polling sources combine cursor reads with chunk checksums, key
   reconciliation, and secondary-UNIQUE audits. Tokens schedule work; they
   are never treated as proof that no rows changed.

ADD and DROP COLUMN evolve live through stable column IDs. Unsafe ALTER
shapes mark only the affected table `needs_resync`. Newly created tables can
be included automatically; dropped source tables remain as explicit orphans
until an operator decides their fate.

## Storage and read consistency

One table writer owns its WAL, mutable memtable, and manifest publication.
Flush and compaction always publish an immutable segment before a manifest
can reference it. Recovery therefore finds a row in either the last complete
WAL record or a manifest-listed segment.

Queries do not acquire the writer lock. A reader:

1. loads and pins one manifest generation;
2. reads only complete WAL records newer than that manifest's flushed
   sequence;
3. rechecks the manifest generation to exclude a publication/WAL-truncation
   race;
4. verifies each referenced segment's checksummed footer and structural
   metadata before exposing the snapshot.

Scans merge versions by physical key, keep the highest source version, and
then remove tombstones. Disjoint globally unique segments stream projected
columns directly and can be prefetched in parallel under a divided memory
budget. Large overlapping views first merge only key, version, and tombstone
headers, then late-materialize the winning projected values in chunks of at
most 8,192 rows. Segment key bounds, bloom filters, block zone maps, and
projection pushdown avoid unrelated I/O. Old segments are reclaimed only
after every reader pin releases them.

Flushes use 16,384-row blocks by default. Size-tier compaction admits at most
50,000 input rows per maintenance pass, performs a block-wise version merge,
and partitions retained output at 128,000 rows per immutable segment. An
oversized candidate is deferred rather than materialized opportunistically;
queries remain correct through merge-on-read while the operator observes the
resulting segment shape and maintenance metrics.

Segments an older build wrote keep their format until something rewrites
them, and a settled table that no merge picks would keep it for good. A
background sweep rewrites them into the format the running build writes,
on by default (`PINTAIL_SEGMENT_UPGRADE=off` turns it off). One upgrade
takes a table's old segments in manifest order, up to 64 MiB of files (at
least one segment), and rewrites each as a merge of that segment alone: the
same slot among `PINTAIL_MERGE_THREADS`, the same lowered priority and
pacing under statements, the same write budget
(`PINTAIL_MERGE_WRITE_BYTES_PER_SEC`), reserved segment IDs and publication;
each rewrite takes its input's place in the manifest, readers keep the
snapshot they hold, and an old file is deleted after its last reader. A
replication stream starts at most one upgrade when it ends, and only when it
reached the end of its source's log, started no merge (a merge rewrites its
inputs in the current format anyway, so a table with one planned is left to
it) and no table copy is running in the process; a table waiting for a
resync, or paused, is skipped. A segment written zstd-compressed by a
whole-table merge is written zstd-compressed again. The stream's tables
wait for the upgrade they started before they close, so the next stream
starts up to one group's rewrite later. The sweep needs free disk for one
group's rewrite at a time beside its inputs (at most 64 MiB, or the one
segment when it is larger), plus the merge disk reserve; without it the
upgrade waits. The
`pintail paths:` startup line reports `segment_upgrade=on|off`, and
`GET /api/storage` reports `segment_upgrade`: the upgrades running and done,
and each table's segments by format version. A table whose last old segment
is rewritten logs `finished upgrading` at info. Polling-mode tables are not
swept; their segments convert when a merge picks them.

A secondary side index answers equality and IN filters, and join key sets,
on integer columns other than the table key, whose values scatter across
every block so zone maps prune nothing. Per segment it holds the column's
non-NULL values sorted with their row positions; a scan that knows the only
values its rows can hold asks it for their rows and decodes only those, and
the scan's own predicates, the memtable overlay and tombstones still decide
every row, so the index chooses candidates and never answers. A probe
naming more than a quarter of a slice's rows declines and the slice scans
as before. Column choice is automatic: a scan builds postings the first
time it probes a column, and a column the index was used on gets a
postings section written by the table's next flush and compaction (segment
format 6), so a restart loads rather than rebuilds them.
`PINTAIL_SECONDARY_INDEX_COLUMNS` (comma-separated column names) persists
named columns from the first flush. Built and loaded postings share one
least-recently-used cache bounded by `PINTAIL_SECONDARY_INDEX_CACHE_MB`
(megabytes; by default an eighth of the memory the process has, and at
least 256); `PINTAIL_SECONDARY_INDEX=0` turns the index off. Rows still
in the memtable hold no postings; the lookup tests each of them directly
(a text value keyed once per distinct value) and leaves out the rows it
rejects before they are materialized, while a rejected row still masks
the segment row it supersedes.

Text columns answer equality and IN filters against text literals under the
collation the comparison itself compiles with (an explicit `COLLATE`, else
the column's, else the plan's). Their postings hold a 64-bit hash of each
value's collation key, so every value equal to a literal under that
collation - other case, accents, trailing spaces where the collation pads -
lands on the literal's hash, and a collision only adds a candidate the
predicate rejects. The persisted section holds the exact values; the keyed
postings derive from it per collation, keying each distinct value once.

A limited sort (`ORDER BY <integer column> [DESC] LIMIT k`, k up to 65,536)
over a scan with no predicates of its own asks the postings where the
first `k` rows in that column's order end: the value at or before which
the scanned segments hold `k` entries. The scan is narrowed to the rows at
or before it, the memtable's included, and every row it leaves out sorts
after every row it keeps, since only the first sort key decides that. The
bound counts superseded and deleted rows, and rows from outside it can
still come back, so the sort keeps an unnarrowed twin of its input and
reads that instead unless the `k`-th row it kept lies at or before the
bound. A nullable column whose NULLs sort first (ascending) is narrowed
only while no segment holds a NULL and the memtable is empty.

The byte-level format and crash ordering are specified in
[`format.md`](format.md).

## Query path

HTTP and MySQL clients both construct the same catalog and open the same
reader-only snapshots. The SQL frontend binds names and MySQL coercions,
creates a logical plan, applies conservative correctness-preserving
optimizations, and lowers to the vectorized executor. Execution uses
4,096-row batches, parallel projected scans, streaming joins and grouped
aggregation, bounded results, and one configurable hard memory ceiling per
query.

The engine is deliberately read-only. Session setup and transaction commands
needed by common MySQL clients are accepted as compatibility no-ops; data
mutation statements are rejected.

## Control plane and security

The first admin password uses Argon2id. Browser sessions use a signed JWT.
Source DSNs and backup credentials are encrypted with ChaCha20-Poly1305 under
the local DSN key. Database API keys are hash-only and shown once; a stored
double-SHA-1 verifier supports `mysql_native_password` challenge
authentication without retaining the key plaintext.

Roles are per workspace, and anyone may create a workspace and administer
it. Settings that apply to the whole node (the OAuth client, the wire
certificate's hostnames) belong to node administrators: the administrators of
the node's first workspace. Live event streams carry only the events of
databases the caller's workspace owns (an API key's stream, its one
database); events that belong to no database reach node administrators only.
A stream re-reads its caller's standing every few seconds and ends once the
account is disabled, the membership removed or the key revoked.

Pintail does not terminate TLS and its embedded dashboard is not a
multi-tenant security boundary. Keep listeners private or place a
TLS-capable ingress in front. S3 prefix validation is an accident guard, not
tenant isolation.

## Operations and recovery

Prometheus metrics cover query work, replication cycles and lag, row counts,
RSS, storage, compaction debt, DLQ depth, and backup outcomes. A failed row is
quarantined with its source location; retry performs a safe reconciliation
before deleting the DLQ record.

The include list and the table catalog are reconciled against the source.
`GET /api/databases/{id}/upstream-tables` probes the source and lists every
base table with its source row estimate and one status: `mirrored` (in the
catalog), `missing` (selected by the include/exclude lists but absent from
the catalog, so nothing mirrors it), `not-included` (left out by the lists),
or `dropped-upstream` (in the catalog, gone from the source). It is
read-only: the stored probe is not replaced. `POST
/api/databases/{id}/upstream-tables` with `{"tables": [...]}` appends the
named source tables to a non-empty include list (an empty list already
selects everything), takes them off the exclude list, refreshes the stored
probe, and starts a non-forced snapshot, which on a live database copies
only the selected tables the catalog lacks. It answers `202` with the run ID,
or with `"state": "queued"` and no run ID when the job slot stays busy. The
supervisor's automatic repair then starts the copy. Both routes need operator
access, refuse local databases, and the POST is audit-logged as
`database.upstream_tables.add`. After each cycle on a database that has
handed off to replication, the supervisor compares the stored probe with the
catalog. When selected tables are missing, it publishes `catalog.drift` with
their count and starts the same non-forced snapshot (`catalog.repair`, or
`catalog.repair_failed`), at most once every ten minutes per database.
Tables created mid-stream never reach this path, because the stream's DDL
adoption records them in the catalog and the probe together.
`pintail_catalog_missing_tables{database}` exports the count.

Backups pin manifests, upload checksum-addressed immutable segments, reuse
objects across incremental generations, and publish the portable manifest
last. Restore verifies SHA-256 checksums and creates a new detached database;
it never overwrites the source replica.

The control-plane metadata file is checked on purpose rather than when a
read trips over damage: a full SQLite integrity and foreign-key check at
startup, a quick check every hour. Damage is logged at error level (so it
reaches Sentry), published as a `metadata.damaged` event and shown on the
dashboard. While checks are clean, a consistent compacted copy is written to
`<data-dir>/meta-backups/` every `PINTAIL_META_BACKUP_HOURS` (default 6) and
the newest `PINTAIL_META_BACKUP_KEEP` (default 8) are kept; zero for either
turns copies off. Run history is pruned on the same pass: successful
replication cycles after a day, everything else finished after thirty days.
Audit events older than `PINTAIL_AUDIT_RETENTION_DAYS` (default 90; `0` keeps
every event; anything but a whole number logs a warning and keeps the default)
are deleted on the same pass, 10,000 per transaction; the setting is on the
`pintail limits:` line as `audit_retention`, and `GET /api/storage` reports it
with the last pass under `metadata.audit_retention`. Dead letters older than
`PINTAIL_DLQ_RETENTION_DAYS` (default 30, validated the same way) are deleted
on the same pass and in the same batches, reported as `dlq_retention` on that
line and under `metadata.dlq_retention`. The dead-letter list, its counts and
the `pintail_dead_letters` gauge are all read from the remaining rows, so a
pruned letter leaves them together; discarding or retrying one answers 404.

MySQL parity is tabulated in [`parity.md`](../parity.md); compatibility
boundaries that remain by design are listed in
[`limitations.md`](limitations.md).
