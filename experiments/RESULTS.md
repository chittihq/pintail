# Experiment results — 2026-07-31

Machines:
- **local** — Apple M2 Pro, 10 cores, macOS, rustc 1.94.0, `lto=thin`, `codegen-units=1`
- **remote** — Ubuntu 24.04 x86_64 docker host (16 cores), container pinned `--cpus=8 --memory=8g`, rust:1.94-slim

All numbers are median-of-7 after warmup, ms, on 20M rows. Every variant within a block
produced identical checksums. Raw outputs: rerun per `TODO.md`; summary tables below.

## e01 — Filter representation

| Variant (SUM WHERE amount>t, 10% sel) | local | remote |
|---|---:|---:|
| fused branchless multiply-sum | **2.9** | **18.4** |
| fused branchy if-sum | 2.8 | 17.9 |
| byte mask, two-pass | 9.4 | 37.6 |
| bitmap words, iterate set bits | 9.4 | 40.3 |
| selection vector + gather | 21.7 | 44.0 |

Q2 shape (SUM(amount) WHERE status=2, dict predicate): fused **3.3 / 16.7 ms** vs
selection-vector 12.5 / 24.8 ms. Same ordering at 1/50/90% selectivity; the selection
vector's only competitive point is ~1% selectivity on x86 (17.0 ms ≈ fused).

**Verdict: fuse predicate + payload into one pass whenever the pipeline allows; never
materialize an intermediate representation for a single-consumer filter.** When a
representation *is* needed (multi-consumer, combining several predicates), byte masks /
bitmaps beat selection vectors except at very low selectivity — matching DaMoN 2021, not
Photon's blanket position-list claim. Note the ceiling: fused runs at memory bandwidth
(~55 GB/s local), so pintail's current 1,495 ms Q2 has ~450× headroom to this kernel.

## e02 — Aggregation strategy

| GROUP BY user_id (200k groups) | local | remote |
|---|---:|---:|
| hashbrown sequential | 112.8 | 729.3 |
| dense perfect-hash sequential | 38.7 | 78.5 |
| **thread-local dense arrays + merge** | **14.9** | **20.9** |
| shared dense atomics (relaxed) | 292.3 | 54.3 |
| thread-local hashmaps + merge | 89.6 | 332.5 |

Low cardinality (5 / 40 groups): direct dict-code arrays beat hashmaps 1.8–7× sequential;
parallel thread-local arrays land at **2.8–5.6 ms** on both machines.

**Verdicts:** (1) dictionary-code direct-array accumulation is mandatory for
low-cardinality group-bys — the hash table should never exist. (2) Thread-local
accumulators + merge win on both machines at every cardinality tested. (3) **The
"Global Hash Tables Strike Back" result did not replicate as stated**: shared atomics
were 5.4× *better* than thread-local hashmaps on x86 but 3.3× *worse* than them on
Apple Silicon (coherence costs differ wildly) — and never beat thread-local dense
arrays on either machine. Per decision rule 2, atomics are ISA-specific: not adopted.

## e03 — Top-K (K=100)

| Variant | local | remote |
|---|---:|---:|
| clone + full sort | 253.2 | 614.3 |
| clone + select_nth_unstable | 25.0 | 129.2 |
| naive bounded heap (push all) | 355.0 | 416.2 |
| cutoff-guarded heap | 9.3 | 17.7 |
| **parallel guarded heaps + merge** | **1.7** | **4.3** |

**Verdict: cutoff-guarded heaps with parallel per-chunk locals — 146× over full sort
locally, no contest on either machine.** The cutoff (threshold prefilter) is the whole
game: the naive heap that pushes every row is *worse* than sorting. Matches the
ClickHouse/DuckDB/Snowflake threshold-pushdown design; next step (e11) is pushing the
cutoff into granule pruning.

## e04 — Join structure (users 200k ⋈ orders 20M, group by region)

| Probe variant | local | remote |
|---|---:|---:|
| hashbrown per row | 75.2 | 207.3 |
| unchained (tags + range scan), simplified | 79.6 | 307.2 |
| **dense direct-address (perfect hash)** | **30.7** | **49.7** |

Semi-join membership (24.9k build side, 20M probes):

| Variant | local | remote |
|---|---:|---:|
| hashbrown HashSet | 177.0 | 205.7 |
| **dense bitmap (3.2 KB, L1-resident)** | **12.5** | **51.5** |
| blocked bloom + exact confirm | 36.4 | 77.0 |

**Verdicts:** (1) perfect-hash (dense direct-address) join wins 2.4–4.2× — pintail must
detect dense integer key domains and use it (MySQL auto-increment PKs make this the
*common* case, not the exception). (2) For semi-joins, small dense bitmaps demolish hash
sets (14×/4×); blocked blooms are the fallback for sparse domains. (3) The simplified
unchained table did **not** beat hashbrown on all-hit inner probes on either machine —
its value per the paper is on miss-heavy/skewed workloads, which this benchmark shape
doesn't have. Not adopted for v1; re-test (with the full paper layout) if miss-heavy
joins appear.

