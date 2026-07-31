# ClickHouse internals: techniques extracted for pintail

Primary sources: the VLDB 2024 paper (web version: https://clickhouse.com/docs/academic_overview),
and source files from `github.com/ClickHouse/ClickHouse` (paths given per item).

---

## 1. Aggregation

### 1a. The hash-table variant zoo (key-type-specialized aggregation)
**Implementation** (`src/Interpreters/AggregatedDataVariants.h`, `src/Interpreters/Aggregator.cpp` — `chooseMethod`): ClickHouse instantiates **30+ hash table variants** from one generic template (paper §4.4), selected at query time by grouping-key type:
- `key8`/`key16` → `FixedHashMap` — **no hashing at all**: the key byte/short is a direct index into a 256/65536-slot array. `Aggregator.cpp` has `addBatchLookupTable8`: for 8-bit keys the aggregate-state pointer is fetched by direct table index, bypassing hash computation entirely.
- `key32`/`key64` → `HashMap` (open addressing, linear probing, power-of-two size, CRC32-based hash) with a *consecutive-keys cache* (see 1d).
- `key_string` → `StringHashMap` (see 1b).
- Multiple fixed-size keys → `keys128`/`keys256`: all key columns are **packed into one 128/256-bit integer** (`AggregationMethodKeysFixed`), with null bitmasks packed in, so the composite key is compared as one word.
- Fallback → `serialized`: keys serialized into an arena.
- `*_hash64` and `nullable_*` variants; `low_cardinality_*` variants (see 1c).

**Why fast**: eliminates hashing/compare cost where the type allows; a group-by on a UInt8 enum literally becomes `states[key]++`.
**Applicability: HIGH.** In Rust: an enum of monomorphized aggregation kernels chosen by key schema: direct-array for ≤16-bit keys, `u64`-keyed table for one integer key, packed `u128` for small composites, string table otherwise. The single most important design to copy during the typed-columns migration.

### 1b. StringHashTable: length-classed string hashing
**Implementation** (`src/Common/HashTable/StringHashTable.h`): four sub-tables partitioned by length: empty-string slot, 1–8 bytes packed into `StringKey8` (u64), 9–16 into `StringKey16` (u128), 17–24 into `StringKey24` (3×u64), ≥25 bytes into a generic string-view table. Short strings are loaded with one unaligned read + mask (`n[0] &= -1ULL >> s`), with a page-boundary check making the overread safe. Hash = hardware CRC32 (`_mm_crc32_u64` / `__crc32cd`) per 8-byte word. Dispatch is `switch ((sz - 1) >> 3)`.
**Why fast**: short strings (the common case) become fixed-width integer keys — integer compare instead of memcmp, no pointer chase, no allocation.
**Applicability: HIGH.** Directly portable to Rust (`u64`/`u128`/`[u64;3]` key types + CRC32 or aHash). Big win for GROUP BY on strings even before LowCardinality exists.

### 1c. LowCardinality aggregation on dictionary codes
**Implementation** (`src/Common/ColumnsHashing.h` — `HashMethodSingleLowCardinalityColumn`): when grouping by a dictionary-encoded column, ClickHouse keeps a `mapped_cache` (aggregate-state pointer per **dictionary position**) and a `visit_cache` (Empty/Found/NotFound per position). The hash table is consulted **once per distinct dictionary code per block**; every subsequent row with that code resolves via array index. The dictionary can carry a `saved_hash` array (precomputed hashes of dictionary entries), so even the first lookup skips hashing.
**Why fast**: turns an O(rows) hash workload into O(distinct codes) hash + O(rows) array indexing. For a 20M-row group-by over ~10 distinct strings, ~10 hash lookups per block total.
**Applicability: HIGH.** The benchmark's low-cardinality group-bys are exactly this. Dictionary-encode strings per segment at write time and aggregate on codes; remap codes → global groups per segment.

### 1d. Consecutive-keys cache
**Implementation** (`src/Common/ColumnsHashingImpl.h` — `LastElementCache`): caches the last key and its mapped state pointer; `if (cache.found && cache.check(key)) return cached;` before any hash work. Exploits sorted/clustered inputs.
**Applicability: HIGH, trivial.** One branch per row; huge when grouping by a column correlated with sort order (likely in sorted segments).

### 1e. Two-level hash tables + parallel bucket merge
**Implementation** (`src/Common/HashTable/TwoLevelHashTable.h`, `Aggregator.cpp`): each thread aggregates into its own table; when a table exceeds `group_by_two_level_threshold` (100k keys) or `..._bytes` (~50MB), it converts to a **two-level** table: 256 sub-tables addressed by the top byte of the hash (`worthConvertToTwoLevel`, `convertToTwoLevelTypeIfPossible`). Merge proceeds **bucket-by-bucket across a thread pool** (`mergeAndConvertOneBucketToChunk`): bucket i of every thread's table is merged by one worker, 256 independent merge tasks, no locks. Also: adaptive `__builtin_prefetch` in `executeImplBatch` — enabled when the table exceeds L2 size, look-ahead distance measured at runtime.
**Why fast**: merge (the serial bottleneck of thread-local aggregation) becomes embarrassingly parallel; resize of a huge table becomes 256 small resizes.
**Applicability: HIGH** for parallelism; MEDIUM if key cardinalities are small (single-level thread-local + serial merge suffices for low-cardinality group-bys — implement two-level only for the high-cardinality path).

### 1f. Sort aggregation fallback
Paper §4.4: when grouping columns are a prefix of the sort key, aggregate runs directly on pre-sorted input — no hash table at all. **Applicability: MEDIUM** — segments are already sorted by PK; free when group key = PK prefix.

---

## 2. IColumn representations

### 2a. PODArray
**Implementation** (`src/Common/PODArray.h`): vector of POD with (a) **no element initialization** on resize (claimed 2.5× faster than `std::vector` push_back), (b) `memcpy`-based growth, (c) **`pad_right = 15` bytes** so SIMD kernels may read/write past the logical end (`memcpySmallAllowReadWriteOverflow15`), (d) **`pad_left`** with a zero-initialized element at index −1 so `offsets[i-1]` works for i=0 without a branch — critical for the offsets→sizes conversion in string columns, (e) mmap for huge allocations.
**Applicability: HIGH.** In Rust: a padded buffer type (`Vec`-like with 16-byte slop both ends, `MaybeUninit`-style resize). Eliminates zeroing costs on every filter/materialize and lets kernels use unpadded unaligned SIMD loads without tail loops.

### 2b. ColumnString: two flat arrays
**Implementation** (`src/Columns/ColumnString.h`): `chars: PaddedPODArray<UInt8>` — all bytes concatenated — plus `offsets[i]` = end of string i (start = `offsets[i-1]` via the pad_left trick; strings may contain zero bytes). `insertFrom` = one `memcpySmallAllowReadWriteOverflow15`.
**Why fast**: zero per-string allocations, perfect cache locality on scans, filter = offset arithmetic + bulk copies.
**Applicability: HIGH** — the `StrColumn` layout to use verbatim (also Arrow's layout; ClickHouse's twist is end-offsets + padding to kill branches).

### 2c. ColumnLowCardinality
**Implementation** (`src/Columns/ColumnLowCardinality.h`): dictionary (`IColumnUnique` with a reverse index) + index column whose **width adapts** (UInt8→UInt16→UInt32→UInt64 as dictionary grows); dictionaries can be **shared** across chunks. **Applicability: HIGH** — pairs with 1c; makes filters/joins on such columns compare small ints.

### 2d. ColumnDecimal / ColumnVector
Fixed-width numerics stored as flat `PODArray<T>`; Decimal is scaled integers (`Decimal64` = i64 + scale metadata). The point: *everything* reduces to flat typed arrays plus per-type kernels. **Applicability: HIGH.**

---

## 3. Filter / scan

### 3a. SIMD filter with all-0/all-1 fast paths
**Implementation** (`src/Columns/ColumnsCommon.cpp` — `filterArraysImpl`; `src/Columns/ColumnVector.cpp` — `filter`): filters are **byte masks**, processed 64 bytes at a time. `bytes64MaskToBits64Mask` converts to a u64 bitmask via `_mm_cmpeq_epi8` + `_mm_movemask_epi8` (×4). Three cases: mask==0 → skip 64 rows; mask==all-ones → one bulk `memcpy` of 64 elements; mixed → iterate set bits with `std::countr_zero` + `mask &= mask-1` (`_blsr_u64`). On Icelake, `filter` uses AVX-512 `compressstore`. `countBytesInFilter` = same mask conversion + `popcount` per 64 — **filtered COUNT(*) never materializes anything**; pure popcount over the mask.
**Why fast**: real filters are bursty (sorted data ⇒ long runs of 0s/1s), so most chunks hit the bulk paths; count-only queries touch no data columns.
**Applicability: HIGH.** In Rust: `u8` mask + 64-wide chunks; movemask via `std::simd` or intrinsics; make `count` a popcount over the mask and never build result columns for `COUNT WHERE`. Plus typed columns, this likely closes a big chunk of the gap on filtered counts.

### 3b. PREWHERE two-phase reads (late materialization)
**Implementation** (`src/Storages/MergeTree/MergeTreeRangeReader.h`, `MergeTreeWhereOptimizer`): filter conditions are automatically moved to PREWHERE steps; the reader reads **only the condition's columns**, evaluates, accumulates a `final_filter`, drops fully-filtered granules (`rows_per_granule`, `collapseZeroTails`), then reads the remaining columns **only for surviving ranges**. If selectivity is poor it deliberately defers filtering since partial copies cost more than they save.
**Applicability: HIGH.** Evaluate predicate columns first, prune whole granules, then gather other columns through the surviving row set (run-list, not row-list, to keep copies bulk).

### 3c. Sparse primary index + granules + skip indexes
**Implementation** (paper §3.1–3.2): rows grouped into **granules of 8192**; the primary index stores PK values of each granule's first row only (~1000 entries per 8.1M rows, always in RAM); binary search → candidate granule ranges. **Marks** (`.mrk`, cached in the marks cache) map granule → (compressed block offset, offset within decompressed block) so granules are randomly addressable despite compression. Skip indexes per block of granules: `minmax`, `set(N)`, `bloom_filter`/tokenbf. **Adaptive granularity**: `index_granularity_bytes` (10MB default) shrinks a granule's row count for fat rows.
**Applicability: HIGH** for granule-level minmax pruning; MEDIUM for set/bloom; adaptive granularity LOW until wide rows exist.

### 3d. Query condition cache (25.3)
**Implementation** (https://clickhouse.com/blog/introducing-the-clickhouse-query-condition-cache; fed by `MergeTreeRangeReader::computeUnmatchedMarkRanges`): after any query runs a filter, it records **one bit per granule** (0 = no row matched). Subsequent queries with the same predicate — even structurally different queries — skip 0-granules outright. 100MB default holds bits for trillions of rows.
**Applicability: MEDIUM/HIGH.** Trivial over immutable segments (predicate-hash → bitset per segment; invalidation free — key by segment id). Great for dashboard-style repeated filters.

---

## 4. Processors / pipeline execution
**Implementation** (`src/Processors/Executors/PipelineExecutor.cpp`, `ExecutingGraph.h`; paper §4): operators are nodes with input/output **ports**. Each processor exposes `prepare()` (cheap status decision: `NeedData`, `PortFull`, `Ready`, `Async`, `Finished`) and `work()` (actual compute). Worker threads pull `Ready` nodes from a shared task queue; after `work()`, `updateNode` walks affected edges and enqueues newly-runnable neighbors. The plan is unfolded into N parallel **lanes** (N = cores) with repartition/exchange operators; pipeline breakers synchronize stages. Threads up/downscale lazily.
**Applicability: MEDIUM.** Full port-graph machinery is overkill at pintail's stage; a simpler **morsel-driven** design (parallel scan of granule ranges → thread-local partial aggregates → parallel merge) captures most of the benefit with 10% of the complexity. Adopt the *chunk* discipline (fixed ~64k-row typed batches through all operators) now.

---

## 5. JIT
**Implementation** (https://clickhouse.com/blog/clickhouse-just-in-time-compiler-jit; `src/Interpreters/JIT/`): LLVM-based; triggers after an expression/aggregation/sort-comparator is seen `min_count_to_compile_expression` = 3 times; compiles fused expression chains (`a*b + c + 1` → one function), fused multi-aggregate update loops, fused multi-column comparators; 5–15 ms compile, ~8KB/function, LRU-cached. Measured: **1.5–3× expressions (up to 20×), 1.15–2× aggregation, 1.15–1.5× ORDER BY**.
**Applicability: LOW.** The gains come from killing type-dispatch and virtual calls per row — which Rust monomorphized kernels + enum dispatch per *chunk* already achieve. Steal the *idea* (fuse per-row work across expressions; dispatch once per chunk), not the JIT.

---

## 6. FINAL / merge-on-read (most relevant to pintail)

### 6a. Split ranges into intersecting vs non-intersecting by PK — skip merging for most data
**Implementation** (`src/Processors/QueryPlan/PartsSplitter.cpp` — `splitPartsWithRangesByPrimaryKey`, `splitPartsRangesImpl`; shipped ~23.12): a **sweep line over PK values** of granule boundaries. Every mark range emits RangeStart/RangeEnd events with PK values from the sparse index; events sorted; while exactly one range is "open", that stretch is provably non-overlapping with any other part, and binary search on the index (`findRightmostMarkLessThanValueInRange` etc.) extracts maximal **non-intersecting mark ranges**. Those bypass the merge/dedup transform entirely and are read as plain parallel scans; only the (usually tiny) intersecting remainder goes through versioned merge. Level-0 (never-merged) parts are always treated as intersecting since they can self-duplicate.
**Why fast**: in a compacted LSM the overlap between segments is a sliver; FINAL becomes "almost free" — ClickHouse reports FINAL ≈ non-FINAL speed when duplicates are rare.

### 6b. Layered parallel merge of the intersecting remainder
Same file, `splitIntersectingPartsRangesIntoLayers`: intersecting ranges are carved into `max_layers` disjoint PK intervals of ~equal row count (min-heap over PK events, `rows_per_layer = total/max_layers`, recorded borders), each layer merged **by a different thread**, with `FilterSortedStreamByRange` enforcing borders. Even the merge-on-read path is parallel, not one k-way merge.

### 6c. `do_not_merge_across_partitions_select_final`
(`ReadFromMergeTree.cpp`; PRs [#15938](https://github.com/ClickHouse/ClickHouse/pull/15938), [#19375](https://github.com/ClickHouse/ClickHouse/pull/19375), [#96110](https://github.com/ClickHouse/ClickHouse/pull/96110)): if the partition key guarantees a logical row never spans partitions, dedup runs per-partition, and a partition consisting of a single level>0 part skips FINAL entirely.

**Applicability: HIGHEST.** Pintail merges on read *always*; this is the structural gap. In Rust: per query, take each segment's sparse index (first PK per granule), run the sweep to classify granule ranges; scan non-overlapping ranges with the fast vectorized path (no version compare, no heap), heap-merge only the overlapping slivers; parallelize slivers by PK layer. Keep per-segment PK min/max so fully disjoint segments classify in O(1). Also worth copying: vertical merge for compaction — merge sort-key columns first, record the row-permutation/survivor map, then apply it column-by-column; lower peak memory than horizontal merge.

---

## 7. Novel / less-known extras

- **Top-K threshold cutoff** (`src/Processors/Transforms/PartialSortingTransform.cpp`): for ORDER BY+LIMIT k, each chunk is partial-sorted to k rows, the current k-th row is kept as a **threshold row**, and every subsequent chunk is pre-filtered with a cheap columnar compare against the threshold before sorting. Streaming top-K collapses to a filter. **Applicability: HIGH, tiny effort.**
- **Sorting kernels** (`src/Columns/ColumnVector.cpp::getPermutation`): LSD **radix sort** for numeric permutations when 256 ≤ n ≤ u32::MAX, else branchless pdqsort; `limit==1` short-circuits to SIMD min/max index scan. **Applicability: MEDIUM/HIGH** (Rust: `rdst`/`radsort`; stdlib unstable sort is pdqsort).
- **Partitioned hash join** (`src/Interpreters/ConcurrentHashJoin.h`): build side scattered by key-hash into N independent HashJoins (per-slot mutex, one writer each); probe exploits **two-level layout** so thread t owns buckets {t, t+N, …} — merge after build is near-constant. Join reuses the whole key-specialization zoo from aggregation. **Applicability: HIGH** for the user×orders join.
- **Compression codec specialization**: `CompressionCodecT64` (transpose 64 values × 64 bits, drop all-zero bit-planes), `DoubleDelta` (delta-of-delta, timestamps), `Gorilla` (XOR floats), chainable with LZ4/ZSTD. Benchmarks: https://altinity.com/blog/2019-7-new-encodings-to-improve-clickhouse, https://clickhouse.com/blog/optimize-clickhouse-codecs-compression-schema. **Applicability: MEDIUM** — helps when I/O-bound; at 20M rows the workload is CPU-bound, so defer.
- **Projections**: per-part alternative physical orders / pre-aggregations, chosen automatically at query time; **marks cache** caches decoded granule→offset maps. Applicability: LOW/MEDIUM for now.

---

## Top 10 by expected payoff for the benchmark (20M rows: filtered counts, low-card string group-bys, join, top-K)

1. **Typed, key-specialized aggregation tables** (1a) — direct-array for narrow ints, u64 table, packed u128 composite, string table; do as part of the typed-columns migration.
2. **FINAL range splitting via PK sweep-line** (6a/6b) — merge-on-read paid only on overlapping slivers. Likely the single biggest structural win.
3. **Dictionary-code aggregation + per-code state cache** (1c/2c) — low-cardinality string group-bys become array indexing.
4. **Byte-mask SIMD filter with all-0/all-1 fast paths + popcount-only counts** (3a).
5. **PODArray-style padded, uninitialized buffers + overflow-tolerant memcpy** (2a) — force multiplier for every kernel.
6. **Thread-parallel aggregation with two-level tables and per-bucket parallel merge** (1e).
7. **Partitioned hash join with typed key tables** (§7).
8. **PREWHERE-style late materialization + granule minmax pruning** (3b/3c).
9. **StringHashTable length-classing** (1b).
10. **Top-K threshold prefilter + radix/branchless sort** (§7); consecutive-keys cache (1d) honorable mention at near-zero cost.

Key URLs: paper https://clickhouse.com/docs/academic_overview · sources: `src/Interpreters/AggregatedDataVariants.h`, `src/Common/HashTable/StringHashTable.h`, `src/Common/ColumnsHashing.h`, `src/Common/ColumnsHashingImpl.h`, `src/Common/PODArray.h`, `src/Columns/ColumnsCommon.cpp`, `src/Columns/ColumnVector.cpp`, `src/Columns/ColumnString.h`, `src/Columns/ColumnLowCardinality.h`, `src/Storages/MergeTree/MergeTreeRangeReader.h`, `src/Processors/QueryPlan/PartsSplitter.cpp`, `src/Processors/Executors/PipelineExecutor.cpp`, `src/Processors/Transforms/PartialSortingTransform.cpp`, `src/Interpreters/ConcurrentHashJoin.h` · JIT: https://clickhouse.com/blog/clickhouse-just-in-time-compiler-jit · query condition cache: https://clickhouse.com/blog/introducing-the-clickhouse-query-condition-cache · FINAL: https://github.com/ClickHouse/ClickHouse/pull/15938, https://github.com/ClickHouse/ClickHouse/pull/19375, https://kb.altinity.com/altinity-kb-queries-and-syntax/altinity-kb-final-clause-speed/
