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
4. verifies every referenced segment before exposing the snapshot.

Scans merge versions by physical key, keep the highest source version, and
then remove tombstones. Segment key bounds, bloom filters, block zone maps,
and projection pushdown avoid unrelated I/O. Old segments are reclaimed only
after every reader pin releases them.

The byte-level format and crash ordering are specified in
[`format.md`](format.md).

## Query path

HTTP and MySQL clients both construct the same catalog and open the same
reader-only snapshots. The SQL frontend binds names and MySQL coercions,
creates a logical plan, applies conservative correctness-preserving
optimizations, and lowers to the vectorized executor. Execution uses
4,096-row batches, parallel projected scans, bounded results, and one
configurable hard memory ceiling per query.

The engine is deliberately read-only. Session setup and transaction commands
needed by common MySQL clients are accepted as compatibility no-ops; data
mutation statements are rejected.

## Control plane and security

The first admin password uses Argon2id. Browser sessions use a signed JWT.
Source DSNs and backup credentials are encrypted with ChaCha20-Poly1305 under
the local DSN key. Database API keys are hash-only and shown once; a stored
double-SHA-1 verifier supports `mysql_native_password` challenge
authentication without retaining the key plaintext.

Pintail does not terminate TLS and its embedded dashboard is not a
multi-tenant security boundary. Keep listeners private or place a
TLS-capable ingress in front. S3 prefix validation is an accident guard, not
tenant isolation.

## Operations and recovery

Prometheus metrics cover query work, replication cycles and lag, row counts,
RSS, storage, compaction debt, DLQ depth, and backup outcomes. A failed row is
quarantined with its source location; retry performs a safe reconciliation
before deleting the DLQ record.

Backups pin manifests, upload checksum-addressed immutable segments, reuse
objects across incremental generations, and publish the portable manifest
last. Restore verifies SHA-256 checksums and creates a new detached database;
it never overwrites the source replica.

Compatibility boundaries that remain by design are listed in
[`limitations.md`](limitations.md).
