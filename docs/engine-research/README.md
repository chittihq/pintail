# Engine research: novel approaches from ClickHouse, DuckDB, ScyllaDB, and the literature

Research conducted 2026-07-31 in support of the "beat ClickHouse" benchmark effort
([issue #3](https://github.com/chittihq/pintail/issues/3)). Four reports:

- [clickhouse-internals.md](clickhouse-internals.md) — source-level techniques from ClickHouse
- [duckdb-internals.md](duckdb-internals.md) — source + papers from DuckDB
- [scylladb-seastar.md](scylladb-seastar.md) — systems techniques from ScyllaDB/Seastar
- [papers-survey.md](papers-survey.md) — verified survey of ~40 papers (1998–2026)

## Synthesis

### Three structural finds (architecture-level, not incremental tuning)

1. **ClickHouse's FINAL range-splitting kills pintail's biggest tax.** `PartsSplitter.cpp` runs a
   sweep-line over PK ranges of granules, classifying them into non-overlapping ranges (read via
   the fast plain-scan path, no version resolution) and overlapping slivers (the only part that
   pays the k-way merge, itself parallelized by PK layers). In a compacted LSM the overlap is a
   sliver, so merge-on-read becomes nearly free. Pintail currently pays the merge on every query.
   Combined with offset-value coding and presortedness-adaptive merging (DuckDB 2025 sort
   redesign: 10.4× on presorted data), pintail's versioned dedup — ClickHouse's documented weak
   point (`FINAL`) — can become a strength.

2. **Immutable segments make two caches correct that nobody else can make correct.**
   Predicate/condition caching (Redshift SIGMOD 2024; ClickHouse query-condition cache 2025):
   per-(segment, predicate) matching-granule bitmaps are *immortal* on immutable segments.
   Result caching keyed on `(normalized plan, max segment version per table)` (Krypton, VLDB
   2023) is *exactly correct* under continuous CDC — where ClickHouse's query cache is
   deliberately transactionally inconsistent (TTL) and DuckDB has none. A legitimate way to beat
   ClickHouse on the dashboard workload pintail actually serves.

3. **ScyllaDB's compaction backlog controller solves compaction-vs-query contention.** One
   system: measure disk capacity at install (iotune) → schedule all CPU and I/O in userspace
   against explicit share classes (query/compaction/flush/WAL) → set compaction shares by a
   proportional feedback controller on a strategy-derived backlog signal → backpressure ingest
   with proportional ack-delay. Plus incremental compaction via fixed-size fragments (constant
   temp space, preemptable, checkpointable).

### Execution-core convergence (all four sources agree)

- **Vectors**: typed packed arrays with flat/constant/dictionary physical forms + selection
  vectors; dictionary encoding survives from storage into execution (group/filter/join on u8/u16
  codes; low-cardinality group-by = array-indexed accumulation, no hash table).
- **Aggregation ladder**: direct-array for tiny key domains → u64-key table + consecutive-keys
  cache → radix-partitioned two-phase only past ~10K groups, parallel per-bucket merge. Note
  Xue & Marcus 2025: shared concurrent table wins at low cardinality.
- **Joins**: Umbra's unchained hash table (48-bit pointers into hash-sorted tuple array + 16-bit
  Bloom tags; ~5-instruction probe) + join filter pushdown / LIP into probe-side zone maps
  (~10× on selective joins) + perfect-hash path for dense integer keys. No radix join at 20M.
- **Filters/counts**: bitmaps while combining zone-map/predicate results, one conversion to
  selection vectors for payloads (DaMoN 2021); filtered COUNT(*) = popcount over the mask; SMAs
  answer fully-covered counts from metadata.
- **Top-K**: per-thread heaps; the k-th value is a dynamic threshold pushed into granule pruning
  mid-scan; lazy materialization + version resolution only for the K survivors.
- **Strings**: German strings (16-byte views, 4-byte prefix, 12-byte inline); length-classed
  string hash tables (short strings become u64/u128 keys).
- **Compression**: FastLanes transposed bit-packing (Rust crate exists), ALP for floats, FSST
  under dictionaries, BtrBlocks-style sampling codec selection at segment freeze. Scan
  compressed.

### Decisions the literature settles

- **Stay vectorized, skip JIT** (Kersten VLDB 2018 bounds the loss; Photon's reasons apply
  doubly to a small team).
- **`portable_simd` / autovectorizable kernels, not per-ISA intrinsics** (Benson ADMS 2023;
  FastLanes) — also resolves the `unsafe_code = "forbid"` tension.
- **No secondary indexes** — zone maps + scan speed; sorted segments are the index (DuckDB's ART
  lesson).
- **Vector size ~1024**; morsel-driven work-stealing parallelism; thread-local sink states,
  lock-free hot path.
