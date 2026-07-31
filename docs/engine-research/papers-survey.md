# Literature survey: high-performance analytical query execution for pintail

All URLs verified to resolve (most PDFs fetched and read). Format per entry:
citation → mechanism → production use → pintail applicability.

---

## Part 1: Foundations (the sharp specifics)

### 1.1 MonetDB/X100: Hyper-Pipelining Query Execution
Boncz, Zukowski, Nes — CIDR 2005. https://www.cidrdb.org/cidr2005/papers/P19.pdf
Diagnoses why tuple-at-a-time engines get ~0.8 IPC and why full-column materialization is
bandwidth-bound. Fix: process cache-resident vectors (~100–1000 values) through type-specialized
primitives — interpretation amortizes, loops auto-vectorize, intermediates stay in L1/L2.
Introduces selection vectors. Production: Vectorwise, and by descent DuckDB, Velox, Photon,
DataFusion. **HIGH (foundational)** — the load-bearing detail is vector size tuned to L1/L2.

### 1.2 Everything You Always Wanted to Know About Compiled and Vectorized Queries
Kersten, Leis, Kemper, Neumann, Pavlo, Boncz — VLDB 2018. https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf
Both paradigms land within ~2× everywhere. Compiled is 74% faster on TPC-H Q1 (register-resident
intermediates); vectorized is 32% faster on Q9 (probe-bound: tight loop keeps many outstanding
loads). SIMD: 8.4× on a selection microbenchmark collapses to 1.4× on full Q6 — memory-bound.
Vector sweet spot ~1K tuples. Their join table stores a small Bloom tag in 16 unused pointer
bits. **HIGH (decision document)** — validates vectorized-only for pintail's benchmark shape;
don't expect SIMD miracles outside cache-resident kernels.

### 1.3 Morsel-Driven Parallelism
Leis, Boncz, Kemper, Neumann — SIGMOD 2014. https://db.in.tum.de/~leis/papers/morsels.pdf
~100K-tuple morsels; dispatcher hands NUMA-local morsels to pinned workers via lock-free queue;
work stealing absorbs skew; DOP can change mid-query. HyPer (morsel) 11.7× on 20 hyper-threads
vs VectorWise (exchange) 7.2×. Production: HyPer, Umbra, DuckDB, ClickHouse (partially).
**HIGH** — morsel-per-segment-range with work stealing from day one.

### 1.4 HyPer / Umbra compilation line
- Efficiently Compiling Efficient Query Plans — Neumann, VLDB 2011. https://www.vldb.org/pvldb/vol4/p539-neumann.pdf
- Adaptive Execution of Compiled Queries — Kohn, Leis, Neumann, ICDE 2018. https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf
- Tidy Tuples and Flying Start — Kersten, Leis, Neumann, VLDBJ 2021. https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf
- Evolution of a Compiling Query Engine — Neumann, VLDB 2021. https://www.vldb.org/pvldb/vol14/p3207-neumann.pdf
- Umbra: A Disk-Based System with In-Memory Performance — Neumann, Freitag, CIDR 2020. https://www.cidrdb.org/cidr2020/papers/p29-neumann-cidr20.pdf — buffer manager with
  **variable-size pages** (size classes 64KiB·2^i, mmap-reserved), versioned latches with
  optimistic lock-free reads, pointer swizzling.
**MEDIUM** — mostly a reason pintail *doesn't* need JIT. The variable-size-page buffer manager is
worth stealing if segments ever exceed RAM (dictionaries in single contiguous pages).

---

## Part 2: Adaptive / robust execution

### 2.1 Velox (Meta) — VLDB 2022. https://www.vldb.org/pvldb/vol15/p3372-pedreira.pdf
(a) **Conjunct reordering** — each AND/OR conjunct scored `time / (1 + rows_in − rows_out)`,
re-evaluated as batches stream (same machinery inside the file readers); (b) **dictionary peeling
+ memoization** — deterministic expressions evaluated only on distinct dictionary values;
dictionary wrappers double as zero-copy selection/permutation vectors; (c) **lazy vectors** —
columns materialize on first use; (d) runtime **dynamic filters** from join build sides pushed
into probe scans; (e) per-vector all-ASCII / no-null flags select specialized kernels.
**HIGH** — the scorer is a one-afternoon win; dictionary-as-runtime-encoding is the single best
architectural idea to copy into the typed arrays.

