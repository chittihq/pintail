# Pintail storage format version 1

All integers are little-endian. Every variable byte string is encoded as a
`u32` length followed by exactly that many bytes. File-format version numbers
start at one; readers reject unknown versions rather than guessing.

## Database and table directories

The production database layout owns:

- `.database.writer.lock`: advisory database-writer lock;
- `database.wal`: the one mutable recovery log shared by registered tables;
- `tables/<stable_table_id>/`: one manifest and immutable segment set per
  table.

Each physical table directory owns:

- `.writer.lock`: advisory single-writer lock;
- `manifest.ptm`: the only authority for live immutable segments;
- `segment-{id:020}.ptseg`: immutable columnar data;
- dot-prefixed temporary files used only until `fsync` plus atomic rename.

Opening a database locks its writer and validates or repairs the shared WAL
record framing. It then verifies each table's live segment footers, removes
unreferenced `.ptseg` crash orphans and interrupted `.ptseg.tmp` writes,
routes records by stable table ID, and replays sequences newer than that
table's manifest checkpoint.
Flushing one table leaves the shared WAL intact while any other table has
unpublished rows. The compatibility `TableStore` API uses the same format for
one table with ID `0` and a local `table.wal`.

## WAL (`PTWAL`, version 1)

The six-byte header is `PTWAL` followed by version `1`. Records follow:

```
u32 payload_length
payload[payload_length]
u64 xxh3(payload)
```

The payload contains a database-wide strictly increasing `u64` sequence, a
stable `u64` table ID, the writer's `u32` schema version, a `u32`
schema-column count, each stable `u32` column ID plus its one-byte logical
type, then a `u32` row count and typed rows. A row is:

```
u32 key_component_count
repeated key components: u8 tag | typed payload
u32 value_count
repeated values: u8 tag | typed payload
u64 source_version
u8 tombstone
```

Key tags are signed 64-bit `0`, unsigned 64-bit `1`, UTF-8 `2`, and binary
`3`. Value tags are null `0`, boolean `1`, signed 64-bit `2`, unsigned 64-bit
`3`, IEEE-754 64-bit `4`, UTF-8 `5`, and binary `6`. Integer and float
payloads are fixed-width little-endian; UTF-8 and binary payloads are
length-prefixed. WAL schema type tags use the segment physical type IDs;
logical M3 types are validated against the caller-owned schema on replay.
Stable IDs let recovery project reordered or dropped columns and materialize
new nullable columns as `NULL`.

Recovery verifies each checksum and sequence. An incomplete final length,
payload, or checksum is a torn tail and is truncated to the last complete
record. A checksum failure in a complete record is corruption and reports the
record byte offset. A live append that reports a write or `always`-sync I/O
failure first truncates back to its pre-record offset, so an immediate retry
cannot be stranded behind a torn record. `always`, `checkpoint`, and `off`
control data
synchronization. Each table manifest suppresses its already-flushed records
during replay. The shared WAL resets only when every registered memtable is
empty, so a crash between a table manifest publication and WAL reset cannot
discard another table's unpublished records.

Reader-only recovery never mutates the WAL. It accepts the same complete
checksummed prefix but ignores an incomplete final record rather than
truncating it, because the active writer may still be appending that record.
A reader loads a manifest, recovers WAL sequences newer than its
`flushed_wal_sequence`, then reloads the manifest. Any generation, epoch, or
flushed-sequence change restarts the open, preventing an old-manifest/new-WAL
combination during publication and WAL reset.

## Manifest (`PTMAN`, version 1)

The checksummed binary manifest is:

```
"PTMAN" | u8 format_version
u64 generation
u32 current_schema_version
u64 current_schema_fingerprint
u8 key_mode                 # primary=0, unique=1, append-rowid=2
u64 flushed_wal_sequence
u64 next_segment_id
u64 memtable_epoch
u32 segment_count
repeated segment_count times:
    u64 segment_id
    bytes file_name
    u64 row_count
    u64 min_version
    u64 max_version
    u64 segment_schema_fingerprint
    composite_key min_key
    composite_key max_key
    bytes primary_key_bloom_filter
    u8 globally_unique_keys
u64 xxh3(all preceding manifest bytes)
```

Publication writes and synchronizes `.manifest.ptm.tmp`, atomically renames
it to `manifest.ptm`, then synchronizes the table directory. A snapshot holds
an `Arc` to one immutable decoded generation.