## e05 — Merge-on-read: the FINAL tax (8 disjoint segments + hot tail)

| f = 1% overlap | local | remote |
|---|---:|---:|
| REF fully-compacted floor | 2.8 | 11.1 |
| A naive 9-way heap merge (always-FINAL) | 190.4 | 488.7 |
| B classified per-segment 2-way merge | 22.9 | 37.2 |
| C scan + patch corrections | **4.0** | **16.6** |

Same ordering at f=0.1% and f=10% (A: 184–524 ms; B: 21–49 ms; C: 3.6–27.7 ms).

**Verdict: the single most important result of the lab.** The naive always-merge path —
which is what pintail effectively does today — costs **17–44× the compacted floor**.
ClickHouse-style overlap classification recovers ~8–13× of that; the scan+patch endgame
for provably-disjoint bases runs within **1.3–2.4× of the floor even at 10% overlap**.
This empirically confirms the sweep-line classification (issue #3 / engine-research) as
the highest-priority structural change, and shows merge-on-read correctness does NOT
have to cost a heap merge.

## e06 — Scanning compressed data (naive FOR+bit-pack, lz4)

| SUM over 20M i64 | local | remote |
|---|---:|---:|
| **plain Vec<i64> scan** | **2.9** | **12.1** |
| FOR+bitpack fused unpack-sum | 17.7 | 26.6 |
| lz4(raw) decompress+sum | 57.5 | 82.7 |
| lz4(packed) decompress+unpack-sum | 19.8 | 32.9 |

Ratios: packed 3.19×, lz4(raw) 1.62×, lz4(packed) 3.17×.

**Verdict: honest negative — naive bit-packing loses 2.2–6× to plain scans despite 3.2×
less memory traffic.** Variable-shift scalar unpacking doesn't autovectorize. This is
precisely the problem the FastLanes transposed layout solves (>100B ints/s claimed);
the follow-up is testing the `fastlanes` crate, NOT hand-rolling packing. Until then:
store compressed (3.2× disk), decode to plain vectors at scan start, scan plain.

## e07 — String representation

| Workload | Vec<String> l/r | chars+offsets l/r | German views l/r |
|---|---:|---:|---:|
| eq short const | 48.0 / 182.5 | 53.7 / 90.1 | **34.5 / 54.6** |
| eq long const | 9.1 / 40.9 | 18.1 / **24.1** | **6.9** / 32.1 |
| ordering `< "m"` | 72.9 / 86.6 | 69.0 / **69.3** | **59.9** / 115.9 |

Memory: Vec<String> 758 MB, chars+offsets 292 MB, views 415 MB.

**Verdict: German-string views win equality workloads on both machines (the group-key
and filter case that dominates pintail's workload) and always beat Vec<String>. Split
decision on x86 for ordering/long-eq where flat chars+offsets wins** — the prefix
fast path branch mispredicts on x86. Adopt views as the execution format; keep the
ordering-comparison kernel eligible for flat-slice specialization (rule 2).

## e08 — Length-classed string hashing

Local: 288.8 vs 288.2 ms. Remote: 671.3 vs 647.9 ms. **Tie (<15%) on both — rule 3:
keep generic hashbrown on byte slices.** ClickHouse's StringHashTable gains come from
hardware-CRC + its whole table design, not length classing alone; revisit only
after a dedicated aggregation table exists.

## e09 — Predicate/condition cache

| 100 dashboard queries | scattered l/r | clustered l/r |
|---|---:|---:|
| full scan | 557.9 / 3794 | 561.4 / 1334 |
| zone-map pruned | 560.7 / 3875 | **31.1 / 132.0** |
| predicate-cache warm | 557.5 / 3907 | 30.9 / 131.0 |

**Verdict: pruning value is entirely a function of data clustering** — on scattered
layouts nothing helps (every 64K-row granule contains every hot tenant); on clustered
layouts zone maps alone give 10–18×, and the predicate cache adds nothing beyond zone
maps *for zone-map-expressible predicates*. The cache's real niche is predicates zone
maps can't express (LIKE, IN-lists, JSON paths) — that test still stands open; and the
result strengthens the case for optional clustering keys / partitioning (GOAL §5.4).

## e10 — Morsel size × core scaling (bandwidth-bound scan)

Local: 3.0 ms @1t → 1.34 ms @4t, flat beyond (memory bandwidth saturates at ~4 cores).
Remote: 17.4 ms @1t → 2.7 ms @10t (6.4× — x86 box has lower per-core bandwidth, scales
further). Morsel size 4K–64K indistinguishable; 1M slightly worse at high thread
counts on both. **Verdict: 64K-row morsels; expect scan parallelism to saturate well
below core count on Apple-class memory systems — parallelism budget belongs to
compute-heavy operators (agg/join), not scans.**

## e11 — Granule-level sweep-line classification (with memtable overlay)

| SUM latest, 250k updates | local | remote |
|---|---:|---:|
| full 10-way heap merge | 206.8 / 195.0 | 538.6 / 499.1 |
| granule-classified, clustered updates (11/312 granules overlap) | **5.9** | **18.7** |
| granule-classified, scattered updates (158/312 overlap) | 23.3 | 43.8 |

**Verdict: the strongest result in the lab — 29–35× under the realistic CDC pattern
(recent-hot updates), 8–11× even under adversarial uniform updates, on both machines.**
Granule-level classification with a memtable overlay is confirmed as the merge-on-read
design for `pintail-store`.

## e12 — Composite-key comparison in k-way merges

| 8×2.5M merge | local | remote |
|---|---:|---:|
| typed tuple heap | 446.8 | 920.6 |
| normalized [u8;20] memcmp heap | 523.5 | 1482.1 |
| **packed (u128,u64) heap** | **444.3** | **725.8** |

Normalized-key encode cost (write-time): ~159–182 ms / 20M rows.

**Verdict: packed u128 keys win-or-tie on both machines (+21% on x86); normalized
memcmp byte keys LOSE 17–61% in heap merges on both — an honest counter to the DuckDB
sorting-paper intuition, whose wins come from radix sort + row-payload locality, not
heap comparisons.** Adopt: pack composite sort keys into ≤128-bit integers when they
fit; keep typed tuples otherwise; offset-value coding remains untested (future).

## Cross-cutting conclusions

1. **Both machines agree on every adopted verdict** (fused filters, dict-code arrays,
   thread-local merge, guarded top-K, perfect-hash joins, dense bitmaps, overlap
   classification). The two disagreements (shared atomics, unchained) were resolved by
   rule 2: not adopted.
2. The M2 is ~2–6× faster per-core on these kernels than the containered x86 host —
   never compare absolute numbers across machines, only orderings.
3. Fused single-pass kernels sit at memory bandwidth; every intermediate representation
   costs 2–8×. The executor design should treat materialization as the exception.
4. These are microbenchmarks of isolated primitives on hot data; end-to-end engine wins
   must be re-proven in `benchmark/` after adoption (issue #3 gates unchanged).

## e13 — High-cardinality parallel aggregation (post-Q6-regression)

20M rows, sparse u64 keys, SUM+COUNT per group, 10 threads local (M2);
remote validation pending (host reserved for benchmark runs).

| median ms | sequential map | thread-local + merge | partitioned shards | two-pass partitioned |
|---|---:|---:|---:|---:|
| 200k groups | 99.1 | 177.2 | 96.3 | **23.7** |
| 2M groups | 421.7 | 684.5 | 169.5 | **47.3** |
| 8M groups | 833.5 | 1163.7 | 386.3 | **122.0** |

**Verdicts:** (1) thread-local hashmaps + merge lose to plain sequential at
every cardinality tested — the Q6 production regression (9.1s → 78.8s,
commit e5ba3ca) was structural, not incidental; the per-round global merge
dominates. (2) Two-pass partitioned aggregation — pass 1 scatters (key,
value) into P per-worker partition buckets, pass 2 aggregates each
partition with zero cross-thread sharing — wins at every cardinality,
4.2–8.9× over sequential. This is the adopted design for parallel
high-cardinality aggregation (task #25); the sequential direct path stays
for small inputs where scatter overhead dominates.

## e14 — Typed kernels vs the Value-enum loop on the Q5 shape

20M rows, per-row date→(year,month) conversion + year==2023 filter (~1/3
selectivity), GROUP BY (year,month) → 12 dense groups, SUM+COUNT, 10
threads local (M2). Born from the 63becb4 run: Q5 takes 5,623 ms in the
engine against 213 ms for ClickHouse despite only ~24 groups.

| variant | median ms |
|---|---:|
| value-enum rows (engine multi-column path model) | 345.5 |
| typed composite u64 key + hashmap | 227.3 |
| typed dense-array kernel | 207.0 |
| two-pass partitioned (composite key) | 40.2 |
| dense array per worker + merge | **29.8** |

**Verdicts:** (1) The original hypothesis is REFUTED: the Vec<Value> key +
hashmap loop costs 345 ms sequential — the engine's 5,623 ms cannot be
living in the group-by. The bottleneck is upstream: YEAR()/MONTH() only
evaluate on Value::Utf8 (expression.rs evaluate_direct_date_part), so Q5
forces the date column's lazy text — 20M native i32 days formatted to
"YYYY-MM-DD" strings, then string-parsed back per row, twice (plus the
WHERE comparing against date literals). Native-unit date-part kernels
(days → civil year/month, no text) are the real Q5/Q7 lever. (2) Once
upstream is fixed, composite-key typed group-by is worth 1.5× sequential
and the dense-array + per-worker merge parallel shape runs the whole
filter+convert+aggregate in ~30 ms — 7× under ClickHouse's end-to-end
213 ms, leaving budget for scan/decode. (3) Extending two-pass to
composite int keys (40 ms) is within 1.35× of the dense ceiling and needs
no cardinality bound; adopt that, keep dense arrays as a follow-up
specialization if profiling justifies it.

## e15 — The Value-enum middle layer's tax (Q6 shape)

20M rows, 2M sparse u64 keys, SUM+COUNT, two-pass P=10 everywhere except
the sequential model, 10 threads local (M2). Value modeled with the
engine's real variant set (32-byte cells).

| variant | median ms |
|---|---:|
| typed contiguous arrays → two-pass | **52.4** |
| typed 64k batches → two-pass | 93.3 |
| Value column batches → two-pass (engine today) | 539.5 |
| Value rows → transpose → two-pass (CDC adopt shape) | 1,229.0 |
| Value batches → sequential hashmap (pre-two-pass path) | 1,170.1 |

**Verdicts:** (1) Enum cells cost 10× over typed arrays on the identical
kernel — building 1.28 GB of 32-byte Value cells for 320 MB of data is
the tax, and its run-to-run variance (226–540 ms) is allocator churn.
The row-major path costs 23×. (2) The engine's Q6 (11,679 ms at 63becb4)
is still ~21× slower than even the Value-batch model, so the Value layer
is necessary but not sufficient to explain it: decode, merge-on-read
visibility, per-group AggregateState indirection and tracker traffic sit
on top. Typed columns end-to-end removes the whole stack between segment
and kernel, not one slice of it. (3) Chunked batches per se are fine
(1.8× from copies, fixable with borrowing) — batching is not the enemy,
materializing enums per cell is.

## e16 — Grouped COUNT(DISTINCT) representation (Q7 lane)

20M rows, 8 regions, 200k user space, COUNT(DISTINCT user)+SUM per
region, 10 threads local (M2).

| variant | median ms |
|---|---:|
| HashSet<Value> per group (engine today) | 388.7 |
| HashSet<u32> per group | 148.5 |
| dense bitmap per group (200 KB total) | 21.1 |
| parallel per-worker bitmaps + OR-merge | **6.5** |
| parallel user-partitioned bitmaps | 37.4 |

**Verdicts:** (1) Dense bitmaps win grouped distinct-count exactly as
they won e04's semi-join membership: 18× sequential, 60× with per-worker
bitmaps OR-merged (bitmap OR is embarrassingly mergeable — the same
property that made e13's thread-local hashmap merge LOSE makes bitmap
merge win). (2) User-partitioned scanning loses: P full scans of the
region/user columns dwarf the merge it avoids. (3) Adoption rule: int
keys with a bounded dense domain (user ids against table row counts) →
per-worker bitmaps + OR-merge; otherwise typed HashSet (still 2.6× over
Value sets). The engine knows key bounds from table statistics.

## e17 — Morsel-driven fusion vs staged decode (Leis et al., SIGMOD 2014)

20M rows, status u8 / amount i64, 64k-row LZ4 blocks (94 MB compressed),
SUM+COUNT WHERE status=2, 10 threads local (M2).

| variant | median ms |
|---|---:|
| staged sequential (decode all, then agg) | 78.8 |
| staged parallel (decode ‖ barrier ‖ agg) | 12.4 |
| morsel-fused (decode+agg per block) | **10.4** |
| morsel-fused, per-thread scratch | 11.1 |

**Verdict (revised per Codex review): parallel decode is the confirmed
win (6.4×); fusion adds ~16% on this shape and removes the materialized
180 MB intermediate.** The two-narrow-column workload cannot separate
staging from fusion more sharply — fusion's case strengthens when staged
intermediates exceed cache; re-measure with wider projections before
citing more than ~1.2× for it. "Zero per-block allocation" was also
false as written (lz4 decompress allocates internally; rayon map_init is
iterator-local, not thread-pinned).

## e18 — Small materialized aggregates per block (Moerkotte VLDB 1998; Data Blocks SIGMOD 2016)

20M rows, 64k blocks, SMA = count/sum/min/max + per-status sub-cube,
112 B/block, built once in 28.6 ms. Hot in-memory model.

| Q3 shape (per-status SUM/COUNT) | median ms |
|---|---:|
| full fused parallel scan | 2.31 |
| SMA, 0% dirty | **0.019** |
| SMA, 1% dirty | 0.10 |
| SMA, 20% dirty | 0.55 |
| SMA, 100% dirty | 2.33 (= scan, no overhead) |

**Verdict: UPPER BOUND ONLY — the product lever is real but this
experiment does not validate CDC correctness.** "Dirty" here rescans
unchanged data; real dirtiness means newer versions/tombstones in
memtables and overlapping segments, MIN/MAX are not delta-adjustable
under deletion, and sub-cubes assume a stable global dict mapping. The
120× clean-path headroom (and ~10× at 20% dirty) justifies building a
CDC-correct prototype through the real snapshot/merge-on-read path with
per-statistic invalidation before any adoption claim.

## e19 — Executing on compressed data (Abadi et al., SIGMOD 2006)

20M rows, FOR+bit-pack per 64k block (20-bit width, 50 MB vs 160 MB raw),
10 threads local (M2).

| variant | median ms |
|---|---:|
| global SUM: unpack to scratch, then sum | 3.04 |
| global SUM: fused unpack-accumulate + FOR algebra | **2.40** |
| filtered SUM: unpack to scratch, then fused | 4.15 |
| filtered SUM: single pass on packed + codes | **3.20** |

**Verdict: fused-on-compressed is worth ~1.3× with scalar unpacking and
3.2× storage; a later lever, contingent on a PTSEG v3 encoding pass
(BtrBlocks/FastLanes-style SIMD widths would change both sides).** One
data-shape caveat: single friendly width tested; codec edge cases
(width 0, partial blocks) need fixtures before any engine adoption.

## Codex adversarial review of e14–e19 (2026-08-02)

A full second-model review produced 29 findings; the ones that change
decisions, adopted as standing rules:

1. **Phase zero is engine profiling, not a rewrite.** Q6's 11.7 s vs
   e15's 540 ms Value-batch model leaves ~21× unexplained by any lab
   result. No typed-pipeline rewrite starts until per-query spans
   (decode, visibility, adoption, Value materialization, buffering,
   scatter, finalize, top-K) explain ≥80% of Q6 wall time and point the
   first milestone at the largest component. Concrete suspects found by
   inspection: even the two-pass path materializes Values via
   group_values.value(row) per row, and top-K clones retained rows.
2. **e15/e14/e16 datasets diverge from the benchmark's** (Q6 is 100k
   correlated user ids summing DECIMAL(12,2), not 2M random u64;
   user↔region is correlated in seed.sql, flattering e16's bitmaps;
   seed spans 5 years not 3). Re-shape before citing exact ratios;
   orderings are expected to hold, magnitudes are not.
3. **Harness gaps:** checksums only validated on the final run; XOR
   folds are collision-prone; fixed variant order shares allocator/
   thermal state. Adopt: per-run checksum stability asserts, sorted
   exact result comparison for small outputs, and null-bearing fixtures
   (current cell_pair-style helpers silently zero NULLs).

## e20 — Encoding census: what PTSEG's encodings cost and what the missing ones buy

20M rows per column, 64k-row blocks, single-threaded, local (M2). Every decoder
reconstructs the column exactly (position-mixing checksum over decoded values).
Sizes marked "+lz4" apply LZ4 to the *encoded* block, which is what PTSEG
actually writes.

### The second compression layer is a loss on bit-packed data

| column | encoding | encoded | +lz4 | decode encoded | decode +lz4 |
|---|---|---:|---:|---:|---:|
| amount (uniform) | FOR+bitpack | 50,007,344 | 50,201,736 | 63.0 ms | 69.2 ms |
| amount (0.1% outliers) | FOR+bitpack | 84,883,024 | 85,214,187 | 63.8 ms | 69.0 ms |

LZ4 over a densely bit-packed block makes it **bigger** (+0.4%) and decode
**8–10% slower**.

(Corrected: the first run of this experiment unpacked from the original
in-memory words after decompressing, measuring a pipeline nobody runs and
reporting a spurious 27% on the outlier row. The kernel now unpacks out of the
decompressed byte buffer, which is the only buffer a real decoder holds. The
conclusion survives at a smaller and more consistent margin.) Bit-packing leaves almost no redundancy for a byte-oriented
matcher to find, so the second layer is pure cost. This is BtrBlocks' §2.1
finding reproduced on our own format.

The layer is not always a loss — it depends entirely on what the first layer
left behind:

| column | encoding | encoded | +lz4 | lz4 verdict |
|---|---|---:|---:|---|
| status (cycles every 5) | dict codes | 7,517,136 | 44,290 | **170× win** |
| status (clustered runs) | dict codes | 7,517,136 | 954,337 | 7.9× win |
| user_id (200k distinct) | dict codes | 176,441,984 | 108,610,649 | 1.6× win |
| region (8 random values) | dict codes | 7,524,480 | 7,542,146 | loss |
| amount | FOR+bitpack | 50,007,344 | 50,201,736 | loss |
| ratio (real doubles) | plain | 160,000,000 | 160,627,452 | loss |

**Verdict: WITHDRAWN pending re-measurement (see the Codex review below).**
The *size* rows are plain byte counts and stand. The decode figures do not:
the no-LZ4 arm decodes from a native `Vec<u64>` while the LZ4 arm parses a byte
buffer, so the two arms run different decoders and the 8–10% is partly that
difference rather than LZ4. And `Compression::None` is a PTSEG segment-version
break, not a free tag: the segment reader accepts versions 1 and 2 only, and the
manifest's version 3 is an independent counter.

### Patched exceptions — the clearest ratio win available

| data | FOR+bitpack | FOR+patched | decode FOR | decode patched |
|---|---:|---:|---:|---:|
| amount, uniform | 3.20× (20 bits) | 3.20× (20 bits) | 62.8 ms | 63.2 ms |
| amount, 0.1% outliers | 1.88× (33–34 bits) | **3.18×** (20 bits) | 65.2 ms | 65.5 ms |

A 0.1% tail of large values costs 13 extra bits on *every* value in the block.
Storing those stragglers out of line restores the narrow width for **1.7× the
ratio at no measurable decode cost** — the patch loop is proportional to the
exception count, not the block. On clean data the chooser lands on the same
width, so it is never worse.

### Run-end over dictionary codes — an execution win, not a storage win

| shape | dict +lz4 | run-end +lz4 | rows/run |
|---|---:|---:|---:|
| status, cycles every 5 (benchmark shape) | 44,290 | 80,630,880 | 1.0 |
| status, clustered into runs | 954,337 | 941,220 | 136.5 |
| region, 8 random values | 7,542,146 | 94,538,581 | 1.1 |

After LZ4, run-end **ties** dictionary on clustered data (941 KB vs 954 KB) and
is catastrophic on unclustered data — 1,800× worse on the benchmark's cyclic
status column. LZ4 already captures run redundancy, so run-end buys no bytes.

What it does buy is compute, because the count is arithmetic per run:

| shape | decode then scan | count per run | speedup |
|---|---:|---:|---:|
| status, clustered | 34.8 ms | **0.080 ms** | 435× |
| status, cyclic | 34.4 ms | 10.5 ms | 3.3× |
| region, random | 34.5 ms | 9.3 ms | 3.7× |

**Verdict: not a compression change.** If adopted it is an execution change,
justified by predicate/aggregate evaluation per run, and it must be gated on
measured run length (BtrBlocks gates RLE at average run length ≥ 2).

### Floats

| column | plain+lz4 | pseudodecimal | decode |
|---|---:|---:|---:|
| price (2-decimal money as f64) | 1.46× | **3.19×** | 72.0 ms |
| ratio (genuinely real doubles) | 1.00× (lz4 *expands* it) | rejected | — |

Pseudodecimal more than doubles the ratio on decimal-like doubles, at a decode
cost (72 ms vs 62.8 ms for FOR on the same row count). **Applicability caveat
that likely disqualifies it for us:** Pintail stores MySQL `DECIMAL` as scaled
i128 units, not as f64, so money never reaches this path. Only real `FLOAT`/
`DOUBLE` columns do, and those are the case where pseudodecimal is rejected.
BtrBlocks reports the same trade — +20% double ratio for −35% double decode —
and gates it off below 10% unique values.

### Not adopted, and why

- **Dictionary on high-cardinality integers**: 0.91× encoded, worse than plain.
  Our chooser already restricts Dictionary to text under 10% distinct; this
  confirms the guard rather than challenging it.

## e21 — FastLanes interleaved bit-packing, in Rust (Afroozeh & Boncz, PVLDB 16(9) 2023)

20M values, frame-of-reference deltas at 20 bits (our `amount` column's real
width), T=32 so 32 lanes of a 1024-value chunk, single-threaded, local (M2).
Only the bit-level interleave was implemented — FastLanes' mechanism 1a, which
preserves logical order. The transposed tuple layout (needed only for DELTA and
RLE) was not tested.

Every published FastLanes throughput number is C++/clang; the Rust port ships
no measurements, so this tests the claim with our own compiler.

| variant | unpack + checksum | unpack + sum |
|---|---:|---:|
| horizontal (PTSEG today) | 57.0 ms | 22.8 ms |
| horizontal, equally tuned (control) | — | 23.2 ms |
| FastLanes interleaved | **39.5 ms** | **5.7 ms** |

**The control matters more than the headline.** The interleaved kernel writes
into a pre-sized slice by index, hoists the mask, and splits the word-crossing
case out of the inner loop; the original horizontal kernel does none of those.
So the gap could have been my coding rather than the layout. `unpack_horizontal_tuned`
gives the horizontal layout every one of those advantages, including processing
a whole repeat group of `32/gcd(W,32)` values whose word/offset pattern is
identical each time. It lands at 23.2 ms — no better than the naive 22.8 ms.
The 4× is the layout.

Packed size is identical (−0.00%) and the decoded output is in logical order —
the checksums match the horizontal decoder and the source array exactly.

**Read the second column, not the first.** The checksum has a serial dependency
chain that costs roughly 35 ms whichever decoder feeds it, which compresses the
apparent gap to 1.4×. Subtracting that common cost puts the actual unpack at
about 22.7 ms horizontal against 5.0 ms interleaved, consistent with the sum
column's **3.5×**. The paper predicts 2× for a purely scalar path at T=32 and
says LLVM then auto-vectorizes further; that is what the sum column shows.

Width sweep (unpack + checksum, so all figures carry the same ~35 ms floor):

| W | horizontal | interleaved |
|---:|---:|---:|
| 4 | 53.6 ms | 37.8 ms |
| 8 | 53.2 ms | 37.5 ms |
| 12 | 55.1 ms | 38.7 ms |
| 16 | 54.3 ms | 38.0 ms |
| 20 | 56.8 ms | 41.5 ms |
| 24 | 57.9 ms | 39.3 ms |
| 28 | 55.8 ms | 40.2 ms |

The advantage is flat across widths, which matters because it means the win does
not depend on the data happening to pack narrowly.

**Verdict: PROMISING BUT NOT ESTABLISHED (see the Codex review below).** The
tuned-horizontal control does not yet give the horizontal layout every advantage
the interleaved kernel has, the experiments do not use PTSEG's actual exact-length
byte bitstream, and they use 64k-row blocks where the engine's default is 16k.** It is a byte-layout change inside a packed column segment,
so it needs a PTSEG format-version bump but touches neither row order nor
predicate paths nor partial reads. Sequenced against e20's finding, the two
compose: interleaving makes unpacking cheaper, and dropping the LZ4 layer over
bit-packed blocks removes the memcpy-plus-match pass that currently sits in
front of it.

Caveat: T=32 only, one machine, and the harness measures decode into a
materialized array. Before adoption it needs the T=8/16/64 kernels, the partial
final chunk (1024 does not divide 20M evenly — 256 values were dropped here),
width 0 and width 64 fixtures, and a re-run on the Linux reference host.

## Codex adversarial review of e20–e21 (2026-08-04)

A second-model review of the two compression experiments before any engine work.
17 findings, 3 critical. The ones that change decisions, adopted as standing
rules alongside the e14–e19 set:

1. **Both arms of a codec comparison must run the identical decoder.** e20's
   no-LZ4 arm decoded from a native `Vec<u64>`; the LZ4 arm parsed a serialized
   byte buffer through a different function. The 8–10% decode tax is therefore
   partly the byte-parsing difference, not LZ4. The size rows are unaffected.
2. **A new block-level tag is a segment format break.** PTSEG's own
   `FORMAT_VERSION` is 2 and `format_version_supported` accepts 1 and 2; the
   manifest's version 3 is a separate counter that does not version block tags.
   `Compression::None = 0` needs a PTSEG version bump, versioned decoding, and
   golden tests proving new readers read v1/v2 while old readers *reject* rather
   than misread new files.
3. **A candidate encoding must be measured through a self-delimiting wire
   format.** `PforBlock` serialized packed words, exception positions and values
   with no exception count, and its size accounting charged four bytes more than
   it emitted; decode read the already-separated in-memory vectors. Neither the
   3.18× ratio nor "no measurable decode cost" is supported for an implementable
   encoding.
4. **Experiments must use the engine's block size.** These used 64k-row blocks;
   `DEFAULT_BLOCK_ROWS` is **16,384**. Block size changes per-block bases,
   outlier counts, metadata share, and cache footprint, so the ratios do not
   transfer.
5. **The engine's representation is not the experiment's.** PTSEG writes an
   exact-length byte bitstream decoded through a 16-byte window and materializes
   into typed columnar builders; the experiments use padded word arrays decoded
   into a `Vec`. Engine-applicability claims need the candidate inserted behind
   `encode_packed`/`unpack` and measured on real segment files.
6. **Dictionary encoding is selected only for Utf8/Binary.** The run-end
   comparison used integer dictionaries the engine never builds, so the
   "ties LZ4-compressed dictionary" result does not describe PTSEG.
7. **An equivalence claim needs an equivalence bound.** "No measurable decode
   cost" rested on median-of-7 with no confidence interval, fixed variant order,
   and checksum validation only on the final run.
8. **Outlier fixtures must be two-sided.** `amounts_with_outliers` generates only
   high outliers; a *low* outlier becomes the frame-of-reference base and widens
   every delta, which the patched-exception search never faced. The width search
   also minimises exception count against a fixed 2% budget rather than encoded
   bytes, so "never worse on clean data" is unproven outside the tested shape.
9. **Five round-trip fixtures are mandatory before any encoding lands:** width 0,
   width 64, partial final block, all-NULL column, single-row block. e21 rejects
   partial chunks outright and drops the final 256 values.

Net: the *direction* survives — LZ4 demonstrably expands densely bit-packed
blocks and demonstrably wins big on dictionary codes — but no adoption decision
is supported by these two experiments as written.

## e22 — The contested claims, settled

Re-run under the methodology the Codex review demanded: **16,384-row blocks**
(the engine's `DEFAULT_BLOCK_ROWS`, not e20/e21's 64k), every decoder reading a
**serialized byte buffer** through the identical `word_at` accessor, a
**const-generic width-specialized** horizontal control, **two-sided** outliers,
a **byte-cost** exception search, and a **self-delimiting** patched format.
Round-trip fixtures pass first: width 0, width 64, single row, partial block,
two-sided outliers.

### Claim 1 — the LZ4 layer: SUPPORTED

Both arms now run the same const-generic kernel over bytes; only the source of
those bytes differs.

| data | FOR size | FOR+lz4 size | decode FOR | decode lz4+FOR |
|---|---:|---:|---:|---:|
| amount, uniform | 50,034,188 | 50,225,934 | 59.9 ms | 65.3 ms (+9.0%) |
| amount, two-sided outliers | 86,133,964 | 86,472,606 | 62.9 ms | 67.0 ms (+6.5%) |

LZ4 over a bit-packed block is **bigger and 6.5–9% slower to decode**, now
measured with the decoders equalized. The claim survives its correction.

### Claim 2 — the interleaved layout: SUPPORTED AT HALF THE CLAIMED SIZE

Consumer costs measured *directly* rather than inferred by subtraction:

| measurement | median |
|---|---:|
| consumer only: checksum over decoded | 35.3 ms |
| consumer only: sum over decoded | 2.6 ms |
| unpack + checksum: horizontal (const-generic) | 59.2 ms |
| unpack + checksum: interleaved | 46.6 ms |
| unpack + sum: horizontal (const-generic) | 28.2 ms |
| unpack + sum: interleaved | 14.2 ms |

Subtracting the directly-measured consumer cost gives pure unpack of **~24–26 ms
horizontal against ~11–12 ms interleaved**, from both the checksum and the sum
path independently: **2.1×**, not the 4.0× e21 reported.

e21's 4× was an artifact of an unfair control, exactly as the review predicted.
Giving the horizontal layout compile-time-constant widths closes half the gap.
The remaining 2.1× is **precisely what the FastLanes paper predicts for a scalar
path at T=32** (64/T = 2), which is the most reassuring outcome available: the
corrected measurement agrees with the published model instead of beating it.

Interleaved is also 9,768 bytes *smaller* across the column — the horizontal
packer carries a sentinel word per block that interleaving does not need.

### Claim 3 — patched exceptions: LARGELY WITHDRAWN

| data | FOR | patched | decode FOR | decode patched |
|---|---:|---:|---:|---:|
| amount, uniform | 3.20× | 3.20× (same width) | 59.9 ms | 64.3 ms |
| amount, **two-sided** outliers | 1.86× | **1.91×** | 62.9 ms | 63.0 ms |

e20 reported 1.88× → 3.18× on outlier data. That was measured with **high
outliers only**. With outliers on both sides — the realistic shape — a low
outlier becomes the frame-of-reference base and widens every delta no matter
what the exception list does, so patching recovers only **2.5%**, not 70%.

Decode is free (63.0 vs 62.9 ms), and on clean data the byte-cost search picks
the same width, so it is never worse. But it is a marginal safety net, not the
headline win e20 claimed.

### Still open

Findings from the review that this experiment does **not** settle, and which
still gate adoption: measuring inside PTSEG's real exact-length bitstream and
typed columnar builders rather than a lab `Vec` (#5, #9); genuinely cold file
scans rather than warm heap buffers (#7); the run-end comparison against the
engine's actual UTF-8-only dictionary path (#13, #14); and the PTSEG
segment-version bump with golden compatibility tests that `Compression::None`
requires (#2).

## e23 — In-engine scan probe: the encoding wins do not transfer

`crates/pintail-store/examples/scan_probe.rs`. 20M rows through PTSEG's real
writer and reader — actual segment files, reopened so no writer state serves
the read, drained two ways: `next_column_chunk` (decoded columns, what a
vectorized operator consumes) and `next_chunk` (additionally transposed into
per-row `Vec<Value>`).

Load: 131.6 s. On disk: 60 segments, 224,867,282 B (11.24 B/row).

| projection | first scan | columns only | + row materialization |
|---|---:|---:|---:|
| amount only | 9823 ms | **9055 ms** | 9862 ms |
| amount + day | 10153 ms | 10622 ms | 11507 ms |
| status only (dictionary) | 10797 ms | 10279 ms | 11007 ms |
| all five columns | 15054 ms | 15403 ms | 17321 ms |

**The decode kernel is not the cost.** e22 unpacks 20M frame-of-reference
values from bytes in **~24–26 ms**. The engine takes **9055 ms** to deliver the
same 20M values as decoded columns — roughly **360×** more. Row materialization,
the obvious suspect, accounts for only ~0.8 s of it (9055 → 9862).

Sizing the two candidates against that:

| candidate | lab saving on 20M values | share of a real 9055 ms scan |
|---|---:|---:|
| FastLanes interleaved bit-packing (2.1×) | ~13 ms | **0.14%** |
| dropping LZ4 over bit-packed blocks | ~5 ms | **0.06%** |

**Verdict: neither candidate is worth implementing now.** Both are real wins on
the kernel and both are invisible in the engine, because something in the scan
path costs three orders of magnitude more than the arithmetic they improve. A
format-version bump, golden compatibility tests and 116 unpack kernels cannot be
justified by 0.14%.

This is the e14–e19 standing rule firing again in a new place: *phase zero is
engine profiling, not a rewrite.* The prerequisite is per-span attribution of
those 9 seconds — segment open and footer parse, key/version/tombstone header
merge across 60 segments, block window decode, null merge, typed builder
appends, memory accounting — until ≥80% is explained. Whatever dominates it is
the actual WS5 target; encoding is downstream of it.

The size findings stand on their own and remain worth acting on independently of
decode: LZ4 measurably *expands* densely bit-packed blocks while earning up to
170× on dictionary codes, so a per-block "keep the codec only if it pays" rule
is still correct — just justified by bytes and I/O, not by the 6–9% decode tax,
which is noise at engine scale.