### 2.2 Photon (Databricks) — SIGMOD 2022 (best industry paper). https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf
Chose vectorized over compiled for build velocity, observability, adaptivity. Mechanisms:
**position lists, not byte masks** (tested; masks worse for all but trivial queries); every
kernel templated on `has_nulls × all_rows_active` with runtime dispatch from per-batch metadata;
sparse batches **compacted before hash probes**; hand-fused hot kernels recover most of the
compiled advantage. **HIGH** — closest architectural cousin; the 2×2 kernel matrix maps onto Rust
monomorphized generics.

### 2.3 Micro Adaptivity in Vectorwise — Răducanu, Boncz, Zukowski, SIGMOD 2013. https://15721.courses.cs.cmu.edu/spring2018/papers/03-compilation/p1231-raducanu.pdf
Multiple compiled "flavors" per primitive (branching vs branch-free, unrolling, compilers);
ε-greedy bandit picks per-invocation on measured cycles/tuple. Rankings flip with selectivity —
no statically best kernel. **HIGH** — trivially Rust-friendly. 2026 successor: Piece of CAKE
(Zhao, Marcus, https://arxiv.org/abs/2602.04181) — per-morsel contextual bandits, up to 2×.

### 2.4 POLAR — VLDB 2024. https://www.vldb.org/pvldb/vol17/p1350-justen.pdf (DuckDB writeup: https://duckdb.org/science/polar/)
Non-invasive alternative join orders for an existing pipeline; regret-bounded routing until a
"plan of least resistance" wins. Up to 9× improvement, <7% overhead. **MEDIUM** — relevant once
multi-join queries matter and the optimizer is young.

### 2.5 Others
- **Filter Representation in Vectorized Query Execution** — Ngom et al., DaMoN 2021. https://db.cs.cmu.edu/papers/2021/ngom-damon2021.pdf. SVs win at low/mid selectivity; bitmaps win for bulk
  boolean combining and AVX-512 masks. **Pintail policy: bitmaps while combining zone-map/
  predicate results, one conversion to selection vector for payload/aggregation.**
- Query Compilation Without Regrets — SIGMOD 2024. https://dl.acm.org/doi/10.1145/3654968; Dynamic Blocks — ADMS 2022. https://db.in.tum.de/~schmidt/papers/dynamic-blocks.pdf — low priority for a vectorized engine.
- PLAQUE — SIGMOD 2024. https://dl.acm.org/doi/10.1145/3639301 — learns implied predicates mid-query; adjacent.

---

## Part 3: Aggregation / join / hash tables

### 3.1 Unchained hash table — Birler, Schmidt, Fent, Neumann, DaMoN 2024. https://db.in.tum.de/~birler/papers/hashtable.pdf
Directory of 64-bit entries: 48 bits point into a contiguous tuple array *sorted by hash prefix*,
16 bits are a per-slot register-blocked Bloom filter (4 bits/tag; FPR ~1/169 at 65% fill). A
slot's matches are a contiguous range — no pointer chasing, duplicates don't pollute neighbors.
Probe hot path ~5 instructions; CRC32 hashing (~4× cheaper than xxh3). Parallel build:
thread-local bump collection → per-slot counts → prefix sum → parallel copy. 2× average over open
addressing; up to 20× over chaining on skew; near-flat probe cost to 0.001 selectivity.
Production: Umbra, CedarDB. **VERY HIGH — build this as pintail's one join table**; the embedded
Bloom bits double as the LIP/semi-join filter. ~A week in Rust.

### 3.2 LIP — Zhu, Potti, Saurabh, Patel, VLDB 2017. https://www.vldb.org/pvldb/vol10/p889-zhu.pdf
Build Bloom filters from *all* dimension build sides, apply all at the fact-table scan before any
join, adaptively reorder by observed drop rate. On SSB, all 24 join orders land within 0.1s of
each other. Production: DuckDB (since 1.2), ClickHouse, Spark/Photon, Umbra. **VERY HIGH** —
join-order robustness without an optimizer. Companion: **Performance-Optimal Filtering** — Lang,
Neumann, Kemper, Boncz, VLDB 2019. https://www.vldb.org/pvldb/vol12/p502-lang.pdf —
register-blocked, cache-sectorized Bloom filters; choose parameters by lookup-cost-vs-work-saved,
not space-optimal FPR.

### 3.3 Predicate transfer (Yannakakis-lite)
- Predicate Transfer — Yang, Zhao, Yu, Koutris, CIDR 2024. https://www.cidrdb.org/cidr2024/papers/p22-yang.pdf — pass Bloom filters along the join graph in both directions before executing
  any join. Avg 3.1× over Bloom join on TPC-H.
- I Can't Believe It's Not Yannakakis — Zhao et al., CIDR 2026. https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf — positional *bitmap* filters instead of Bloom: cheaper, precise.
- Parachute — Stoian et al., VLDB 2025. https://www.vldb.org/pvldb/vol18/p3299-stoian.pdf — precomputed join-induced "fingerprint" columns on FK tables; 1.54× end-to-end over DuckDB on JOB
  at 15% extra space. Precomputed auxiliary columns fit immutable segments unusually well.
**HIGH** — how a young engine beats ClickHouse on multi-join queries (its optimizer is the
documented weak spot).

### 3.4 Aggregation
- DuckDB parallel grouped aggregation — https://duckdb.org/2022/03/07/aggregate-hashtable; peer-reviewed: Robust External Hash Aggregation in the Solid State Age, ICDE 2024. https://duckdb.org/pdf/ICDE2024-kuiper-boncz-muehleisen-out-of-core.pdf. Thread-local tables (16-bit salt +
  payload pointer); radix partitioning only after 10K groups; resize rebuilds only the directory;
  **compressed materialization** (statistics-driven narrowing of columns entering sorts/aggs).
