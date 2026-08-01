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
