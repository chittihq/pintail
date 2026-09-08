# Allocation layout investigation

Research and isolated experiments, 2026-09-07. Engine changes are not included.
The measured engine type comes from the archived commit in `BASE_COMMIT`;
concurrent edits in the working checkout were not part of the experiment.

## What the research suggests

These are sources of hypotheses, not performance predictions for Pintail.

| Reading | Transferable technique | Pintail application and disposition |
| --- | --- | --- |
| [DNS cache memory optimization](https://blog.cloudflare.com/dns-cache-memory-optimization-1111/) | Freeze immutable containers, combine small allocations, infer redundant fields from context, measure enum layout, and pack variable data. The article reports higher insertion throughput and lower lookup latency as well as memory savings. | Test row ownership and string storage after join build / memo insertion. Do not replace mutable ingestion buffers with fixed slices. No expectation that its fleet-wide savings transfer. |
| [Reducing memory indirections](https://abseil.io/fast/83) | Target frequent small allocations with related lifetimes; removing pointer hops can reduce cache misses. | Flatten row payloads within one operator lifetime. Keep hash lookup and row ownership separate. This is the first measured candidate below. |
| [Identifying and reducing memory bandwidth needs](https://abseil.io/fast/62) | Read fewer bytes and reuse data before it leaves cache. | Preserve the existing packed scan and aggregate paths. Prototype compact cells only in the remaining materialized row paths. |
| [A good day to trie-hard](https://blog.cloudflare.com/pingora-saving-compute-1-percent-at-a-time/) | Optimize the measured common case; its example exploits frequent negative lookups and contiguous metadata, then checks production profiles. | Explore early rejection for long string join keys only if profiles show negative probes dominate. Existing runtime join filters must be the baseline. A trie is not automatically suitable for general SQL joins. |
| [SIEVE cache eviction](https://www.usenix.org/publications/loginonline/sieve-cache-eviction-can-be-simple-effective-and-scalable) | Lazy hit metadata updates can avoid work and improve cache scalability; effectiveness depends on request reuse patterns. | `ReplicaCache::lookup` takes a mutex and updates `last_used`. Measure contention before changing eviction: this cache stores databases, not billions of tiny objects. Replacing eviction alone would not remove the lookup mutex or stamp checks. Deferred. |
| [Beware microbenchmarks bearing gifts](https://abseil.io/fast/39) | A local speedup may fail to improve the application because cache, code footprint, and surrounding work differ. | Treat the measurements below as mechanism evidence. Require actual SQL throughput, memory, and correctness experiments before engine adoption. |

## Repository evolution and issue mapping

The July engine-research reports and existing experiments already investigate typed
execution, caching, buffer recycling, and compression. Repeating those proposals
without checking adoption would not identify new work.

- `0486c3d` retains packed predicate survivors; `d6cf654` folds packed aggregate
  lanes; `1385555` batches the dense aggregate window. Keep these fast paths.
- `607c855` finalizes join builds into dense tables. The current
  [join payload](../../crates/pintail-exec/src/execution/join.rs) still contains
  `HashMap<JoinHashKey, Vec<Vec<Value>>>`. The experiment isolates payload
  storage, not hash-table design, partition selection, or the whole join.
- `81f0787` added dependent subquery memoization. Its
  [memo entries](../../crates/pintail-exec/src/execution/memo.rs) still own
  vector keys and results. Frozen payloads are a possible second target, with
  the existing query budget and volatile-expression exclusions preserved.
- [#12](https://github.com/chittihq/pintail/issues/12) is closed: bounded
  spilling exists. Recent spill changes include `4cdacd2`, `95ace90`, and
  `be93b79`. Compact payloads should reduce pressure on those paths, not
  bypass reservation, disk quotas, or skew handling.
- [#27](https://github.com/chittihq/pintail/issues/27) is closed: adaptive
  per-block compression was adopted after cross-target evidence. Global
  compression removal is not a new recommendation.
- [#34](https://github.com/chittihq/pintail/issues/34) is closed: cache
  eligibility across metadata bookkeeping was corrected. Preserve freshness
  semantics in any future cache optimization.
- [#31](https://github.com/chittihq/pintail/issues/31) remains open: the timed
  benchmark needs a live update tail. Static or in-process measurements cannot
  settle throughput under CDC. This is a prerequisite for a live-tail claim.
- Other open issues are #7 (local writable mode), #28 (query privacy), and #29
  (repair of CDC-invisible actions). None supplies evidence that row arenas
  improve those features. Do not expand this experiment into those projects.

The older `e15-value-tax` duplicates a pre-ENUM `Value` definition. This lab
imports the real `pintail_types::Value`. Measured on the pinned compiler,
`Value` and `FrozenValue` are both 32 bytes: adding a variant did not imply a
larger enum, and removing string capacity did not imply a smaller cell.
Only the arena cell is 24 bytes. These are measured layouts, not Rust ABI
promises. This rejects a blanket "box strings to shrink Value" argument.

## Experiment design

Five arms form an ablation:

1. `nested`: `Vec<Vec<Value>>`, cloning exact-length input rows. No deliberately
   inflated capacity or obsolete stand-in type makes the baseline look bad.
2. `boxed`: fixed row slices and a fixed outer slice. Same scalar type and
   per-row allocation count; isolate container-header savings.
3. `flat`: one fixed cell array, row start computed from the common width.
   Same scalar type and string allocations; isolate row indirection removal.
4. `compact`: flat cells with owned boxed text/bytes. A deliberately testable
   hypothesis about immutable scalar layout; includes ENUM ordinals.
5. `arena`: 24-byte cells and one byte arena. Strings/bytes use checked u32
   offsets and lengths, with a separate ENUM ordinal. No deduplication,
   dictionary encoding, lossy conversion, or altered hash semantics.

All arms start from identical input. Build timing includes copying and any
pre-sizing pass, allocations, and freezing. The arena has a 4 GiB byte limit;
a real implementation would need bounded slabs. Fixed-width rows here mean a
fixed column count, not fixed string lengths. Variable column counts would
need another row index and are not covered.

Four invented distributions contain 131,072 rows each: four numeric columns,
sixteen numeric columns, eight columns with 25% short variable payloads, and
eight columns with 75% long variable payloads. NULLs occur independently
before the text selection; actual payload proportions are consequently a
little smaller. Text lengths vary by 0–16 bytes. Types include numeric,
boolean, binary, UTF-8, and ENUM. The unit round-trip also covers empty
payloads, non-ASCII text, embedded NUL, signed/unsigned extrema, NaN, and -0.

Each run checks every stored cell against the source before timed reads;
checksums alone are not the correctness oracle. A batch performs 262,144
seeded random row accesses and consumes every cell through a numeric/token
reduction. For text it reads length and endpoint bytes: this is **not** a SQL
collation comparison or a full string hash. One warmup precedes seven timed
batches. One/four-worker arms partition the same total work; worker creation
is included. These are row probes, not independent SQL queries.

There are three fresh processes per case/worker/arm, shuffled with seed 8107.
Processes run sequentially with CPU affinity; only workers within a process
run concurrently. Build, destruction, and three single-worker materialization
passes are timed separately. Each materialization copies 16,384 selected
rows back to ordinary `Value` rows and checks its token reduction. Output
allocation is included; output destruction is outside that timer. Full-cell
storage round-trip checks establish lossless representation independently.

Memory is an exact census of requested **retained payload capacity** and
live nonempty heap buffers, calculated from the final structures. It excludes
allocator size-class rounding, allocator metadata, stacks, source fixture,
probe indices, hash tables, and transient growth. It is not an instrumented
allocation-call count. `fixture_and_build_peak_kib` is measured Linux VmHWM
before verification; it includes the source fixture and intermediate build
allocations and must not be called engine RSS or steady-state memory.

The native Linux build server ran this lab; no Docker measurement host was
used. The process had one/four-CPU affinity on a 32-logical-CPU machine.
The machine was not reserved exclusively. Rust 1.97.0, release + thin LTO,
and the default System allocator were used. Pintail's shipped binary uses
jemalloc; allocator-specific throughput and resident savings need a separate
engine measurement. Exact toolchain, base commit, source digest, all samples,
and summaries are in `evidence/`. Per-process medians expose run variability;
21 batch samples are not 21 independent process repetitions.

## Reproduction and checks

Run on the authorized Linux build machine, from this directory, after syncing
an archived checkout at `BASE_COMMIT` plus this experiment directory:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo fmt --check
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo clippy --all-targets -- -D warnings
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --release --locked
python3 run.py
```

`run.py` writes only this experiment's evidence, runs no Docker commands, and
finishes with `ALLOCATION-LAYOUT-DONE`. Its CPU affinity and `/proc` readings
require Linux. `BASE_COMMIT` identifies the dependency snapshot used here;
when comparing another snapshot, archive it and update that marker first.

Strict experiment clippy and the all-variant test passed on the build server.
Every process must pass full stored-cell equality and every timed checksum.
This is a separate Cargo workspace, and it adds no engine dependencies.
No production engine files changed; the release validation suite was not run
and this research is not a release gate.

## Results

All 120 processes passed. The table uses median batch latency across seven
batches in each of three processes. Speedup is baseline time / variant time.
Retained MiB is requested capacity, not process RSS.

| Fixture | Nested MiB | Flat MiB | Arena MiB | Flat speedup, 1 / 4 workers | Arena speedup, 1 / 4 workers |
| --- | ---: | ---: | ---: | ---: | ---: |
| narrow_numeric | 19.00 | 16.00 | 12.00 | 1.39× / 1.44× | 1.50× / 1.58× |
| wide_numeric | 67.00 | 64.00 | 48.00 | 1.36× / 1.32× | 1.83× / 1.79× |
| mixed_short | 42.73 | 39.73 | 31.73 | 1.29× / 1.19× | 1.56× / 1.61× |
| mostly_long_text | 180.11 | 177.11 | 169.11 | 1.19× / 1.00× | 1.36× / 1.23× |

Construction and output costs matter. These are single-worker medians in ms;
materialization returns 16,384 rows, not the whole fixture.

| Fixture | Build: nested / flat / arena | Output: nested / flat / arena | Destroy: nested / flat / arena |
| --- | ---: | ---: | ---: |
| narrow_numeric | 5.61 / 5.85 / 7.84 | 0.45 / 0.34 / 0.72 | 2.04 / 0.82 / 0.37 |
| wide_numeric | 18.12 / 23.08 / 29.78 | 1.81 / 1.15 / 2.47 | 7.54 / 5.08 / 1.30 |
| mixed_short | 17.82 / 16.68 / 21.74 | 1.86 / 1.57 / 2.31 | 8.18 / 7.18 / 0.53 |
| mostly_long_text | 56.17 / 55.01 / 57.66 | 4.80 / 4.71 / 5.75 | 29.84 / 18.12 / 4.14 |

Interpretation:

- **Contiguous ordinary rows are the first candidate.** Single-worker reads
  improve 1.19–1.39×; output copying also improves in this matrix. Memory
  savings are exactly 3 MiB per fixture by removing row headers. Numeric
  retained allocations fall from 131,073 to one. The wide numeric build is
  slower (23.08 versus 18.12 ms): do not optimize for probe time alone.
- **The arena has stronger memory evidence and a conversion cost.** Retained
  payload drops 36.8%, 28.4%, 25.7%, and 6.1% in fixture order. String-heavy
  buffers fall from 891,897 live allocations to two; that does not mean the
  build made only two allocator calls or that RSS falls by that ratio.
  Arena reads improve 1.36–1.83× with one worker and 1.23–1.79× with four.
  Output materialization regresses in all four fixtures, and wide numeric
  construction grows from 18.12 to 29.78 ms. Favor retained/reused state or
  memory pressure, and measure output-heavy workloads before adopting.
- **Boxing alone is not the priority.** Boxed rows save only 1 MiB per fixture;
  boxed scalar payloads do not reduce the cell size or retained bytes beyond
  the flat arm. `compact` does improve the particular probe kernel, so it is
  not universally slower; its memory rationale is what this test rejects.
- **Parallel bandwidth can erase a local gain.** Four-worker long-text flat
  reads are effectively unchanged (1.00×), despite a 1.19× single-worker gain.
  Do not turn a one-thread result into a concurrency claim.
- **Observed variability limits precision.** Wide numeric baseline process
  medians were 34.25, 39.90, and 35.05 ms; arena medians were 19.07, 19.28,
  and 19.08 ms. The direction survives that spread, but decimal-place
  speedups are descriptive, not confidence bounds. All process medians,
  batch minima, and batch p95 values are in `summary.json`. Batch p95 is
  not SQL-request p95.

## Proposed engine experiments before adoption

These are the next decision gates, not completed validation and not newly
filed issues. Start with flat ordinary `Value` payloads; retain the arena as a
second, separately switchable arm so its costs can be attributed.

1. Add an isolated in-process SQL harness around the actual join build and
   probe path. Use an explicit layout switch and the same catalog, plan,
   input batches, hash implementation, and results in both arms. Include
   unique keys, duplicates, extreme skew, NULLs, all relevant join kinds,
   ENUM/SET ordinals, mixed collations, decimals, and both high and low
   output selectivity. Measure complete build/probe/output/destruction time,
   not just the changed loop. Dense join and fused aggregate fast paths
   remain enabled in both baselines.
2. Reserve slab and index capacity through `MemoryTracker` before allocating.
   Charge coexistence of old/new storage during any freeze step. Use bounded
   slabs rather than a single 4 GiB arena; verify spill transition, hot-key
   replay, cancellation, and drop cleanup at small budgets. Re-run forced
   spill versus in-memory exactness. This protects the work tracked by #12.
3. Run the same engine binary with the shipped allocator and collect
   allocator live/active/resident bytes, whole-process RSS, peak query charge,
   spill bytes, CPU time per query, and wall latency. Test concurrency
   1/4/8/16 under a fixed memory cap and enough duration for stable results.
   Report completed SQL queries per second and per-request p50/p95/p99.
   Randomize A/B order and use at least five independent runs, with a
   reported interval over runs. Use an idle measurement host.
4. Confirm on the dedicated live differential pair with immutable segments,
   memtable inserts, and schema history. For the #31 scenario keep a recorded
   update/delete tail during timing, then fence source position for exact
   comparisons. Compare static and live-tail results separately.
5. Proposed acceptance: at least 10% lower retained query-state bytes, or at
   least 10% higher complete SQL throughput, with no reproducible >5% p95
   or output-heavy regression, exact results, and respected memory/disk caps.
   Smaller effects require more repetitions, not rounded-up claims. If only
   memory improves, adopt only with explicit policy for the latency tradeoff.
   Those thresholds are research proposals, not edits to release policy.
6. After an engine slice is selected, run touched-crate strict clippy and
   tests, commit it, then one required full validation profile. Release
   gating and evidence freshness remain unchanged.

Amdahl's law bounds the expectation: if payload probing is 30% of query time,
a 1.5× probe improvement yields only `1 / (0.7 + 0.3 / 1.5) = 1.11×` query
speedup before construction and output costs. The 30% here is an example,
not a Pintail profile measurement. The present experiments prove allocation
and access mechanisms; **end-to-end SQL throughput and engine RSS gains
remain unproven**.
