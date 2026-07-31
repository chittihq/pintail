# DuckDB internals: extraction report for pintail

Sources: DuckDB blog/docs, DuckDB source (file paths verified), CWI papers. Format per technique:
implementation → why fast → applicability to pintail.

---

## 1. Unified vector format

**1a. Four physical vector representations behind one logical type.**
- **How**: Every `Vector` is logically an array of 2048 values (`STANDARD_VECTOR_SIZE`), but physically one of: **Flat** (contiguous array), **Constant** (one value), **Dictionary** (child vector + selection vector of indices), **Sequence** (offset + increment). Docs: https://duckdb.org/docs/stable/internals/vector.html. Source: `src/include/duckdb/common/types/vector.hpp`, `src/common/types/vector.cpp`.
- Operators that don't specialize call `Vector::ToUnifiedFormat()` → `{data pointer, selection vector, validity mask}` — a flat vector gets an identity selection vector that compiles to a no-op indirection. Hot operators specialize on flat/constant/dictionary combos via templated dispatch (`BinaryExecutor`, `TernaryExecutor` in `src/include/duckdb/common/vector_operations/`).
- **Why fast**: constant vectors make `col + 5` touch the literal once; dictionary vectors let dictionary-compressed storage flow into execution *without decompression* (group-by on a dictionary vector can hash 200 dictionary entries instead of 2048 rows); selection vectors make filters zero-copy — a filter emits a selection vector over the same buffers instead of materializing survivors.
- **Applicability: HIGH.** Bake in from day one of the typed-array migration: a Rust enum `{Flat, Constant, Dict{codes, values}, Sequence}` + a `SelectionVector` + `unified()` accessor. Selection vectors alone transform filtered-count and filter→group-by pipelines: never materialize post-filter columns.

**1b. Validity masks as bitmasks with an "all valid" fast path.**
- **How**: `ValidityMask` (`src/include/duckdb/common/types/validity_mask.hpp`) is a 64-bit-word bitmask; a null internal pointer means "no nulls anywhere," and every kernel checks `mask.AllValid()` once per vector and takes a null-free loop.
- **Applicability: HIGH.** In Rust: `Option<Box<[u64]>>` per array; kernels branch once per batch, not per row.

**1c. German strings (`string_t`): 16-byte fixed string with 4-byte prefix.**
- **How**: `src/include/duckdb/common/types/string_type.hpp`. Union of two 16-byte layouts: `{u32 len, char inlined[12]}` for len ≤ 12, else `{u32 len, char prefix[4], char *ptr}`. Comparison loads the first 16 bytes of both and compares length+prefix first; only on tie does it chase the pointer. Long-string bytes live in a per-vector `StringHeap`.
- **Why fast**: most real strings fit in 12 bytes → zero pointer chasing; ~99% of inequality comparisons resolve on the 4-byte prefix; fixed 16-byte stride keeps string columns SIMD/cache-friendly and lets strings ride through row layouts used by sort/join/aggregate.
- **Applicability: HIGH.** Directly portable (16-byte `#[repr(C)]` union; Arrow `StringView` is the same idea). Careful with lifetimes: tie views to the segment/heap they point into.

---

## 2. Push-based pipeline execution

