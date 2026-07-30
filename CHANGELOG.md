# Changelog

All notable changes to Pintail are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- MySQL-dialect SQL parsing façade with backtick identifiers, MySQL
  offset/count limits, metadata statements, explain, common table expressions,
  and explicit single-statement request validation.

## [M1] - 2026-07-30

### Added

- Dependency-free typed schema, scalar value, composite-key, and versioned-row
  model shared by Pintail's data-path modules.
- Single-writer table store with atomic typed batches, an RCU-style memtable,
  configurable WAL synchronization, length-prefixed records, and per-record
  xxh3 checksums.
- Database store with one globally sequenced WAL multiplexed by stable table
  ID; per-table flush checkpoints preserve every other table's unpublished
  records.
- WAL recovery that discards a torn final record while rejecting checksum or
  sequence corruption with the failing byte offset.
- Immutable version-1 `PTSEG` files with independently checksummed,
  LZ4-compressed column blocks, null bitmaps, block statistics, sparse
  primary-key indexes, bloom filters, and checksummed footers.
- Atomic, checksummed table manifests that publish flushed segments before WAL
  truncation and pin reader snapshots by reference-counted generation.
- Adaptive version-1 block codecs for plain, dictionary, run-length,
  bit-packed, and delta-bit-packed values, with typed min/max statistics and
  retained 64-register HLL sketches.
- Bounded size-tier compaction for similarly sized overlapping segments,
  including byte-debt reporting, max-version collapse, partial-merge
  tombstone retention, full-merge tombstone removal, and zstd cold output.
- Reference-counted obsolete-segment reclamation that preserves pinned reader
  generations across writer drop/reopen and cleans unreferenced crash orphans
  only after the last process-local snapshot releases.
- Metadata-only nullable column additions for older segment and WAL rows,
  stable-ID dropped-column reads, and compaction-time removal of dropped
  bytes, with incompatible physical changes rejected.
- Stable column IDs embedded in every WAL batch so reordered, inserted, and
  dropped columns recover without positional value shifts; schemas also
  reject IDs reserved for physical storage metadata.
- Explicit primary, UNIQUE-fallback, and append-rowid table modes; append mode
  generates durable monotonic storage keys and deliberately performs no
  source-key deduplication.
- Enforced memtable bounds: a threshold-crossing batch performs one bounded
  flush, compaction, and obsolete-file maintenance step.
- Storage metrics for memtable bytes, live segment count, and compaction debt;
  compaction yields between input segments to preserve query scheduling
  opportunities.
- Manifest-resident primary-key bounds and bloom filters with pruned point and
  inclusive range reads that skip unrelated segment block decoding.
- Retained-version range scans that prune segments whose stored version bounds
  do not overlap the requested filter interval.
- Projected range scans with checksummed key-block zone-map pruning,
  cross-segment winner resolution before late materialization of requested
  user columns, and physical scan counters.
- Whole-block xxh3 coverage for null bitmaps, codec metadata, compressed
  values, zone maps, and HLL sketches, preventing corrupt statistics from
  causing false pruning.
- A manifest `globally_unique_keys` marker on full-compaction output and a
  single-segment scan fast path that bypasses merge-on-read state.

### Verification

- Public-interface tests verify well-typed rows and reject nullability or type
  mismatches before ingestion.
- Reopen tests verify checkpoint recovery, pinned reader snapshots,
  last-version-wins tombstones, pre-WAL validation, torn-tail repair, and
  precise checksum failures.
- WAL storage-exhaustion tests inject `StorageFull` after a partial record and
  verify recovery preserves and truncates to the prior complete prefix; live
  write and `always`-sync append failures roll back before a caller can retry.
- Multi-table tests verify global WAL sequencing, recovery through one
  database log, safe partial-table flushes, and rejection of unregistered WAL
  table IDs.
- Segment tests cover every scalar and null representation, multi-block
  reopen, pre-flush snapshots, max-version merge-on-read across segments and
  WAL recovery, and precise block-checksum corruption.
- On-disk format tests force and round-trip all five version-1 block encodings.
- Compaction tests cover delayed reclamation, partial versus full tombstone
  rules, zstd cold output, and 96 deterministic randomized segment-count,
  non-monotonic-version, and tombstone interleavings against a naive reference
  model.
- Recovery tests verify live footers during open, discard unpublished segment
  orphans, and prefer a durable manifest checkpoint when a crash leaves the
  pre-flush WAL in place.
- A process-level crash-fuzz test performs 100 kill/reopen cycles while a
  separate writer loops two tables through the shared database WAL, flush,
  manifest, and compaction paths; each reopen is checked against an external
  acknowledged-commit oracle for the full two-table state. A dedicated
  child-to-parent acknowledgement pipe prevents test-harness output capture
  from making that oracle stale.

## [M0] - 2026-07-30

### Added

- Rust 2024 Cargo workspace and SQLite WAL-mode control plane.
- Complete version 1 metadata schema, transactional migrations, and
  insert-once settings.
- Bun-managed Nuxt 4 + shadcn-vue dashboard source with a generated Badge
  component and responsive M0 shell.
- Prescribed Rust crate, integration-test, load-generator, SQL-logic, and
  benchmark boundaries for every planned component.
- `pintail-api` Axum `/health` route and build-time embedding of freshly
  generated dashboard assets.
- Single `pintail` executable with TOML, `PINTAIL_*`, and CLI configuration.
- First-boot JWT and DSN-encryption secrets, displayed only when created; the
  JWT is insert-once SQLite metadata and the DSN key uses an owner-only Unix
  boot-secret file.
- Owner-only Unix permissions for the data directory, SQLite control-plane
  database, and its WAL sidecars.
- Bun-only multi-stage container build and persistent Docker Compose
  deployment.
- M0 milestone gate report, local quick start, and architecture decisions for
  build tooling and control-plane boundaries.

### Verification

- Migration tests verify every required control-plane table and idempotent
  reopen.
- Settings tests verify insert-once secret persistence.
- Bun type checking and static generation verify the dashboard source.
- Dashboard HTTP tests verify embedded HTML and the JSON health response.
- Binary boot/restart tests verify SQLite initialization, `/health`, and
  one-time secret display.
- Unix permission tests protect every file that can contain first-boot
  secrets.
- Concurrent first-boot tests verify that another process waits for a complete,
  durably published boot-secret file.
- Unified CI generates the dashboard before running Rust formatting, linting,
  and workspace tests against those exact static assets.