- **Global Hash Tables Strike Back!** — Xue, Marcus, PVLDB 2025. https://arxiv.org/abs/2505.04153. A purpose-built shared concurrent table (per-slot atomics, optimistic reads) matches or beats
  two-phase aggregation — especially at **low cardinality** (pintail's workload).
- Partial-aggregation abandonment — Velox/Presto monitor rows-in/rows-out of the partial agg and
  bypass hashing if reduction is poor. https://facebookincubator.github.io/velox/configs.html
- ClickHouse (VLDB 2024 paper): >30 hash-table variants; direct-index lookup tables for tiny key
  domains.
**VERY HIGH** — the winning ladder for low-cardinality group-bys: dictionary-code array-indexed
accumulators (no hashing) → global concurrent table → two-phase with the 10K trigger for the
high-cardinality tail.

### 3.5 Join consensus and the n:m tail
- Thirteen Relational Equi-Joins — Schuh, Chen, Dittrich, SIGMOD 2016. https://15721.courses.cs.cmu.edu/spring2018/papers/19-hashjoins/schuh-sigmod2016.pdf — counting output
  materialization and skew, simple no-partitioning chaining is the robust end-to-end winner.
- To Partition, or Not to Partition — Bandle, Giceva, Kemper, SIGMOD 2021. https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf — radix pays only when both sides are huge
  and pre-materialized. Baseline: Balkesen et al. http://www.vldb.org/pvldb/vol7/p85-balkesen.pdf
  **Consensus at 20M-row scale: unpartitioned global unchained table; don't build a radix join.**
- Diamond Hardened Joins — Birler, Kemper, Neumann, VLDB 2024. https://db.in.tum.de/people/sites/birler/papers/diamond.pdf — Lookup/Expand suboperators, shrink before expand; up to 500× on CE
  benchmark. Adaptive Factorization — Groß, ten Wolde, Boncz, CIDR 2025. https://vldb.org/cidrdb/papers/2025/p21-gro.pdf. Both MEDIUM: only if skewed n:m joins become a target.
- Negative result: Learned In-Memory Joins — Sabek, Kraska, VLDB 2023. https://www.vldb.org/pvldb/vol16/p1749-sabek.pdf — plain radix hashing still wins for hash joins. Skip learned
  partitioning.

---

## Part 4: Compression-aware execution

### 4.1 Data Blocks (HyPer) — Lang et al., SIGMOD 2016. https://db.in.tum.de/downloads/publications/datablocks.pdf
Immutable compressed blocks (≤2^16 records); per column-per-block: single-value,
**order-preserving dictionary** (range predicates run on codes), truncation (single-frame FOR).
**Positional SMAs**: per attribute a lookup table mapping value-byte → [begin,end) *position
range* within the block; ranges intersect across predicates before SIMD kernels run on compressed
codes. Production: HyPer/Tableau. **VERY HIGH** — the closest published blueprint to pintail's
immutable sorted segments; PSMAs are ideal on sorted-by-key segments.

### 4.2 FastLanes — Afroozeh, Boncz, VLDB 2023. https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf
Bit-packing/DELTA/FOR/RLE defined against a **virtual 1024-bit SIMD register** with a unified
transposed value layout, so plain scalar code auto-vectorizes to any ISA — no intrinsics. Delta
redefined against the value 128 positions back to stay data-parallel. >100 billion ints/sec
(~40× naive scalar). Follow-ups: FastLanes on GPU (DaMoN 2024); The FastLanes File Format (VLDB
2025, https://www.vldb.org/pvldb/vol18/p4629-afroozeh.pdf). **Rust crate:
https://github.com/spiraldb/fastlanes (used by Vortex).**
**VERY HIGH — arguably the single most actionable paper.** Overturns Data Blocks'
anti-bit-packing argument for pure OLAP scans; fixes pintail's vector size at 1024. Resolution:
FastLanes bit-packing for scan columns; byte-aligned ordered dictionaries where cheap positional
access is needed (late materialization of top-K/join payloads).

### 4.3 ALP — Afroozeh, Kuffó, Boncz, SIGMOD 2024. https://ir.cwi.nl/pub/33334/33334.pdf
Doubles encoded as round(n·10^e·10^−f) integers (exact round-trip verified; failures →
exceptions), then FOR + FastLanes bit-packing; exponents by two-level sampling so control flow is
per-vector. 1–2 orders of magnitude faster than Gorilla/Chimp/Patas at ~3.0× ratio. Production:
DuckDB default float codec since v0.10. **VERY HIGH if floats are stored** — ClickHouse's
Gorilla/DoubleDelta are sequential and non-vectorizable.

### 4.4 FSST — Boncz, Neumann, Leis, VLDB 2020. https://www.vldb.org/pvldb/vol13/p2649-boncz.pdf (code: https://github.com/cwida/fsst)
Static table of ≤255 symbols of 1–8 bytes; decompression is a branch-free code→8-byte-load loop
at LZ4-class speed, ~2× better ratio on string columns, **every string decompresses
independently** (random access). Equality predicates evaluate by compressing the constant.
Production: DuckDB, CedarDB/Umbra, Velox, Arrow, Polars, Vortex, Lance, BtrBlocks. **HIGH** —
use under a dictionary; random access fits merge-on-read late materialization.

### 4.5 BtrBlocks — Kuschewski et al., SIGMOD 2023. https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/btrblocks.pdf (code: https://github.com/maxi-k/btrblocks)
64K-value blocks; **sampling-based greedy selector picks a cascade** (depth ≤3) from 8
lightweight encodings. 2.2× faster scans than Parquet+Zstd. Thesis: drop general-purpose
compression layers; choose lightweight encodings per block. **HIGH** — the selection algorithm
pintail should run at segment-freeze time.

### 4.6 Executing on compressed/encoded data
- Integrating Compression and Execution — Abadi, Madden, Ferreira, SIGMOD 2006. https://www.cs.umd.edu/~abadi/papers/abadisigmod06.pdf — the foundational compressed-block API (operators
  branch on block *properties*, not codecs); aggregate RLE by value×run-length; join on
  dictionary codes.
- Velox encoding-wrapped vectors (§2.1); DuckDB compressed materialization (ICDE 2024).
- **Small Materialized Aggregates** — Moerkotte, VLDB 1998. http://www.vldb.org/conf/1998/p476.pdf — origin of zone maps; SMAs can *answer* count/sum from metadata when a block is fully
  covered by the predicate — a fast path pintail's filtered counts should have.
**VERY HIGH** for low-cardinality group-by: aggregate by dictionary code with array-indexed
accumulators; no hash table at all.

---

## Part 5: Sorting / top-K

### 5.1 DuckDB sorting line
- These Rows Are Made for Sorting — Kuiper, Mühleisen, ICDE 2023. https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf — row format beats columnar for sort; normalized
  memcmp-able keys; radix sort; Merge Path parallel merges.
- DuckDB 1.4 redesign (2025) — https://duckdb.org/2025/09/24/sorting-again — single-pass k-way merge; run formation **adaptive to presortedness** (Vergesort detection → Ska radix → pdqsort).
  2.7× on 1B random rows, **10.4× on presorted data**.
- Theory: Efficient Sorting, Duplicate Removal, Grouping, and Aggregation — Do, Graefe, Naughton,
  TODS 2022. https://arxiv.org/pdf/2010.00152 — aggregate *during* run generation in the
  tree-of-losers; **offset-value coding** turns most comparisons into single integer compares.
  Plus offset-value codes for modifying sort orders — EDBT 2025. https://openproceedings.org/2025/conf/edbt/paper-79.pdf
- SIMD kernels: Origami — Arman, Loguinov, VLDB 2022. https://www.vldb.org/pvldb/vol15/p259-arman.pdf
**VERY HIGH** — segments are already key-sorted: ORDER BY key is a k-way OVC merge with no sort;
merge-on-read dedup is the same loop with a keep-latest-version reducer.

### 5.2 Top-K pushdown
- ClickHouse granule-level top-N skipping — https://clickhouse.com/blog/clickhouse-top-n-queries-granule-level-data-skipping — the current N-th heap value is a dynamic threshold pushed into the
  scan, tightening mid-query (10×, 9.42GB → 520MB read); composes with read-in-order and lazy
  materialization.
- Pruning in Snowflake — Zimmerer et al., SIGMOD 2025. https://arxiv.org/abs/2504.11540 — metadata pruning extended to LIMIT, top-K, and JOIN pruning; fleet-wide, pruning eliminates
  99.4% of micro-partitions.
**VERY HIGH** — implement the trio: dynamic cutoff → SMA pruning; re-pruning as the cutoff
tightens; lazy materialization + version resolution only for the K winners. Recent enough in
ClickHouse that doing it uniformly is a genuine edge.

---

## Part 6: Scan sharing / result caching / MV-lite

- Cooperative Scans — Zukowski et al., VLDB 2007. https://www.vldb.org/conf/2007/papers/research/p723-zukowski.pdf; Main-Memory Scan Sharing — Qiao et al. (IBM Blink), PVLDB 2008. http://www.vldb.org/pvldb/vol1/1453924.pdf. MEDIUM: matters when dashboard concurrency is the
  bottleneck.
- Recycling intermediates: MonetDB recycler — SIGMOD 2009. https://ir.cwi.nl/pub/12225; Recycling in Pipelined Query Evaluation — ICDE 2013. https://ir.cwi.nl/pub/21352; **HashStash** —
  Dursun et al., SIGMOD 2017. https://cs.brown.edu/~kayhan/papers/hashstash.pdf — cache the hash
  tables joins/aggregations already built, with subsumption and incremental updates; ~2×. HIGH: a
  dashboard group-by re-run 30s later with only new CDC segments is the canonical case.
- **Krypton (ByteDance)** — VLDB 2023. https://www.vldb.org/pvldb/vol16/p3528-chen.pdf — result cache whose **key includes the data version**, self-invalidating as data lands. **VERY HIGH** —
  pintail's versioned immutable segments make `(normalized plan, max segment version per table)`
  a *correct* result-cache key under CDC; ClickHouse's query cache is deliberately
  transactionally inconsistent (TTL 60s), DuckDB has none.
- **Predicate Caching (Redshift)** — Schmidt et al., SIGMOD 2024. https://www.amazon.science/publications/predicate-caching-query-driven-secondary-indexing-for-cloud-data-warehouses — cache
  per (table, predicate) the qualifying block ranges discovered during a scan. ClickHouse shipped
  the same idea as the query condition cache (2025). **VERY HIGH: on immutable segments these
  bitmaps are immortal — the cheapest big win for repeated filtered counts.**
- Napa (Google) — VLDB 2021. http://www.vldb.org/pvldb/vol14/p2986-sankaranarayanan.pdf — MV maintenance folded into LSM compaction. MEDIUM/longer-term.
- ParCuR — arXiv 2023. https://arxiv.org/abs/2307.08018; GraftDB — arXiv 2026. https://arxiv.org/abs/2606.04303. LOW-MEDIUM.

---

## Part 7: 2023–2026 novelty for a single-node Rust engine

- **Portable SIMD**: Evaluating SIMD Compiler-Intrinsics for Database Systems — Benson, Ebeling,
  Rabl, ADMS 2023. https://ceur-ws.org/Vol-3462/ADMS5.pdf — compiler vector extensions matched or
  beat platform intrinsics in DB kernels; they deleted Velox's xsimd layer with no loss.
  **Verdict: `portable_simd`-first, drop to `core::arch` only for compress-store gaps.** Related:
  TSL generator https://arxiv.org/pdf/2407.18728, Google Highway.
- Vectorized hash tables across ISAs — Böther et al., VLDB 2023. https://www.vldb.org/pvldb/vol16/p2755-bother.pdf — Swiss-table-family SIMD probing wins broadly; calibrates when a custom
  table beats hashbrown.
- NVMe/io_uring: What Modern NVMe Storage Can Do — Haas, Leis, VLDB 2023. https://vldb.org/pvldb/vol16/p2090-haas.pdf (12.5M IOPS needs thousands of outstanding requests; per-core queues);
  High-Performance DBMSs with io_uring — Jasny et al., arXiv 2025. https://arxiv.org/abs/2512.04859 (wins come from registered buffers, NVMe passthrough, SQPOLL, completion batching);
  Spilling without Killing Performance — SIGMOD 2025. https://zenodo.org/records/14843332;
  Saving Private Hash Join — VLDB 2025. https://www.vldb.org/pvldb/vol18/p2748-kuiper.pdf.
  MEDIUM until segments exceed RAM; then HIGH.
- Competition papers: **ClickHouse VLDB 2024** — https://www.vldb.org/pvldb/vol17/p3731-schulze.pdf. Key intel: 8192-row granules; sparse PK index with monotonicity-aware preimage rewriting;
  >30 hash-table variants; repeat-triggered LLVM JIT; and **merge-on-read costs them a per-query
  tax** (lightweight deletes filter a bitmap into every SELECT; ReplacingMergeTree needs FINAL or
  tolerates duplicates) — pintail's sorted-segment positional merge dedup can be cheap by
  construction. **DataFusion SIGMOD 2024** — https://andrew.nerdnetworks.org/pdf/SIGMOD-2024-lamb.pdf — async Streams on Tokio are adequate for scheduling; DataFusion loses to DuckDB on
  high-cardinality group-bys and join order — the last 2–5× lives in the aggregation table, join
  robustness, scan pushdown.
- GPU awareness only: Sirius — CIDR 2026. https://vldb.org/cidrdb/papers/2026/p12-yogatama.pdf; Theseus — https://arxiv.org/abs/2508.05029.

---

## The 8 highest-leverage ideas for beating ClickHouse single-node

1. **Unchained hash table with embedded Bloom tags** (DaMoN 2024) — one table for join and
   high-cardinality group-by; tags give semi-join filtering free.
2. **FastLanes transposed layout for packed arrays** (VLDB 2023 + Rust crate) — portable-SIMD
   decompression from safe Rust; sets vector size (1024); scan compressed by default.
3. **Ordered dictionaries + execution on codes + PSMAs** (Data Blocks, Abadi 2006, Velox) —
   low-cardinality group-by = array-indexed accumulation over codes; range predicates on codes;
   count-from-metadata for fully-covered filters.
4. **Predicate/condition caching + version-keyed result cache** (Redshift 2024, Krypton 2023) —
   immortal on immutable segments; correct under CDC where ClickHouse's cache is approximate.
5. **Dynamic top-K cutoff pushed into SMA pruning + lazy materialization** (Snowflake 2025;
   ClickHouse 2025–26).
6. **LIP / predicate transfer with performance-optimal Bloom or bitmap filters** (VLDB 2017/2019;
   CIDR 2024/2026) — join-order robustness, neutralizing the optimizer maturity gap.
7. **Velox/Photon-style micro-adaptivity** (VLDB 2022, SIGMOD 2022, SIGMOD 2013) — conjunct
   scoring, per-batch kernel dispatch, sparse-batch compaction, ε-greedy flavors. Individually
   small, multiplicative together.
8. **Presortedness-adaptive k-way merge with offset-value coding** (DuckDB 2025; Graefe TODS
   2022) — ORDER BY/dedup/merge-on-read reduce to one OVC-accelerated k-way merge; makes the
   versioned-dedup path a strength.

Two cross-cutting decisions the literature settles: **stay vectorized, skip JIT** (Kersten 2018
bounds the loss; Photon's reasons apply doubly to a small team); **write kernels in
`portable_simd`/autovectorizable scalar style** (Benson ADMS 2023, FastLanes), not per-ISA
intrinsics.