- **How**: In 2021 DuckDB switched from pull (Volcano) to push (issue: https://github.com/duckdb/duckdb/issues/1583, talk: https://dsdsd.da.cwi.nl/past_talks/duckdb-push-based-execution/). The physical plan is cut at pipeline breakers into a DAG of **Pipelines**, each = one **Source** + N streaming **Operators** + one **Sink**. `PhysicalOperator::BuildPipelines()` decomposes; `src/parallel/pipeline.cpp`, `pipeline_executor.cpp`, `executor.cpp`.
- Interfaces: Source `GetData(GlobalSourceState, LocalSourceState)`; Operator `Execute(input→output chunk)`; Sink `Sink(chunk, GlobalSinkState, LocalSinkState)` + `Combine(local→global)` + `Finalize()`.
- **Morsel-driven parallelism**: a pipeline is instantiated as K `ExecutorTask`s; each pulls a morsel (~122,880 rows) from the shared source state (one atomic fetch-add), pushes it through its *private* operator/local-sink states, and only touches shared state in `Combine`/`Finalize`. Event-based scheduling unlocks dependent pipelines.
- **Why fast**: (1) parallelism is a scheduler property, not an operator property; (2) thread-local sink states mean *no locks or atomics on the hot path*; (3) morsel stealing gives load balance on skewed filters; (4) explicit control flow enables suspend/resume and backpressure.
- **Applicability: HIGH — the architecture decision that makes everything else compose.** In Rust: `trait Sink { type Local: Send; fn sink(&self, local, chunk); fn combine(&self, local); fn finalize(&self) }` with rayon or a small work queue; ownership makes local-vs-global state safe by construction. Immutable sorted segments are natural morsels.

---

## 3. Parallel hash aggregation (radix-partitioned, two-phase)

- **How**: Blogs https://duckdb.org/2022/03/07/aggregate-hashtable.html and https://duckdb.org/2024/03/29/external-aggregation. Source: `src/execution/aggregate_hashtable.cpp` (`GroupedAggregateHashTable`), `src/execution/radix_partitioned_hashtable.cpp`, `src/common/types/row/tuple_data_collection.cpp`.
- **Layout**: (1) dense **pointer table** of 8-byte entries = row pointer + **salt** (upper hash bits) — probe compares salt before dereferencing; linear probing. (2) **Payload blocks** holding rows of `[groups | hash | aggregate states]`. Resize = throw away pointer table, re-insert from payload pages using the *stored* hash (no rehash, no payload movement).
- **Two phases**: Phase 1, each thread owns a small thread-local HT; once it exceeds ~10k entries (or memory pressure), its payload is flushed into **radix partitions** by high hash bits (16–256 partitions ≫ threads). Zero cross-thread communication. Phase 2, partitions distributed to threads; each merges/finalizes its partitions into an independent final HT — no locks, no giant global table. Same-hash ⇒ same-partition guarantees correctness. Spilling falls out free: partitions are buffer-managed pages.
- **Why fast**: thread-local phase 1 keeps HTs cache-sized; salt turns most probe misses into one L1 compare; partition-parallel merge scales; resize is O(pointer table).
- **Applicability: HIGH.** Per-thread flat table of `(salt, ptr)` + arena-allocated payload rows, partition merge at end. At very low cardinality also steal the implicit special case: thread-local tables never partition and merge is trivial. Dictionary vectors compound: group-by on a dictionary-encoded column aggregates per code with a plain array, no hashing.

---

## 4. Hash join

- **How** (source: `src/execution/join_hashtable.cpp`, `src/execution/operator/join/physical_hash_join.cpp`; walkthrough: https://deepwiki.com/duckdb/duckdb/8.1-hash-join-implementation):
  - **Build**: threads append build rows to a `RadixPartitionedTupleData` (row-format pages, no locks). `Finalize` sizes one global pointer table and inserts all rows: 64-bit entries = **47-bit row pointer + 16-bit salt**, linear probing, collisions chained via a `next` pointer *embedded in the row*.
  - **Probe (vectorized)**: hash a whole vector of probe keys → gather candidate entries → salt-compare filters to a selection vector → full key compare on survivors → follow embedded chains only for still-unmatched rows. All vector-at-a-time.
  - **Perfect hash join**: `src/execution/perfect_hash_join_executor.cpp`. Build computes key min/max; if the domain is small/dense enough it builds a *direct-address array* (`idx = key - min`) — no hashing, no probing. Since v1.2 decided *dynamically after build* (PR https://github.com/duckdb/duckdb/pull/14971).
  - **Join filter pushdown (the gem)**: after build, DuckDB pushes `probe_key BETWEEN min AND max` as a **table filter into the probe-side scan** — evaluated against zone maps, so whole row groups are skipped before reaching the join. 1 key → equality filter; ≤ ~50 distinct keys → IN-list. Blog: https://duckdb.org/2024/11/14/optimizers (~10× on selective joins); `src/optimizer/join_filter_pushdown_optimizer.cpp`. Extended toward Bloom-style dynamic filters ("Saving Private Hash Join", VLDB 2025: https://www.vldb.org/pvldb/vol18/p2748-kuiper.pdf).
  - **Out-of-core fallback**: build rows already live in radix partitions on buffer-managed pages → spill some partitions, probe resident ones, recurse.
- **Why fast**: one cache miss per probe hit (salt + embedded chain), fully vectorized probe, perfect-hash path eliminates the table for dim-table integer keys, join filter pushdown converts join selectivity into *scan* pruning.
- **Applicability: HIGH.** (1) Join filter pushdown is cheap (per-segment min/max already exists — the filter can skip whole segments); (2) perfect hash join for small int dim keys is ~50 lines; (3) 47-bit pointer + 16-bit salt packing works identically in Rust (arena + tagged u64). Out-of-core: LOW priority at 20M rows.

---

## 5. Sorting and top-K

- **How** (blog: https://duckdb.org/2021/08/27/external-sorting; paper: "These Rows Are Made for Sorting…", ICDE 2023; source: `src/common/sort/`):
  - **Normalized (memcmp-comparable) keys**: all ORDER BY columns re-encoded into one fixed-width big-endian byte string — sign bit flipped for signed ints, doubles order-encoded, bits inverted for DESC, null byte prepended, strings truncated to a fixed prefix (ties → fallback comparator). Multi-column comparison becomes one `memcmp`.
  - **Row layout**: keys and payload converted to fixed-width rows (variable-length data in heap blocks), so reordering the payload is sequential row copies. Pointer swizzling makes blocks spillable.
  - **Algorithm**: per-thread LSD radix sort on the normalized key; parallel cascaded two-way merges using **Merge Path**; branchless merge loop.
  - **Top-K** (`src/execution/operator/order/physical_top_n.cpp`): per-thread bounded heaps on the normalized-key encoding, plus a dynamic **boundary value** — once a heap is full, its current worst key pre-filters incoming chunks vector-at-a-time (most chunks discarded with one comparison per row or skipped wholesale); local heaps merged at Combine. Heap improvements: https://github.com/duckdb/duckdb/pull/14900. Top-N aggregates: https://duckdb.org/2024/10/25/topn.
- **Applicability: HIGH for top-K, MEDIUM for full sort.** Implement normalized-key encoding + per-thread heaps + boundary short-circuit. Reuse the same key encoding for merge-on-read version resolution and sorted-segment merges — *one* row/key format serves sort, window, and top-K.

---

## 6. Storage: row groups, adaptive lightweight compression, zone maps

- **How** (blog: https://duckdb.org/2022/10/28/lightweight-compression.html; docs: https://duckdb.org/docs/stable/internals/storage; source: `src/storage/compression/`):
  - Tables split into **row groups** of 122,880 rows; column segments on fixed 256KB blocks.
  - **Two-phase adaptive selection at checkpoint**: each candidate compression scheme runs an **Analyze** pass over the actual data; best scorer wins (bias multiplier favoring cheap-to-decompress schemes); then a **Compress** pass. Schemes: Constant, RLE, bit-packing (per-1024-value width), Frame-of-Reference, dictionary, **FSST** (repeated substrings within strings), **ALP** for floats (replaced Chimp/Patas in v0.10: exact double→scaled-integer + FOR/bit-packing with exception patches; 1–2 orders of magnitude faster than Gorilla-family; paper: https://ir.cwi.nl/pub/33334/33334.pdf).
  - Choices are per-column-per-row-group. **Zone maps** (min/max per segment) stored for all columns, drive table-filter pruning — including the *dynamic* join filters from §4.
- **Why fast**: lightweight schemes decode at memory bandwidth or don't decode at all (dictionary/constant segments become dictionary/constant *vectors*), unlike zstd blocks forcing 256KB decompression for any access.
- **Applicability: HIGH (structure).** Immutable sorted segments make this easier: compress once at segment write. Priority: (1) zone maps per segment+column; (2) dictionary encoding flowing into execution as dictionary arrays; (3) bit-packing/FOR for ints and timestamps; (4) analyze-then-choose framework; FSST and ALP later (Rust crates exist: `fsst`, https://lib.rs/crates/alp).

---

## 7. Less-known gems

- **Statistics propagation through the plan** (`src/optimizer/statistics_propagator/`; https://duckdb.org/2024/11/14/optimizers): base-table min/max/null/distinct stats propagate *through* every operator at plan time — after a filter stats tighten; through a join they intersect, and DuckDB **manufactures new filters** (t1.a ∈ [25,50] joined to t2.a ⇒ inject `t2.a BETWEEN 25 AND 50`), proves comparisons always-true/false, downgrades sort widths. **Applicability: MEDIUM-HIGH.**
- **Adaptive filter reordering at runtime** (`src/execution/adaptive_filter.cpp`): for conjunctions, maintains a permutation of clauses; periodically swaps an adjacent pair, measures runtime, keeps if faster, halves that pair's retry likelihood if not. No cost model; converges to cheapest-and-most-selective-first; adapts to drift within a scan. **Applicability: MEDIUM** — ~150 lines.
- **Join order with (almost) no statistics** (Tom Ebergen MSc thesis: https://homepages.cwi.nl/~boncz/msc/2022-TomEbergen.pdf; `src/optimizer/join_order/`): assume every equi-join is PK-FK, estimate from HyperLogLog distinct counts, DPhyp-style DP with greedy fallback. You don't need histograms. **Applicability: LOW-MEDIUM now, HIGH later.** Cheap first step: HLL or exact distinct count per segment column.
- **FSST inside execution**: equality comparisons can run on compressed strings (compress the probe constant once) — FSST paper (Boncz/Neumann/Leis, VLDB 2020). **Applicability: LOW now.**
- **ART index restraint**: DuckDB has ART (`src/execution/index/art/`) but uses it almost only for PK/unique constraints; deleted index joins entirely. Lesson: **don't build secondary indexes; build zone maps + scan speed.** Sorted segments are the index.
- **Unified buffer manager for temp + persistent data**: spill structures live on the same fixed-size buffer-managed pages as table data — "out of core" is not a mode, it's LRU eviction. **Applicability: LOW now, HIGH architecturally.**

---

## Top 10 for pintail's benchmark shape

1. **Selection vectors + flat/constant/dict vector representations** (§1a) — foundation; filters become zero-copy.
2. **Morsel-driven push pipelines with Local/Global sink state** (§2) — lock-free parallel group-by/join/top-K.
3. **Zone maps per segment + table-filter pushdown into scans** (§6).
4. **Join filter pushdown (dynamic build-side min/max → probe scan filter)** (§4) — ~10× on selective joins; nearly free once #3 exists.
5. **Radix-partitioned two-phase aggregation with salt+pointer table and payload pages** (§3).
6. **German strings** (§1c).
7. **Vectorized probe with 47-bit-pointer+16-bit-salt entries and row-embedded chains** (§4).
8. **Top-K via per-thread heaps over normalized memcmp keys + boundary-value pre-filtering** (§5).
9. **Dictionary encoding that survives into execution** (§6+§1a).
10. **Perfect hash join for dense integer build keys, decided after build** (§4).

Honorable mentions: adaptive filter reordering (§7); normalized sort keys reused for merge-on-read version resolution (§5); analyze-then-compress framework (§6).

Key URLs: vectors https://duckdb.org/docs/stable/internals/vector.html · sorting https://duckdb.org/2021/08/27/external-sorting · aggregation https://duckdb.org/2022/03/07/aggregate-hashtable + https://duckdb.org/2024/03/29/external-aggregation · compression https://duckdb.org/2022/10/28/lightweight-compression.html · optimizers https://duckdb.org/2024/11/14/optimizers · push-based execution https://github.com/duckdb/duckdb/issues/1583 · Saving Private Hash Join https://www.vldb.org/pvldb/vol18/p2748-kuiper.pdf · ALP https://dl.acm.org/doi/10.1145/3626717 · join-order thesis https://homepages.cwi.nl/~boncz/msc/2022-TomEbergen.pdf · hash-join walkthrough https://deepwiki.com/duckdb/duckdb/8.1-hash-join-implementation
