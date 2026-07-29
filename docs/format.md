# Pintail storage format version 1

All integers are little-endian. Every variable byte string is encoded as a
`u32` length followed by exactly that many bytes. File-format version numbers
start at one; readers reject unknown versions rather than guessing.

## Table directory

One physical table owns:

- `.writer.lock`: advisory single-writer lock;
- `table.wal`: mutable recovery log;
- `manifest.ptm`: the only authority for live immutable segments;
- `segment-{id:020}.ptseg`: immutable columnar data;
- dot-prefixed temporary files used only until `fsync` plus atomic rename.

Opening a table locks the writer, verifies every live segment footer, removes
unreferenced `.ptseg` crash orphans, then replays WAL sequences newer than the
manifest checkpoint.

## WAL (`PTWAL`, version 1)

The six-byte header is `PTWAL` followed by version `1`. Records follow:

```
u32 payload_length
payload[payload_length]
u64 xxh3(payload)
```

The payload contains a strictly increasing `u64` sequence, a `u32` row count,
and typed rows. A row contains its composite primary key, values, `u64`
source version, and a one-byte tombstone flag.

Recovery verifies each checksum and sequence. An incomplete final length,
payload, or checksum is a torn tail and is truncated to the last complete
record. A checksum failure in a complete record is corruption and reports the
record byte offset. `always`, `checkpoint`, and `off` control data
synchronization. A manifest checkpoint covering every recovered sequence wins
after a crash between manifest publication and WAL reset.

## Manifest (`PTMAN`, version 1)

The checksummed binary manifest is:

```
"PTMAN" | u8 format_version
u64 generation
u32 current_schema_version
u64 current_schema_fingerprint
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
columns in schema order. System column identifiers occupy the top three
`u32` values; user identifiers are stable catalog column IDs.

Logical type IDs are boolean `0`, signed 64-bit `1`, unsigned 64-bit `2`,
IEEE-754 64-bit `3`, UTF-8 `4`, binary `5`, and composite key `6`.

### Column chunks and blocks

Each chunk begins with `u32 column_id`, `u8 logical_type`, and
`u32 block_count`. Each block is:

```
u32 row_count
bytes dense_null_bitmap
u8 encoding
u8 compression
u32 uncompressed_payload_length
bytes compressed_payload
u64 xxh3(compressed_payload)
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

The primary-key bloom filter is 2,048 bits with three xxh3-derived positions.
The sparse key index records the first key of each target-sized block.
Readers locate the footer from the final eight bytes and verify its checksum
before accepting the segment. Every decoded block separately verifies the
checksum of its compressed payload and reports the segment path and byte
offset on failure.

## Flush and recovery ordering

Flush sorts the memtable by primary key and writes one immutable segment:

1. write the segment temporary file, synchronize it, rename it, synchronize
   the directory;
2. publish and synchronize a new manifest containing the segment and flushed
   WAL sequence;
3. swap the in-memory manifest generation and clear the memtable;
4. truncate the WAL to its six-byte header and synchronize according to its
   policy.

A crash before step 2 leaves an orphan ignored and removed during reopen. A
crash after step 2 replays no covered WAL records. Thus a row is reachable
from either the old WAL state or the new manifest state.

## Reads, schema evolution, and compaction

Scans merge every pinned segment with the pinned memtable in primary-key
order, keep maximum `_version`, then remove `_deleted` rows. Pinned readers
retain their old manifest and memtable `Arc`s across flush and compaction.

A higher schema version may add nullable columns; old segment and WAL rows
materialize those columns as `NULL`. Stable-ID renames and segment-backed
drops are readable. Compaction rewrites dropped bytes away. Required-column
additions over existing data and physical type changes are rejected.

Compaction selects a bounded fan-in of overlapping files whose sizes differ
by at most fourfold. The debt value is the selected input bytes. Partial
merges retain tombstones because unselected older versions may remain. A
merge covering the complete manifest drops tombstones immediately and emits
zstd. Publication uses the same segment-before-manifest ordering as flush.
Obsolete files are deleted only after every snapshot pinning their manifest
generation releases it; a process restart can immediately remove files not
listed by the durable manifest.
