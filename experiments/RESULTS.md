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