## Segment (`PTSEG`, version 1)

### Header

```
"PTSEG" | u8 format_version
u32 segment_schema_version
u64 segment_schema_fingerprint
u64 row_count
u32 physical_column_count
u32 target_block_rows
```

Physical columns are the composite key, `_version`, `_deleted`, then user
columns in schema order. Their system IDs are respectively `u32::MAX - 2`,
`u32::MAX - 1`, and `u32::MAX`; schemas reject those IDs for user columns.
Other user identifiers are stable catalog column IDs.

Physical type IDs are boolean `0`, signed 64-bit `1`, unsigned 64-bit `2`,
IEEE-754 64-bit `3`, UTF-8 `4`, binary `5`, and composite key `6`. M3 logical
types reuse these carriers: `Int8/16/32` use signed 64-bit,
`UInt8/16/32` use unsigned 64-bit, `Float32` uses IEEE-754 64-bit, and
decimal/date/date-time/time/JSON use canonical UTF-8. The exact logical type
remains part of the schema fingerprint and caller-owned schema.

### Column chunks and blocks

Each chunk begins with `u32 column_id`, `u8 logical_type`, and
`u32 block_count`. Each block is a length-prefixed payload followed by
`u64 xxh3(block_payload)`. The payload is:

```
u32 row_count
bytes dense_null_bitmap
u8 encoding
u8 compression
u32 uncompressed_payload_length
bytes compressed_payload
u32 null_count
bytes typed_min
bytes typed_max
bytes hll_registers       # exactly 64 registers
```

Null bits are one for null and zero for present. Payloads contain present
values only. Min/max use logical ordering, including IEEE total order for
floating-point values. The retained HLL sketch uses 64 registers.

Encoding IDs:

- `0` plain: consecutive physical scalars;
- `1` dictionary: dictionary count and values, then `u32` value indexes;
- `2` RLE: run count, then `(u32 run_length, value)` pairs;
- `3` bit-packed: typed base, bit width, then packed normalized integers;
- `4` delta+bit-packed: first integer, bit width, then packed nonnegative
  deltas.

Pintail selects RLE for constant blocks, dictionary encoding for repeated
string/binary blocks, delta encoding for monotonic integers, bit-packing for
other integer/boolean blocks, and plain otherwise. Compression ID `1` is LZ4
and is the flush default. Compression ID `2` is zstd and is used by a
full-merge cold-tier compaction.

Physical scalars are one byte for boolean, eight bytes for integer/float
bits, and length-prefixed bytes for UTF-8/binary. Composite keys start with a
component count; each component has a type tag and typed bytes.

### Footer and trailer

```
"PTFTR"
u64 row_count
u64 min_version
u64 max_version
u64 segment_schema_fingerprint
u64 unique_key_count
composite_key first_key
composite_key last_key
u32 column_offset_count | repeated u64 column_offset
u32 sparse_key_count |
    repeated (u64 row_ordinal, composite_key key)
bytes primary_key_bloom_filter
u64 xxh3(footer bytes above)
u64 footer_start_offset
```

The primary-key bloom filter is 2,048 bits. Pintail xxh3-hashes the physical
key bytes, takes the value shifted right by 0, 21, and 42 bits, and reduces
each result modulo 2,048.
The sparse key index records the first key of each target-sized block.
Readers locate the footer from the final eight bytes and verify its checksum
before accepting the segment. Every visited block verifies the checksum of
its complete payload—including null bits, codec metadata, compressed values,
zone maps, and HLL—before its statistics can prune or its values can decode.
Failures report the segment path and byte offset.

## Flush and recovery ordering

Flush sorts the memtable by primary key and writes one immutable segment:

1. write the segment temporary file, synchronize it, rename it, synchronize
   the directory;
2. publish and synchronize a new manifest containing the segment and flushed
   WAL sequence;
3. swap the in-memory manifest generation and clear the memtable;
4. truncate a standalone table WAL to its six-byte header, or truncate a
   shared database WAL only after every registered table memtable is empty,
   and synchronize according to its policy.

A crash before step 2 leaves an orphan ignored and removed during reopen. A
crash after step 2 replays no covered WAL records. Thus a row is reachable
from either the old WAL state or the new manifest state.

## Reads, schema evolution, and compaction

Scans merge every pinned segment with the pinned memtable in primary-key
order, keep maximum `_version`, then remove `_deleted` rows. Pinned readers
retain their old manifest and memtable `Arc`s across flush and compaction.
Point reads prune manifest entries by key bounds and bloom filters before
touching segment files. Inclusive range reads prune disjoint manifest entries
by their persisted key bounds. Projected range scans then use checksummed key
block zone maps and return segment/block pruning counters for query
statistics. Disjoint globally unique segments decode requested columns
directly. A large overlapping view streams only key/version/tombstone headers
to choose winners, then reads the winning physical rows' requested columns in
chunks of at most 8,192 rows. Neither path requires loading a complete PTSEG
file into memory.

A higher schema version may add nullable columns; old segment and WAL rows
materialize those columns as `NULL`. Stable-ID renames and segment-backed
drops are readable. Compaction rewrites dropped bytes away. Required-column
additions over existing data and physical type changes are rejected.

The schema fingerprint is xxh3 over: `u32 schema_version`, `u8 key_mode`,
`u32 column_count`, then for each physical-order user column its `u32` ID,
one-byte logical type plus decimal precision/scale or temporal fractional
precision where applicable, one-byte nullable flag, raw UTF-8 name bytes, and
a zero terminator. Original M1 physical types retain tags `0..=5`; M3 adds
logical tags `6..=17` only inside the fingerprint input, not as PTSEG column
type tags. HLL uses the low six hash bits as the register index and the
leading-zero rank of the remaining 58 bits. Packed integers are written
least-significant bit first within each byte; bit width is the number of
significant bits in the maximum normalized value.

Snapshot bulk ingest validates and sorts one source chunk, writes its
version-zero rows directly as an immutable segment, and publishes the manifest
using the same segment-before-manifest ordering as flush. It does not append
snapshot rows to the WAL or memtable. The SQLite chunk journal is completed
only after manifest publication; replaying an interrupted chunk is
at-least-once and merge-on-read hides duplicate keys.

Compaction selects a bounded fan-in of overlapping files whose sizes differ
by at most fourfold. The default planner admits no more than 50,000 total
input rows to one pass; an oversized window is deferred rather than exceeding
the maintenance memory envelope. The merge advances one checksummed input
block at a time, moves the winning row into the output buffer, and publishes
at most 128,000 retained rows per output segment. The debt value for an
eligible plan is the selected input bytes. Partial merges retain tombstones
because unselected older versions may remain. A merge covering the complete
manifest drops tombstones immediately and emits zstd. Publication uses the
same segment-before-manifest ordering as flush. Obsolete files are deleted
only after every snapshot pinning their manifest generation releases it; a
process restart can immediately remove files not listed by the durable
manifest.
Full-merge output is marked globally unique in the manifest. A snapshot with
that single segment and no memtable rows returns its already-sorted rows
without allocating merge-on-read state.

The catalog fixes one key mode when a table is created: source primary key,
first source UNIQUE key, or append-rowid. Primary and UNIQUE modes resolve
maximum versions by that key. Append-rowid mode replaces the source key with a
monotonic generated `UInt64` storage key and therefore retains every source
row without deduplication. Reopen derives the next row ID from durable WAL and
segments; key mode cannot change after data exists.

## Portable backup manifest version 1

Backups preserve PTMAN/PTSEG bytes; they do not transcode the storage format.
Each backup publishes `<prefix>/<database>/<backup>/backup.json` only after
every referenced object exists. Its top-level fields are:

```text
format_version = 1
database_id
backup_id
parent_id | null
created_at
control_plane                 # portable JSON metadata, no source DSN
tables[]
```

Each table records its logical name, stable directory name, one manifest
object, and its complete segment-object list. Every object reference contains
an object key, lowercase SHA-256 digest, byte length, and the backup generation
that originally uploaded it. An incremental manifest can therefore reference
unchanged segment objects owned by an ancestor while still describing a
complete restorable database view.

Restore rejects unknown backup versions, downloads into a new temporary
directory, checks every byte length and SHA-256 digest, and publishes the
side-by-side directory only after the complete chain verifies. A backup
manifest is portable across S3-compatible services, but restoring PTSEG/PTMAN
version 1 still requires a Pintail binary that supports storage format 1.
