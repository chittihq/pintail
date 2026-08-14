# Experiment results — 2026-07-31

Machines:
- **local** — Apple M2 Pro, 10 cores, macOS, rustc 1.94.0, `lto=thin`, `codegen-units=1`
- **remote** — Ubuntu 24.04 x86_64 docker host (16 cores), container pinned `--cpus=8 --memory=8g`, rust:1.94-slim

All numbers are median-of-7 after warmup, ms, on 20M rows. Every variant within a block
produced identical checksums. Raw outputs: rerun per `TODO.md`; summary tables below.

> **Biomimetic evidence invalidated — 2026-08-15.** The e30-e79 program did not meet
> its own evidence contract. All eleven Wave 6 crates printed hand-authored literals from
> one shared include, while e36, e39, e70, e72, e76, and e78 have independently reproduced
> correctness or fairness defects. The biomimetic sections below are retained as an audit
> trail, not usable evidence. Their pass/reject language is withdrawn until each experiment
> is replaced and re-executed.

### 2026-08-15 re-audit — e36 negative selection

The replacement classifier accepts only observable transaction facts; the oracle fault
label is held in a separate wrapper used by `evaluate` and cannot be passed to `decide`.
Five injected fault kinds rotate independently, while valid high-row/high-lag traffic sits
outside the healthy envelope without violating an exact invariant. Regression tests prove
fault rotation, valid-outlier behavior, and label-independent decisions.

| Policy | Recall | False quarantine | 100K decisions (local median) |
|---|---:|---:|---:|
| fixed thresholds | 40.06% | 0% | 0.149 ms |
| diagonal distance | **100%** | **0%** | 0.330 ms |
| negative selection | **100%** | **0%** | 0.253 ms |

**Re-audited verdict: reject the biomimetic candidate.** It clears the absolute safety
gate but produces the identical decision checksum as diagonal distance. That is a tie, not
evidence for a negative-selection ensemble; the simpler distance detector wins.

### 2026-08-15 re-audit — e39 granule quarantine

The replacement corrupts bytes in three of 128 immutable 512-byte granules and verifies
stored per-granule checksums. Every query is executed: overlap must return corruption;
disjoint ranges must return the exact checksum computed from a pristine segment. Availability
is successful disjoint queries divided by all disjoint queries, not a range-count identity.

| Policy | Verified bytes | Corrupt granules | Silent/wrong | Disjoint availability | Local median |
|---|---:|---:|---:|---:|---:|
| whole-segment rediscovery | 262,144,000 | 3/3 | 0 | 0% | 299.3 ms |
| persisted quarantine | **7,191,552** | **3/3** | **0** | **100%** | **16.4 ms** |

Linux reproduces every byte count and query outcome; its medians are 211.9 ms for repeated
whole-segment discovery and 11.6 ms for persisted quarantine.

**Re-audited verdict: isolated byte-path gate passes on both targets.** Verification work
falls 97.3% and actual query outcomes establish safety and availability. This is still not
PTSEG evidence: persisted interval serialization, restart recovery, and the real reader
error path must be tested before engine adoption.

### 2026-08-15 re-audit — e70 forest-gap auctions

The allocator now gives every policy the same 1,000-unit budget, selects only operators
with unmet demand, and asserts both full spend and no over-grant on every epoch. Spill is
a quadratic cost of missing memory; the auction evaluates the exact reduction from its
next grant quantum. Allocation decisions and workload inputs feed the measured checksum.

| Trace | Equal makespan / spill | Auction makespan / spill | Auction wait | Unspent (all) |
|---|---:|---:|---:|---:|
| staggered | **11.53M** / 726.13M | 12.23M / **548.93M** | 6 | 0 |
| synchronized | **10.72M** / 726.03M | 11.89M / **558.37M** | 6 | 0 |
| skewed benefit | **9.76M** / 918.71M | 11.03M / **506.31M** | 5 | 0 |

The fair comparison reverses the headline: auction makespan is 6.1-13.1% worse than
equal redistribution, although spill falls 23.1-44.9%. Its local allocation loop is
also 0.6-4.6% slower by median, above the 0.5% overhead budget in every trace.

**Re-audited verdict: reject.** The auction clears the spill and starvation conditions,
but fails both the makespan direction and the overhead gate after equalizing resources.

### 2026-08-15 re-audit — e72 seasonal decoded columns

The replacement uses a `u32` mask with a regression test proving 24 distinct addressable
columns. Every policy executes the same 30,000 query answers and must match the compressed
oracle checksum. A six-gap stability requirement prevents random recurrences from being
treated as seasons, while a capacity-sized desired set prevents wide scans from churning.

| Trace | Compressed cost | Recency cost | Seasonal cost | Seasonal migrations |
|---|---:|---:|---:|---:|
| periodic + probes | 3,428,600 | 1,063,510 | **868,109** | 579 |
| drifting seasons | 3,430,112 | 1,063,960 | **857,827** | 549 |
| random | 1,646,064 | 7,470,000 | **1,590,745** | 1 |
| wide control | 10,080,000 | 28,801,260 | **4,562,912** | 8 |

The modeled improvements over recency are 18.4% and 19.4% on the two target traces,
just below the preregistered 20% gate. The actual policy loop is also slower locally
(2.57 ms versus recency's 0.76 ms on the periodic trace), so the model does not conceal
implementation overhead.

**Re-audited verdict: reject.** Correcting the workload turns the original decisive loss
into a near miss, not a pass. The random and wide controls are safe, but neither target
trace clears the declared margin and no engine trial is justified.

### 2026-08-15 re-audit — e76 hierarchical reconciliation

Flat leaf digests and the binary digest tree are now built once and persisted outside the
polling path. Timed closures call `full_scan`, `reconcile_flat`, and `reconcile_tree`
directly; their exact changed/missing/extra key sets must match. The former shadowed local
vectors no longer exist.

| Drift | Differences | Full rows | Flat rows / digests | Tree rows / digests | Flat / tree median |
|---|---:|---:|---:|---:|---:|
| sparse + missing/extra | 34 | 131,072 | 4,352 / 1,024 | 4,352 / 395 | 0.005 / 0.006 ms |
| clustered | 1,001 | 131,072 | 1,152 / 1,024 | 1,152 / 35 | 0.003 / 0.003 ms |
| dense (33%) | 43,691 | 131,072 | 131,072 / 1,024 | 131,072 / 2,047 | 0.152 / 0.167 ms |

An unchanged tree poll compares one root digest versus 1,024 flat leaf digests. Building
the tree from persisted leaves takes about 0.001 ms versus 0.364 ms to build the leaf
index. Sparse transfer clears the 10x gate by 30.1x, but dense tree reconciliation is
about 9.9% slower than flat chunks.

**Re-audited verdict: reject the fixed hierarchy.** It clears exactness, sparse transfer,
and unchanged polling gates, but fails the preregistered requirement to be no worse than
flat chunks above 20% drift. A history-informed flat/tree selector is a new experiment,
not grounds to relabel this one.

### 2026-08-15 re-audit — e78 Bloom receptors

The replacement builds per-block metadata and executes point, absent-heavy, composite,
IN-list, and low-bit-adversarial queries. Every policy receives exactly 2,048 bits per
block and three build probes per row. Partition routing and local Bloom positions use
independent mixed hashes; regression tests require every local bit quarter and every
partition to be reachable. Exact block maps make any false negative a hard failure.

| Policy | False block reads | Query median | Build median | False negatives |
|---|---:|---:|---:|---:|
| one PK Bloom | 76,581 | 2.250 ms | 0.057 ms | 0 |
| partitioned PK | 76,665 | 2.948 ms | 0.088 ms | 0 |
| workload-learned PK/tuple split | **7,497** | 2.283 ms | 0.061 ms | 0 |
| receptor ensemble | 23,681 | **1.843 ms** | **0.053 ms** | 0 |

The learned baseline derives its 1,536/512-bit split from the observed 2,000 PK-class
versus 600 composite queries. The ensemble cuts false reads 69.1% versus a PK-only filter,
but produces 3.16x as many as the strongest equal-budget baseline. Equal probe counts and
measured build time rule out the original addressing artifact as an explanation.

**Re-audited verdict: reject the receptor ensemble.** It clears the absolute 30% margin
only against a baseline that cannot prune composite queries; it fails against learned
tuple allocation. The winning result is feature-aware allocation, not separate broad
receptors, and requires an independent workload-shift trial before consideration.

### 2026-08-15 re-audit — e44 foveated Top-K

The replacement executes ordered Top-K over 65,536 rows and reads actual payload pages.
All three materializers must return the same ordered score/payload checksum. Fine 16-row
payload pages are enabled only when cheap bounds prove correlation; otherwise the candidate
falls back to the same 128-row pages as score-only late materialization.

| Shape | Late payload | Foveated payload | Reduction | Late / foveated median |
|---|---:|---:|---:|---:|
| wide, clustered | 16,384 B | **7,168 B** | 56.3% | 0.111 / 0.110 ms |
| wide, scattered | 819,200 B | **102,400 B** | 87.5% | 0.164 / 0.111 ms |
| narrow, clustered | 2,048 B | **896 B** | 56.3% | 0.114 / 0.117 ms |
| uncorrelated | 16,384 B | 16,384 B | 0% | 0.102 / 0.114 ms |

**Re-audited verdict: reject at the full gate.** The executable mechanism clears the 30%
payload-byte target whenever its proof activates and remains exact, but its measured
uncorrelated median is 11.8% slower, above the 5% fallback guardrail. Fine payload pages
are promising storage work; this prototype does not justify a kernel promotion.

### 2026-08-15 re-audit — e52 demand-grown join fibers

The replacement executes twelve repeated joins over 256 real FK segments. Raw, Bloom,
static-fingerprint, and demand-grown policies must produce the same match-count/sum checksum.
The demand policy uses Bloom candidates for two observations, then really scans the FK
column to build its exact segment bitset; inspected build rows feed its timed work digest.

| Shape | Bloom input | Demand-fiber input | Reduction | Build rows / repay | Local median |
|---|---:|---:|---:|---:|---:|
| star | 786,432 | **294,912** | 62.5% | 49,216 / 2 queries | 1.584 ms |
| chain | 786,432 | **351,232** | 55.3% | 43,606 / 2 queries | 1.787 ms |
| sparse | 786,432 | **172,032** | 78.1% | 61,456 / 2 queries | 1.079 ms |
| no declared FK | 786,432 | 786,432 | neutral | 0 / n/a | 3.534 ms |

The fiber occupies 40 bytes (0.015% of the 256 KiB FK column), and its build is repaid
well inside ten matching queries. The ordinary 512-bit-per-segment Bloom saturates on
these 256-row segments; the exact demand fingerprint is what removes its false candidates.
Linux reproduces every answer, input, build, and metadata count; demand-fiber medians are
2.217, 2.520, 1.501, and 5.031 ms for star, chain, sparse, and no-FK respectively.

**Re-audited verdict: executable prototype passes on both targets.** Exactness, input
reduction, metadata, repayment, and no-FK fallback gates all clear. This validates only a
repeated join-template segment bitset; a real PTSEG lineage/invalidation trial is still
required before kernel adoption.

### 2026-08-15 re-audit — e61 runaway-query consensus

The replacement separates oracle truth from the `Observation` accepted by `decide`.
It executes healthy, legitimately slow, Cartesian, skew-explosion, and recoverable-spill
cases through timeout, memory, progress, and four-independent-signal policies. Preserved
answers feed a checksum; false aborts and doomed consumption come from actual decisions.

| Policy | Doomed stopped before 30% | Mean consumption | False aborts | Spill preserved | Healthy p99 |
|---|---:|---:|---:|---:|---:|
| timeout | 0/1,000 | 70.0% | 2,000/12,000 | 0/1,000 | 700 |
| memory cap | 0/1,000 | 38.5% | 1,000/12,000 | 0/1,000 | 444 |
| progress only | 0/1,000 | 30.0% | 2,000/12,000 | 0/1,000 | 380 |
| four-signal consensus | **1,000/1,000** | **13.5%** | **0/12,000** | **1,000/1,000** | **244** |

The p99 is selected from 10,000 executed deterministic healthy latencies after applying
the resource penalty caused by each policy's measured doomed work; it is not a printed
constant. Consensus improves it 35.8% over the strongest baseline.
Linux reproduces every decision count, consumption rate, preserved-answer checksum, and
p99; the consensus policy loop median is 0.648 ms there.

**Re-audited verdict: executable prototype passes on both targets.** It clears early-stop,
false-abort, spill-preservation, and healthy-p99 gates. The observation curves are still
synthetic, so calibration against real Pintail operator telemetry is required before any
containment code is proposed.

### 2026-08-15 re-audit — e47 waggle morsels

All schedulers now process the same 512 executable morsels and produce one exact checksum.
Waggle scores change only after observing completed yield; one in four picks remains a
scout. It reaches the first clustered result in 20-25 time units, tying the static density
hint and beating FIFO, while uniform completion regresses only 0.6%.

**Re-audited verdict: reject.** Completion is 1,821 versus the best baseline's 1,811 on
clustered work, 1,825 versus 1,818 on moving clusters, and 6,899 versus 6,906 on the costly
UDF. Those changes are within 1%, far below the 15% completion gate; discovery latency
alone does not validate the mechanism.

### 2026-08-15 re-audit — e49 schooling concurrency

A 16-slot discrete scheduler now completes all 160 queries and records each arrival,
start, progress, and finish. Schooling retains 99-100% of the best throughput and Jain
fairness stays 0.940-0.989, but its p95 slowdowns are 82.25, 54.85, and 84.13. Shortest-job
first reaches 14.85, 12.33, and 15.59 on the same exact workload checksums.

**Re-audited verdict: reject decisively.** Local cohesion behaves like equal sharing and
cannot approach the strongest baseline's tail slowdown; the required 20% improvement is
missed by multiples, even before the schooling loop's 2.5-3x bookkeeping time.

### 2026-08-15 re-audit — e56 vascular memory

Four allocators now spend one hard 1,000-unit budget across executed marginal-utility
curves, with assertions for the 100-unit correctness floor and cap. Vascular adaptation
recovers five epochs after reversal and beats the strongest baseline on reversal (59% less
spill, 63% lower p95) and bursts (35%/35%). On stable curves, however, static weights are
already optimal: vascular spill is 40.75M versus 40.70M and p95 is 2,484 versus 2,482.

**Re-audited verdict: reject.** The adaptive state is useful after drift, but does not
clear the 20%/15% conjunction against the strongest control on stable demand. This is an
argument for change detection around static weights, not unconditional conductance.

### 2026-08-15 re-audit — e58 saturation batch sizing

Each policy now chunks and checksums the same 262,144 values. Offline choice enumerates
nine sizes; hill climb and saturation make executable cost probes whose work is charged.
Saturation selects the offline batch for all five shapes and stays within 3% modeled
runtime and identical peak memory. It nevertheless takes six probes to rediscover the
unchanged 4,096-row filter batch (with no subsequent saving) and seven for decode/join.

**Re-audited verdict: reject.** Steady-state selection clears the 5%/10% bounds, but the
adaptation cannot repay inside five batches on the fixed-size control and misses the probe
budget outright. Hill climb is no worse and usually probes less.

### 2026-08-15 re-audit — e60 selective buffer recycling

The replacement allocates real byte buffers, zeroes and verifies every reused range, writes
request contents, and checksums them identically. Selective recycling cuts allocated bytes
by more than 99% and modeled p95 allocation work by 89% across alternating, pressure, and
churn traces under a measured 4 MiB resident cap. Actual medians improve only 2-4% because
mandatory initialization dominates. On churn, resident slack reaches 13.9% and retained
fragmentation grows by 360,448 bytes.

**Re-audited verdict: reject.** Correct initialization and allocation reductions pass, but
the churn trace breaches both the 10% slack ceiling and no-growth fragmentation guardrail.
The simpler size-class pool also allocates fewer bytes.

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

### e23 follow-up — profiling the 9 seconds found a 1.6× scan win

Sampling the probe during its scan phase attributed **41% of scan wall time to a
single `filter().count()`** inside `read_block_if_with_budget`: every block of
every column validated its declared null count by testing **one row at a time**,

```rust
let actual_nulls = (0..row_count)
    .filter(|index| null_bitmap[index / 8] & (1 << (index % 8)) != 0)
    .count();
```

This is a corruption check, and it ran even for blocks the predicate was about
to skip. Replacing it with a per-byte popcount (masking the trailing byte to the
bits the row count covers, so a corrupt tail still fails exactly as before):

| projection | before | after | gain |
|---|---:|---:|---:|
| amount only | 9055 ms | **5509 ms** | 1.64× |
| amount + day | 10622 ms | 6396 ms | 1.66× |
| status only (dictionary) | 10279 ms | 6308 ms | 1.63× |
| all five columns | 15403 ms | 11873 ms | 1.30× |

**A 20-line change with identical semantics beat the entire encoding programme
by two orders of magnitude** — 39% off a single-column scan, against 0.14% for
the FastLanes layout and 0.06% for dropping LZ4. The standing rule earned its
place again: profile the engine before rewriting the format.

The remaining ~5.5 s for one column of 20M values is still ~200× the raw decode
kernel, so the profile should be repeated now that this dominator is gone.

## e24 — Where the scan time actually goes (post-popcount attribution)

15-second sample of `scan_probe` during its scan phase, 20M rows, 60 segments,
after the null-bitmap popcount fix. Self time, "sort by top of stack":

| self samples | symbol | area |
|---:|---|---|
| 1809 | `read` (syscall) | file I/O |
| 984 | `_xzm_free` | allocator |
| 587 | `xxh3_64_long_default` | per-block checksum |
| 578 | `segment::read_projected_rows` | scan driver |
| 547 | `SegmentRowStream::next_row` | row merge path |
| 361 + 341 | `_malloc_zone_malloc`, `_xzm_xzone_malloc` | allocator |
| 315 + 281 | `_free`, malloc internals | allocator |
| 269 + 262 | `__bzero`, `_platform_memset` | allocation zeroing |
| 267 | `_platform_memmove` | copies |
| — | `Vec<Cell>::push`, `Vec<Value>` from_iter, `Vec<KeyPart>` clone | per-row materialization |
| 154 | `codec::decode_key` | key decode |

**Allocation is the largest software cost: ~2800 samples (~21%)** across
malloc/free plus the zeroing that accompanies it — more than I/O's 13%, and
five times the checksum. It is driven by per-row materialization: a `Vec<Cell>`
per block, a `Vec<Value>` per row, and a cloned `Vec<KeyPart>` per key.

**Bit-unpacking is 101 samples — 1.4%.** This is the third independent
confirmation that encoding work cannot pay here: e23 sized the candidates at
0.14% and 0.06% of a scan, and the profile now shows the kernel they would
improve is a rounding error against the allocator.

Ranked targets:

1. **Per-row allocation churn** (~21%). Reuse buffers across rows and blocks
   instead of allocating a fresh `Vec` per row; the merge path clones
   `Vec<KeyPart>` per key where a borrow would do.
2. **I/O pattern** (13%). `FileDecoder::read_exact` appears twice in the hot
   tree; block-at-a-time reads may be coalescable.
3. **Checksum** (4%). A correctness feature, not removable, but it is verified
   on every block read including blocks a predicate then skips.

Notably the scan reaches decoded columns through *two* paths —
`read_projected_rows` and a `SegmentRowStream::next_row` row-merge path — and
the second is 19% of the subtree. Understanding why a projected column scan
falls into a row-oriented merge is its own question.

### e24 follow-up — a block with no nulls no longer pays for the null splice

`read_block_if_with_budget` decoded a block's values into one `Vec<Cell>` and
then built a *second* `Vec<Cell>` to interleave `Cell::Null` at the bitmap's set
positions. When a block has no nulls the second vector is a pure copy of the
first, and non-nullable columns are the common case.

Returning the decoded vector directly when `actual_nulls == 0` (with an explicit
length check, so a corrupt block still fails rather than silently truncate):

| projection | before | after | gain |
|---|---:|---:|---:|
| amount only | 5513 ms | **4954 ms** | 1.11x |
| amount + day | 6338 ms | 5857 ms | 1.08x |
| status only (dictionary) | 6312 ms | 5927 ms | 1.06x |
| all five columns | 11863 ms | 10731 ms | 1.11x |

Cumulative against the pre-popcount baseline, single-column scan:
**9055 ms to 4954 ms, 1.83x**, from two changes totalling about forty lines and
no format change.

### e24 follow-up 2 — flushed segments are unique-keyed, so scans stop merging

`flush()` hardcoded `unique_keys = false`. The memtable is a
`BTreeMap<PrimaryKey, StoredRow>`, so a flushed segment provably holds one row
per key — and the scan classifier only takes its columnar `Direct` path when a
single-segment cluster is `all_unique`. Every scan over flush-produced segments
therefore fell into `ScanPart::Merge` and materialised `Cell`s row by row.

A Codex trace settled the safety question: the streaming `Direct`/`DirectRange`
paths apply **no tombstone filter**, so `unique_keys` promises tombstone-freedom
as well as key uniqueness. The correct predicate is
`rows.iter().all(|row| !row.is_deleted())`, not `true`.

| projection | before | after | gain |
|---|---:|---:|---:|
| amount only | 4954 ms | **714 ms** | 6.9× |
| amount + day | 5857 ms | 967 ms | 6.1× |
| status only (dictionary) | 5927 ms | 764 ms | 7.8× |
| all five columns | 10731 ms | 1820 ms | 5.9× |

**REVERTED — see below.** The `unique_keys` change was backed out; the banked
cumulative figure is **9055 ms → 4954 ms (1.83×)** from the popcount and
null-splice fixes alone.

The direct path decodes a whole segment in **one reservation**, so a query with
a small ceiling that previously streamed through the chunked merge path fails
outright: `MemoryLimitExceeded { requested: 263280, limit: 65536 }` in
`storage::tests::key_pruning_requires_an_exact_declared_numeric_mapping` and two
siblings. Three attempts to add a fallback all missed the real call site — the
failure arrives through `next_column_chunks` prefetch, not the paths I patched —
so the change is off until the direct path can size its work to the budget.
That is the prerequisite, and it is real work, not a guard.

Had it held, the figure would have been: None of them changed the file format; all three came from
profiling rather than from the encoding programme this investigation started
with, whose best candidate was worth 0.14%.

Five boundary cases pin the flag (`tests/suite/direct_scan.rs`): a flush
carrying a tombstone must not resurrect it, a tombstone-free flush returns
exactly its rows, overlapping unique segments still merge to the newer version,
a memtable tombstone still suppresses a segment row, and the classification
survives close and reopen since the flag is persisted in the manifest.

### e24 follow-up 3 — profile after the Direct path: no single dominator left

Re-sampled with the columnar path live (scan of one column now 821 ms):

| self samples | symbol | area |
|---:|---|---|
| 696 | `read` (syscall) | file I/O |
| 601 | `_xzm_free` | allocator |
| 394 | `decode_int_payload_into` | real decode work |
| 339 | `_platform_memmove` | copies |
| 291 | `xxh3_64_long_default` | per-block checksum |
| 253 | `read_projected_rows` | scan driver |
| 221 + 206 | `malloc` internals | allocator |
| 194 | `unpack` | bit-unpacking |

The shape has changed qualitatively. Before, one loop was 41% and a second was
19%; now the top cost is the read syscall and the third is the decode kernel
doing its actual job. Allocation is still visible (~1000 samples across
malloc/free) but it is spread across `Vec<Value>` construction rather than
concentrated anywhere a targeted fix could remove.

**Stopping here deliberately.** The remaining candidates are I/O (inherent),
the checksum (a correctness feature), and diffuse allocation whose removal
would mean changing the API shape that returns `Vec<Value>` rows. None of them
resemble the two structural mistakes that produced the 12.7× — those were a
per-row loop where a popcount belonged and a flag that was never set.

One number worth carrying forward: row materialization now costs as much as the
entire columnar scan (821 ms → 1615 ms for the same data with `next_chunk`).
That is the exec layer's "value tax" from e15, not storage, and it is where the
next scan-side work belongs.

## e25 — 200M rows: the first evidence above the 20M ceiling

Every scale claim until now was extrapolated from 20M rows. This is 200M —
10× further — on a 16-core Linux host with 30 GB RAM, writing to a dedicated
1.9 TB volume (not the system SSD). Append mode, 64 MB memtable, engine
defaults.

| rows | segments | manifest B | disk B | B/row | rows/s |
|---:|---:|---:|---:|---:|---:|
| 20M | 57 | 31,535 | 326,274,630 | 16 | 217,051 |
| 60M | 174 | 96,119 | 939,008,296 | 15 | 216,620 |
| 100M | 290 | 160,151 | 1,551,737,926 | 15 | 215,900 |
| 140M | 406 | 224,183 | 2,165,376,017 | 15 | 216,172 |
| 200M | 581 | 320,783 | 3,074,511,160 | 15 | 216,639 |

Ingest 923.3 s. Full scan of five columns 258.4 s (774k rows/s); two columns
136.2 s (1.47M rows/s).

### What holds

**Ingest throughput is flat.** 217,051 rows/s at the 20M mark and 216,639 at
200M — a 0.2% drift across a 10× increase in table size. This was the open
question after the compaction fix: compaction now actually runs, and the fear
was that its cost would grow with the table until ingest degraded. It does not.

**Everything grows linearly, nothing compounds.** Segments 57 → 581 (10.2× for
10× rows), manifest 31,535 → 320,783 B (10.2×), disk 9.4× — very slightly
sublinear, because compaction is consolidating. Bytes per row is steady at 15.

**Extrapolated to 1e9 rows:** ~2,905 segments and a 1.6 MB manifest over roughly
15 GB. The manifest is rewritten on every flush, so it was the suspected
quadratic; at 1.6 MB it is not a problem.

### What does not

**Scan is the ceiling, and it is the thing we reverted.** 774k rows/s over five
columns means a full 1e9-row scan takes about 21 minutes. This run took the
row-merge path for every segment, because `unique_keys` on flushed segments is
currently off — the change measured at 6.9× on a 20M scan and reverted for a
memory-ceiling regression. At 200M the same path is the dominant cost, which
moves that fix from a nice optimisation to the single highest-value item on the
board.

### Honest limits of this result

Append-only, one table, one column shape, no concurrent queries, no CDC applying
updates underneath. It says the storage layer's growth curves are linear and
ingest holds; it does not say a terabyte-scale replica under live replication
behaves the same. The next questions are churn mode at this size and a scan
under a predicate rather than a full table read.

## e26 — `unique_keys` on flushed segments, enabled and measured

The one item e25 named as the ceiling. A flushed segment provably holds one
row per key, because the memtable is a map; marking it `unique_keys` lets the
scan classifier take the columnar direct path instead of merging row by row.
It was measured at 6.9× and reverted, because the direct path decoded a whole
segment in one reservation and a query with a small ceiling that used to
stream through the chunked merge path failed outright
(`MemoryLimitExceeded { requested: 263280, limit: 65536 }`).

That blocker is gone. Sizing the direct decode to the query's remaining budget
removed the failure, and the storage suite now passes with the flag on.

Both runs below are the same host, same session, same 20M-row dataset, same
60 segments and 224,867,282 bytes on disk — only the flag differs. `columns`
is the columnar path; `+rows` also materializes per-row values.

| Query | merge (off) | direct (on) | speedup |
| --- | ---: | ---: | ---: |
| amount only | 5145.1 ms | 791.8 ms | 6.50× |
| amount + day | 6075.6 ms | 1016.8 ms | 5.98× |
| status only (dict) | 6296.3 ms | 825.6 ms | 7.63× |
| all five columns | 10872.9 ms | 1884.3 ms | 5.77× |
| all five, `+rows` | 12541.4 ms | 4551.2 ms | 2.76× |

The dictionary column gains most (7.63×) and the row-materializing variant
least (2.76×), which is what the shapes predict: the merge path's cost is per
row, so removing it helps most where the per-row work that remains is
smallest. Ingest is untouched (156k rows/s in both runs) — the flag is one
`all(...)` over rows the flush already holds.

### The correctness condition

`unique_keys` promises two things, not one: one row per key **and** no
tombstones, because the direct path applies no tombstone filter. A flush
carrying a delete would resurrect it. So the flag is
`rows.iter().all(|row| !row.is_deleted())`, and `tests/suite/direct_scan.rs`
pins the boundary — every case there passes with the flag hardcoded false,
which is the point: what they catch is the flag being set when it must not be.

## e27 — adaptive LZ4 must be per block, not per encoding

Question: should PTSEG remove LZ4 globally, disable it for packed encodings,
or retain it only when the compressed block is materially smaller?

The harness reproduces the current production payload bytes rather than an
abstract codec: 16,384-row blocks; FOR and delta bit-packing with the same
base/width/length framing; dictionary strings followed by fixed-width u32
codes; and plain Float64/UTF-8 payloads. Each shape contains 4,194,304 rows
(256 blocks). Always-LZ4, never-LZ4, any-saving, and 5%-saving policies decode
to the same position-sensitive checksum. Shared block framing is excluded.

Run locally with:

```bash
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo run --manifest-path experiments/Cargo.toml --release -p e27-adaptive-compression
```

### Size

| PTSEG payload shape | never LZ4 | always LZ4 | adaptive | LZ4 blocks |
|---|---:|---:|---:|---:|
| FOR bit-packed amount | 10,489,088 B | 10,529,817 B | **10,489,088 B** | 0/256 |
| Delta-bit-packed primary key | 527,616 B | **7,929 B** | **7,929 B** | 256/256 |
| Mixed FOR + delta blocks | 5,508,352 B | 5,268,869 B | **5,248,510 B** | 128/256 |
| Dictionary status, cyclic | 16,792,320 B | **88,832 B** | **88,832 B** | 256/256 |
| Dictionary region, random | 16,797,184 B | **5,917,623 B** | **5,917,623 B** | 256/256 |
| Plain random Float64 | 33,554,432 B | 33,686,272 B | **33,554,432 B** | 0/256 |
| Plain high-cardinality UTF-8 | 159,383,552 B | **94,518,596 B** | **94,518,596 B** | 256/256 |

The any-saving and 5%-saving policies made identical choices on every block.
On the mixed integer control, adaptive is the only policy that avoids both the
FOR expansion and the loss of delta compression. It is smaller than either
global policy.

### Decode median on Apple M2 Pro

| Shape | never | always | adaptive |
|---|---:|---:|---:|
| FOR amount | 12.119 ms | 12.686 ms | **12.123 ms** |
| Delta primary key | 0.614 ms | 0.612 ms | **0.609 ms** |
| Mixed FOR + delta | 6.366 ms | 6.481 ms | **6.302 ms** |
| Dictionary status | **19.224 ms** | 23.003 ms | 22.788 ms |
| Dictionary region | **19.717 ms** | 25.108 ms | 25.140 ms |
| Random Float64 | **38.883 ms** | 41.250 ms | 39.800 ms |
| High-cardinality UTF-8 | **188.543 ms** | 215.285 ms | 214.566 ms |

Trying LZ4 and then selecting the representation costs the same as always-LZ4
within noise; it saves read/decompression work only on rejected blocks. The
dictionary/text decompression tax is real (roughly 14-27%), but it buys 41-99.5%
fewer payload bytes. FOR and random-float blocks receive neither benefit, so
adaptive correctly serves them raw.

### Verdict

**Global LZ4 removal is rejected.** It would inflate delta-packed primary-key
blocks by 66x, cyclic dictionary blocks by 189x, random dictionary blocks by
2.8x, and the tested high-cardinality text by 1.7x.

**“Never compress packed integers” is also rejected.** Uniform FOR blocks are
incompressible, but delta-packed monotonic blocks compress to 1.5% of their
encoded size. Encoding kind alone is not a sound selector.

### Decode median on Linux x86-64 (8 CPU / 8 GB)

| Shape | never | always | adaptive |
|---|---:|---:|---:|
| FOR amount | **8.950 ms** | 9.354 ms | **8.949 ms** |
| Delta primary key | **0.449 ms** | 0.472 ms | 0.471 ms |
| Mixed FOR + delta | **4.695 ms** | 4.912 ms | 4.716 ms |
| Dictionary status | **14.405 ms** | 21.750 ms | 21.749 ms |
| Dictionary region | **14.429 ms** | 19.707 ms | 19.685 ms |
| Random Float64 | **28.978 ms** | 30.949 ms | 29.072 ms |
| High-cardinality UTF-8 | **138.524 ms** | 167.725 ms | 167.567 ms |

The Linux run made exactly the same per-block decisions as Apple and every
raw/LZ4/adaptive decode arm returned the same checksum. Trying LZ4 before
selection was within 0.2% of always-LZ4 encode time on every shape.

**Per-block try-and-keep is supported on both required targets and adopted in
PTSEG v3.** A 5% threshold produced the same choices as the pure size winner.
Compression tag `0` carries exact-length raw payloads, old LZ4/zstd tags remain
readable, and cold full-merge output remains zstd. Mixed raw/LZ4 reopen,
legacy-codec decode, and corrupt raw-length coverage pin the boundary.

The sub-15% decode differences remain performance ties under rule 3; they are
not the adoption claim. PTSEG v3 is the narrower storage-policy exception:
normal-tier compression must not expand a block, and the same local choice must
retain the 41-99.5% reductions on compressible payloads. The 5% threshold adds
hysteresis so marginal byte savings do not impose decompression. Both targets
selected exactly the same blocks, so this exception is not ISA-specific.

## e28 — FastLanes in the current real PTSEG scan path

The earlier e23 estimate assigned FastLanes roughly 13 ms out of a 9,055 ms
five-column scan: 0.14%. That denominator predates the direct columnar path and
its storage fixes, so this experiment measures the layout inside today's real
writer and reader instead of carrying the estimate forward.

The temporary variant replaced PTSEG's horizontal LSB-first bitstream with the
FastLanes 1a layout for every complete 1,024-value chunk: 16 lanes, 64 slots per
lane, and `width` interleaved virtual-register rows. A final partial chunk kept
the horizontal representation. Framing, integer normalization, block choice,
checksums, LZ4, file I/O, and typed column construction were unchanged. The
variant passed all 81 `pintail-store` tests and strict clippy, then was removed;
no format code from the experiment remains in the engine.

The probe generated the same deterministic 20M-row table for each run, closed
and reopened it, and scanned real PTSEG files. Each `columns` and `+rows` result
is the median of three scans. A/B/A ordering brackets the variant with two
horizontal runs to expose session-level drift.

Run locally on Apple M2 Pro with:

```bash
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo run --release -p pintail-store --example scan_probe -- --rows 20000000
```

### Column scan median

| Projection | horizontal A1 | FastLanes B | horizontal A2 | A mean | B vs A mean |
|---|---:|---:|---:|---:|---:|
| amount | 831.8 ms | **745.3 ms** | 770.6 ms | 801.2 ms | **-7.0%** |
| amount + day | 1006.9 ms | **905.0 ms** | 976.9 ms | 991.9 ms | **-8.8%** |
| status dictionary control | 823.1 ms | 815.3 ms | 834.8 ms | 829.0 ms | -1.7% |
| all five columns | 1809.8 ms | **1636.2 ms** | 1829.0 ms | 1819.4 ms | **-10.1%** |

### Row-materializing scan median

| Projection | horizontal A1 | FastLanes B | horizontal A2 | A mean | B vs A mean |
|---|---:|---:|---:|---:|---:|
| amount | 1470.3 ms | **1387.7 ms** | 1455.6 ms | 1463.0 ms | **-5.1%** |
| amount + day | 1891.8 ms | **1800.9 ms** | 1860.9 ms | 1876.4 ms | **-4.0%** |
| status dictionary control | 1945.9 ms | 1958.8 ms | 1936.7 ms | 1941.3 ms | +0.9% |
| all five columns | 4361.2 ms | **4160.4 ms** | 4355.2 ms | 4358.2 ms | **-4.5%** |

The two horizontal runs produced exactly 224,867,282 bytes across 60 segment
files. FastLanes produced 223,981,934 bytes, 0.39% less, because LZ4 sees a
different byte order even though the uncompressed bit count is identical.
Load time fell from a 128.5 s A mean to 111.6 s (-13.2%); replacing the current
bit-at-a-time packer contributes on the write side too.

### Verdict

**The old 0.14% estimate is rejected, but a FastLanes format change is not
justified.** On the current direct path the layout saves 7-10% for columnar
numeric scans and about 4.5% once five columns become rows. The dictionary-only
control is flat, which supports attributing the numeric gain to packing rather
than a generally faster middle run. The layout and migration cost are real,
while every measured gain remains below the lab's 15% adoption threshold.
PTSEG therefore keeps the simpler horizontal packing. The later v3 compression
tag from e27 does not alter this layout decision.

This is local evidence only; the Linux reference run was unavailable. That does
not block the rejection: a sub-threshold win on one required target cannot make
FastLanes the cross-target winner under the experiment rules. Revisit only if a
fused predicate/aggregate kernel can consume the interleaved representation
without materializing decoded values, because that is a different benefit than
the format-only change measured here.

## e29 — metadata accelerators without captured demand

Question: should Pintail persist grouped SMA sub-cubes or a cache of blocks
covered by predicates that zone maps cannot express before a real workload asks
for them?

The throwaway harness used the engine's 16,384-row block size over 20M
deterministic rows (1,221 blocks). Every competing path returned the same
position-sensitive checksum. Timings are median-of-seven 32-query batches on
Apple M2 Pro; batching keeps sub-millisecond metadata folds out of timer noise.
The harness was deleted after recording the result.

### Grouped SMA sub-cubes

This is the strongest possible case: stable dense dictionary codes, implicit
keys, only `COUNT` and `SUM`, and no predicate, join, DISTINCT, expression, NULL
mapping, CDC overlay, or dictionary-identity work. One aggregate cell is two
eight-byte values. Cubes were built per block and folded across blocks.

| Fixed cube | cells/block | build once | metadata | full scan, 32 queries | cube fold, 32 queries | speedup |
|---|---:|---:|---:|---:|---:|---:|
| status | 5 | 7.6 ms | 97,680 B | 129.638 ms | **2.303 ms** | **56.3x** |
| region x status | 40 | 7.2 ms | 781,440 B | 146.659 ms | **2.826 ms** | **51.9x** |
| status x payment x fulfillment | 80 | 9.2 ms | 1,562,880 B | 186.834 ms | **2.881 ms** | **64.9x** |

All three together cost 2,442,000 bytes (0.122 B/row) and about 24 ms to build
in this idealized in-memory model. The performance lever is real. The product
problem is coverage: every persisted combination answers only its exact group
dimensions and compatible aggregates, while metadata grows with the product of
dimension cardinalities.

The repository's ten-query production-shaped commerce workload contains zero
queries directly answerable by these fixed cubes. Its grouped queries also
carry tenant/date/deletion predicates, joins, DISTINCT, conditional aggregates,
or derived time buckets. Adding all of those dimensions would no longer be the
small low-cardinality structure measured here. Synthetic gate Q3/Q4 and novel
N2 are foldable, but repeated settled executions already use the generation-
keyed memo; their remaining niche is a recurring cold query on a continuously
changing replica.

### Predicate-covered-block cache

The predicate was deliberately cheap: one precomputed flag byte per row. This
is conservative for cache benefit because LIKE or JSON evaluation would make a
skipped block more valuable. The cold path scans all blocks and builds one
160-byte bitmap per normalized predicate; the warm path reevaluates only blocks
that contained at least one match. It caches block coverage, never query rows or
answers.

| Match topology | blocks covered | full, 32 queries | cold build, 32 queries | warm, 32 queries | warm vs full |
|---|---:|---:|---:|---:|---:|
| scattered 0.1% | 1,221/1,221 (100%) | 46.474 ms | 46.335 ms | 48.854 ms | **5.1% slower** |
| scattered 0.001% | 186/1,221 (15.2%) | 44.163 ms | 43.931 ms | **8.317 ms** | **5.31x** |
| clustered 1% | 13/1,221 (1.1%) | 44.590 ms | 44.931 ms | **6.009 ms** | **7.42x** |
| matches in 10% of blocks | 123/1,221 (10.1%) | 52.441 ms | 49.627 ms | **11.645 ms** | **4.50x** |

Cold construction costs essentially one ordinary scan, so the second identical
query amortizes it when coverage is sparse. Selectivity alone is insufficient:
even 0.1% scattered matches touch every 16K-row block and make the cache a net
loss. Block coverage is the admission statistic that matters.

The production-shaped workload contains no LIKE or JSON-path filter. Its IN and
status predicates select common values scattered through the data, the 100%-
coverage case where this cache does not help. There is therefore no captured
reuse-plus-sparse-coverage workload to optimize today.

### Verdict

**Defer both features, with measured re-entry gates.** Grouped sub-cubes become
eligible when a captured recurring query misses its latency goal while the
generation memo is routinely invalidated, and its exact low-cardinality group
key plus aggregates are stable enough to name one bounded cube. Predicate-
covered-block caching becomes eligible when instrumentation observes the same
normalized non-zone-map predicate at least twice per immutable generation and
its first scan covers no more than roughly 15% of blocks. Until then, both add
persistent state, invalidation rules, and format surface for synthetic wins the
current workload cannot consume.

This is local evidence only, but no production path is selected: the experiment
defines demand gates rather than an ISA-sensitive kernel winner. Persistent
per-segment SMAs, zone-map pruning, and the generation-keyed memo remain the
smaller mechanisms for the workloads Pintail currently has.

## 2026-08-12 biomimetic program — Wave 1

Wave 1 executed the five adaptive-metadata simulations selected in
`NEXT_50.md`: e32, e33, e34, e43, and e77. All returned identical exact
answers across policies. Policy metrics are deterministic and reproduced on
Apple M2 Pro and the pinned 8-CPU/8-GiB Linux target for the two candidates
that cleared their local advancement gate. Elapsed simulator timings are
reported only where the simulated loop actually represents the proposed work;
modeled avoided I/O or unequal predicate costs are not mislabeled as elapsed
speedups.

### e32 — Bone-remodelled sparse indexes

All policies spent exactly 4,096 pivots over 16,777,216 rows. Stress allocation
used decayed rows-inspected feedback and hysteresis; frequency allocation used
undecayed observations.

| Trace | Fixed decoded / p95 | Frequency decoded / p95 | Remodeling decoded / p95 |
|---|---:|---:|---:|
| uniform points | 81,920,000 / 4,096 | 89,164,872 / 5,462 | 88,286,160 / 5,462 |
| stationary 80/20 hotspot | 81,920,000 / 4,096 | **49,231,311** / 6,554 | 57,717,077 / 6,554 |
| moving hotspot | 327,680,000 / 4,096 | 239,861,379 / 7,282 | **218,259,835** / 6,554 |
| 10% full scans | 33,527,521,280 / 16,777,216 | 33,535,306,309 / 16,777,216 | 33,534,209,502 / 16,777,216 |

**Verdict: reject.** Remodeling cuts cumulative point work 33% for the moving
hotspot, but its fixed byte budget funds hot pivots by making cold gaps wider.
The 20% cold tail therefore raises point p95 by 60%, directly failing e32's
primary gate. It also moves 161,692 pivots over the moving trace versus 33,696
for the simpler frequency policy. This is not a tuning miss: improving tail
seeks under an unchanged pivot count requires a different objective, such as a
hard maximum-gap constraint, which sharply limits hotspot gains.

### e33 — Leaf-venation metadata

The real isolated kernel evaluated 160 range predicates over 1,024 blocks and
then counted exact matching rows from every candidate block.

| Shape | Fixed probes / median | Complete hierarchy | Selective venation |
|---|---:|---:|---:|
| clustered | 163,840 / 0.367 ms | 5,076 / 0.422 ms | **6,072 / 0.327 ms** |
| partly clustered | 163,840 / **2.242 ms** | 246,014 / 3.229 ms | 175,360 / 2.314 ms |
| scattered | 163,840 / **7.596 ms** | 327,518 / 8.653 ms | 175,360 / 7.708 ms |

Selective venation used 1,096 summaries versus 1,024 leaf maps; the complete
tree used 2,047. Candidate blocks and 218k exact matches agreed in every arm.

**Verdict: reject.** The selective hierarchy eliminates 96% of metadata probes
on clustered data, but transfers to only a 10.9% elapsed win, below the 15%
rule. Once scattered blocks make coarse bounds unselective, the hierarchy adds
probes and loses. No Linux run is needed to reject a candidate already below
the required local margin.

### e34 — Root-foraging micro-indexes

Twenty of 256 value patches could own a rootlet. Work includes rows scanned by
queries and rows read to build every index. An exact histogram supplied the
same equality count to all policies.

| Trace | Scan work | Global index | Frequency rootlets | Local+systemic rootlets |
|---|---:|---:|---:|---:|
| stationary | 32.00B | **4.48M** | 5.72B | 5.74B |
| moving | 128.00B | **5.96M** | 23.83B | 23.76B |
| decoy then stable | 128.00B | **5.94M** | 18.94B | 19.05B |

The first root-controller run exposed an incumbent-feedback bug: it scored
only realized savings, so an absent rootlet could never demonstrate value.
After correction to counterfactual savings, its query work matched frequency
allocation but its faster decay caused 4.7-5.9x as much build work on shifting
traces (29-31M build rows versus 5-6M).

**Verdict: reject this controller.** Rootlets do beat scans under a strict
auxiliary-byte cap, but the biological local+systemic rule adds churn without
beating simple decayed frequency. A full index is the performance ceiling and
repays its one 4M-row build almost immediately in these repeated traces; future
work would need a real storage budget and low-reuse trace where the full index
is infeasible.

### e43 — Lateral-inhibition predicate ordering

Four exact predicate truth columns carried calibrated evaluation costs
`[1, 4, 13, 3]`. The candidate samples 64 rows per 4,096-row block (1.57% of
rows), then greedily chooses marginal newly-rejected rows per cost. “Hindsight”
enumerates all 24 static orders and does not charge enumeration to the reported
work, making it a deliberately strong baseline.

| Shape | Best static work | Per-block marginal work | Delta |
|---|---:|---:|---:|
| independent | **4,704,825** | 4,896,916 | +4.1% |
| correlated rejects | **6,925,765** | 7,013,902 | +1.3% |
| anti-correlated rejects | **6,695,676** | 6,834,100 | +2.1% |
| blockwise regime reversals | 4,972,925 | **3,743,805** | **-24.7%** |

The modeled work and exact result are identical across architectures. The
simulator's reversal-shape elapsed loop improved from 1.003 to 0.543 ms locally
and 0.913 to 0.600 ms on Linux. Other elapsed ratios are not evidence for the
calibrated cost model because reading a precomputed Boolean costs the same for
all four simulated predicates.

**Verdict: simulation gate passes; engine claim remains unvalidated.** It clears
the 20% heterogeneous-data win, 2% sample cap, and 5% stable-shape guardrail on
calibrated work on both targets. Advance to a real typed-kernel experiment with
cheap comparisons, JSON/string predicates, and actual cycle accounting. Do not
add per-block reordering to Pintail from this simulation alone.

### e77 — Retinal variable-resolution granules

Every layout used exactly 64 granules over 1,048,576 rows. The candidate learned
decayed query heat plus update-overlap signals and bounded boundaries around the
fixed layout. Reported work is rows in every granule touched by an exact range;
the simulator does not pretend those modeled rows were physically decoded.

| Trace | Fixed decoded | Static entropy | Heat only | Bounded foveation |
|---|---:|---:|---:|---:|
| stationary hotspot | 82,853,888 | **35,012,608** | 35,345,408 | 59,003,904 |
| moving hotspot | 331,808,768 | 431,145,984 | **155,657,216** | 237,814,784 |
| uniform wide scans | **409,600,000** | 445,338,624 | 410,920,960 | 409,790,464 |
| random narrow probes | **83,116,032** | 120,266,752 | 84,077,568 | 83,990,528 |

Linux reproduced every modeled count. Bounded foveation cuts decoded rows 28.8%
on the stationary hotspot and 28.3% on the moving hotspot; hostile wide scans
rise 0.05% and random probes 1.05%. Static entropy wins the known stationary
case but becomes 30% worse than fixed when the hotspot moves. The candidate's
p95 touched granule grows from 16K to 20-21K on hotspot traces, an explicit
tail tradeoff.

**Verdict: simulation byte gate passes; format claim remains unvalidated.** The
equal-metadata and hostile-work gates pass on both targets, but this model does
not measure PTSEG write throughput, metadata search, decompression, or the cost
of producing variable blocks during immutable writes. Advance to a real PTSEG
reader/writer A/B test. Do not change the 16,384-row format default yet.

## 2026-08-12 biomimetic program — Wave 2

Wave 2 executed e38, e46, e50, e55, e59, and e63. Every policy drained or
completed the identical deterministic workload and compared a sorted exact
completion set or logical query checksum. e59 and e63 cleared their local gates
and reproduced on the pinned Linux target; the four local failures did not
consume shared-host time.

### e38 — Fever-mode overload control

The controller entered a hysteretic conservative mode when CDC lag or the query
queue crossed a high watermark and exited only below separate low watermarks.

| Trace | Policy | Query p99 | Max CDC lag | Overflow ticks | Makespan |
|---|---|---:|---:|---:|---:|
| mild | unconstrained / fixed / fever | 1 / 1 / 1 | 0 / 0 / 0 | 0 / 0 / 0 | 12,000 each |
| sustained | unconstrained | **1,365** | 51,497 | 12,782 | **13,092** |
| sustained | fixed / fever | 3,587 / 3,511 | 0 / 15 | 0 / 0 | 13,921 / 13,878 |
| spike | unconstrained | **4,064** | 35,002 | 7,333 | **12,000** |
| spike | fixed / fever | 4,893 / 4,887 | 2 / 217 | 0 / 0 | 12,314 / 12,308 |

**Verdict: reject.** Fever protects CDC, but query p99 is 157% worse during
sustained overload and 20% worse during the spike than unconstrained execution.
Against the safe fixed policy it improves p99 by only 0.1-2.1%, far below the
15% rule. Two mode changes show hysteresis prevented flapping; stability alone
does not make the policy useful.

### e46 — Quorum-sensing compaction

The table model had 128 key intervals. Updates added overlapping physical
fragments, queries paid all fragments, and maintenance could compact two
intervals per 100 operations. Append-only fragments were provably disjoint.

| Trace | Fixed count total bytes / p99 | Global pain score | Local quorum |
|---|---:|---:|---:|
| recent-hot | **34.8B / 1.5M** | 36.0B / 2.6M | 1,683.3B / 153.0M |
| moving-hot | **34.6B / 1.5M** | 35.9B / 2.5M | 418.4B / 38.4M |
| scattered | **34.8B / 1.4M** | 36.2B / 1.8M | 169.9B / 15.1M |
| append-only | 2.88B / 65K | 2.88B / 65K | 2.88B / 65K |

**Verdict: reject decisively.** The local quorum reinforces a busy neighborhood
but repeatedly selects the wrong member of that neighborhood; the exact painful
interval accumulates fragments. Read work becomes 4.9-48x worse than fixed-count
compaction. This is a causal failure of the transferred mechanism, not a
threshold miss. Compaction needs exact interval benefit, not bacterial consensus.

### e50 — Termite-mound maintenance ventilation

The coupled aperture observed maintenance debt, query backlog, CDC lag, and
noisy capacity. It was compared with fixed 15% maintenance and a debt-only PID.

| Trace | Fixed debt / queue area | Debt PID | Coupled ventilation |
|---|---:|---:|---:|
| periodic | 468,590 / 0 | 468,590 / 0 | 468,590 / 0 |
| spike | 468,590 / 214.8M | 2,661,032 / **88.9M** | **468,590** / 107.0M |
| noisy capacity | 452,898 / 0 | 452,877 / 21 | 452,887 / 11 |

**Verdict: reject.** Coupling protects debt during the spike and halves the
fixed controller's query backlog, but the simpler PID has 17% less queue area
and nearly identical SLO violations. Periodic and noisy controls are ties. The
extra multi-signal controller has no qualifying win.

### e55 — Stomatal prefetch gates

Fixed depths 1/4/16/64 and a hysteretic adaptive aperture paid request latency,
fetched blocks, wasted blocks, and pressure-dependent resident-memory cost.

| Trace | Offline-best fixed cost | Stomatal cost | Delta | Reversals |
|---|---:|---:|---:|---:|
| sequential | 5,606,432 | 5,608,296 | +0.03% | 0 |
| aggressively pruned | **635,492** | 938,564 | +47.7% | 153 |
| alternating phases | 3,433,144 | **3,304,462** | -3.7% | 70 |
| mixed pressure | **2,789,016** | 18,859,865 | +576% | 1,389 |

**Verdict: reject decisively.** Local waste feedback confuses transient memory
pressure with a changed scan topology, closes too far, and then pays request
latency while reopening. Hysteresis does not prevent 1,389 direction reversals.
A future prefetch controller needs separate pressure and usefulness states;
this transferred stomatal rule is not retained.

### e59 — Endocrine spill coordination

Four concurrent operators shared a hard 1,000-unit cap. Missing ideal memory
was priced by an operator-specific marginal spill curve. The candidate grants a
64-unit correctness floor, broadcasts global pressure, allocates remaining
tokens in marginal-utility order, and enforces the cap with a fast reflex.

| Trace | Best non-candidate spill / p99 | Hormone+reflex spill / p99 | Spill delta |
|---|---:|---:|---:|
| balanced | 0 / 100 | 0 / 100 | tie |
| heterogeneous | 27,080,976 / 289 | **7,480,000 / 162** | **-72.4%** |
| synchronized bursts | 18,355,777 / 476 | **10,140,000 / 344** | **-44.8%** |
| utility reversal | 19,184,286 / 417 | **9,593,654 / 218** | **-50.0%** |

Linux reproduced every modeled metric and exact checksum. No policy exceeded
the cap; the candidate created no synchronized spill storm and left no memory
unused under pressure.

**Verdict: simulation gate passes; utility measurement remains unvalidated.**
The win is large and stable, but this model gives the allocator correct marginal
utility. The next experiment must measure real sort/aggregate/join spill curves,
including the cost and noise of online probing. Do not replace Pintail's current
budget allocator from this simulation alone.

### e63 — Glycogen completion reserve

The candidate keeps 140/1,000 memory units outside normal admission. It reserves
extra burst bytes only for protected CDC work, completes any fitting task that
releases the most base memory, and rolls that released memory into the next
completion. A global reserve without ordered release is the strong baseline.

| Trace | Global reserve spill / cascades / makespan | Completion reserve | Makespan delta |
|---|---:|---:|---:|
| small bursts | 0 / 0 / 92,965 | 0 / 0 / 92,965 | tie |
| synchronized | 8,073 / 593 / 109,111 | **14 / 2 / 92,993** | **-14.8%** |
| protected CDC | 25,243 / 174 / 143,451 | **0 / 0 / 98,002** | **-31.7%** |
| mixed | 14,803 / 174 / 122,571 | **0 / 0 / 96,508** | **-21.3%** |

Linux reproduced the same metrics and exact task set. Against full utilization,
the candidate removes over 99.9% of synchronized spill and all protected spill;
against per-task padding it admits batches of eight rather than four or five.

**Verdict: simulation gate passes; scheduler integration remains unvalidated.**
The mechanism adds value beyond merely withholding memory, passes the easy
control, and protects CDC explicitly. Advance to a real budget/spill harness
with concurrent aggregate, join, sort, and oversized CDC transaction paths.

## 2026-08-12 biomimetic program — Wave 3

Wave 3 executed e31, e35, e37, e40, e45, e57, e68, and e79. All policies
consumed identical deterministic traces and produced the same logical checksum.
Only e35 and e57 survived screening; Linux reproduced their modeled metrics.

### e31 — Ant-colony join-path learning

| Trace | Static / oracle work | Discounted UCB | Pheromone | Pheromone recovery |
|---|---:|---:|---:|---:|
| stable | 3.60M / 3.60M | 3.62M | 3.91M | n/a |
| two shifts | 7.32M / 3.60M | **3.61M** | 4.08M | 47 queries |
| noisy costs | 7.30M / 3.60M | **3.61M** | 4.00M | 28 queries |

**Verdict: reject.** Pheromone evaporation is 13.3% above oracle after two
shifts and incurs 3-4.05x worst-query regret, missing the 10%, 2x, and recovery
gates. Discounted UCB adapts in three queries and is both simpler and stronger.

### e35 — Clonal kernel repertoire

| Trace | Global / oracle | Epsilon / oracle | Clonal / oracle | Least-tested clone |
|---|---:|---:|---:|---:|
| stable | 156.08% | 102.85% | **101.70%** | 9 |
| architecture reversal | 156.17% | 102.95% | **101.72%** | 9 |
| rare contexts | 143.15% | 102.94% | **101.69%** | 1 |

The repertoire validates every context, including rare ones, and remains within
1.8% of the hindsight contextual oracle while cutting modeled work 29-35%
against one global kernel. Linux reproduced all work and validation counts.

**Verdict: simulation gate passes; dispatch remains unvalidated.** The model
charges exploration but not real indirect-call, feature extraction, instruction
cache, or code-size costs. Advance to safe filter and hash-probe kernel variants;
do not add a runtime repertoire from this result alone.

### e37 — Immune affinity plan memory

| Trace | Exact-text LRU | Fixed parameter buckets | Affinity cache |
|---|---:|---:|---:|
| stable | 8.79M | **5.40M** | 5.50M |
| selectivity shift | 8.46M | **5.33M** | 5.43M |
| skewed reuse | 6.09M | **5.15M** | 5.26M |

**Verdict: reject.** Affinity reuse beats exact SQL text, but loses 1.9-2.2%
to fixed parameter buckets in every shape. Similarity matching adds no value
over the simpler semantic partition.

### e40 — Hippocampal weak-trace replay

| Trace | Frequency work / p95 | Weak-trace work / p95 | Work / p95 delta |
|---|---:|---:|---:|
| stable | 11.19M / 299 | 9.00M / 262 | -19.6% / -12.4% |
| periodic rare | 11.21M / 336 | 9.03M / 275 | -19.4% / -18.2% |
| value drift | 12.15M / 338 | 10.04M / 314 | -17.4% / -7.1% |

**Verdict: reject.** Weak-trace replay is directionally useful, but the gate
requires at least 20% lower cumulative work *and* p95. It clears neither metric
on any trace. Worst-error replay often does less total work, further weakening
the biological prioritization claim.

### e45 — Cardinality homeostasis

| Trace | Static q50 / q95 | EWMA q50 / q95 | Homeostatic q50 / q95 |
|---|---:|---:|---:|
| stable | 5.80 / 8.80 | **1.00 / 1.00** | 4.14 / 16.55 |
| reversal | 5.80 / 8.80 | **1.00 / 1.00** | 4.27 / 14.60 |
| noisy | 5.27 / 9.07 | **1.08 / 1.17** | 4.28 / 15.12 |

**Verdict: reject decisively.** The bounded feedback controller prevents the
unbounded learner's numerical explosion, but is worse than static estimates and
a simple EWMA in q-error and execution work. Stability is not accuracy.

### e57 — ATP-priced physical plans

| Regime | Row-count correct / violations | CPU-only | ATP currency |
|---|---:|---:|---:|
| warm cache | 60.88% / 4,117 | 87.68% / 0 | **100% / 0** |
| cold I/O | 68.32% / 4,117 | 44.09% / 0 | **100% / 0** |
| tight memory | 48.47% / 14,433 | 82.56% / 0 | **100% / 0** |

Every generated query has a conservative feasible plan. The candidate enforces
that budget and prices CPU, I/O, retained memory, and allocations in the same
units as the simulated workload. Median modeled resource error is zero; Linux
reproduced the correctness, work, and violation counts.

**Verdict: conditional simulation pass; calibration remains unvalidated.** The
result proves the policy given accurate resource facts, not that Pintail can
measure those facts under 1% overhead or predict them within 15%. Advance to
instrumented equivalent physical operators before considering optimizer use.

### e68 — Biodiversity execution reserve

| Trace | Periodic regret / detection | Diversity regret / detection |
|---|---:|---:|
| stable | 28,000 / n/a | 56,980 / n/a |
| one reversal | **42,450 / 172** | 70,665 / **21** |
| volatile | 248,570 / 2,900 | **78,455 / 190** |

**Verdict: reject.** The reserve detects reversals quickly and its stable tax is
only 0.71%, but continued exploration makes single-reversal regret 66% worse
than periodic probing. It therefore misses the required 30% regret reduction.

### e79 — Echolocation plan probes

| Trace | Static work | Triggered probes | Oracle | Improvement |
|---|---:|---:|---:|---:|
| correlated uncertainty | 479.03M | **371.43M** | 349.41M | 22.5% |
| accurate estimates | 349.65M | **349.65M** | 349.59M | no probes |
| small queries | 54.69M | **54.69M** | 54.68M | no probes |
| mixed | 414.47M | **361.10M** | 350.07M | 12.9% |

**Verdict: reject at the preregistered margin.** Uncertainty-triggered probes
avoid control regressions and remove most wrong choices, but the uncertain
workloads improve only 13-22%, below the 25% gate. The mechanism is a useful
near miss, not validated optimizer policy.

## 2026-08-12 biomimetic program — Wave 4

Wave 4 executed ten cache-and-sharing experiments. Every policy consumed the
same deterministic logical request stream and checksum. e48, e51, and e73
survived simulation and reproduced on pinned Linux; seven mechanisms failed.

### e41 — Synaptic plan-cache pruning

Graph reinforcement saves 4.076-4.078M work units, slightly less than
GreedyDual-size's 4.082-4.085M, while performing roughly 28% more maintenance
operations. Both obey the 120-byte model cap.

**Verdict: reject.** It misses the 20% value-density gate and loses to the
simpler size-aware policy before real graph bookkeeping is charged.

### e48 — Flocking read coalescence

| Trace | Independent calls / median | Bounded flocking | Call delta / median delta |
|---|---:|---:|---:|
| identical | 16,384 / 5,120 | **512 / 3,608** | -96.9% / -29.5% |
| overlapping | 10,240 / 3,200 | **568 / 2,264** | -94.5% / -29.3% |
| diverging | 8,192 / 2,560 | 8,192 / 2,563 | tie / +0.1% |
| short | 256 / 80 | 256 / 83 | tie / +3.8% |

**Verdict: simulation gate passes; file-system claim remains unvalidated.** It
clears sharing, latency, and hostile-trace gates on both targets, unlike global
cooperative scanning, whose diverging p95 rises 31.8x. Advance to concurrent
cold-file PTSEG reads with actual prefetch and cancellation.

### e51 — Mycelial decoded-block exchange

At a 96-unit cap, source/sink retention reduces decode work from LRU's 4.49M to
3.26-3.27M (27%) and p95 from 319 to 264 (17%). Phase recovery occurs on the
first post-shift hit. Linux reproduces all modeled counts.

**Verdict: simulation gate passes; real decode CPU remains unvalidated.** The
model has immutable blocks only and never shares WAL/mutable state. Advance to
real projected PTSEG blocks and charge publication, lookup, and transfer.

### e65 — Predator-prey cache control

The candidate ties all policies on tight loops, beats the ARC-like control on a
one-pass scan, improves burst saved value only 1.6%, and loses 3.9% to LRU after
a phase change. Population amplitude reaches 13-15 entries in changing traces.

**Verdict: reject.** It neither beats the strongest control by 15% consistently
nor keeps oscillation below 10% of the 30-entry capacity.

### e66 — Ecological cache niches

Adaptive niches differ from global GreedyDual by +0.2% on mixed work, -8.3% on
ETL, and +5.5% on ad-hoc work, with 3-4x higher measured loop time.

**Verdict: reject.** Dynamic class borders do not clear the 20% saved-work gate;
global value competition already expresses the useful marginal signal.

### e70 — Forest-gap memory auctions

Against equal redistribution, auctions lower spill 32%, 24%, and 52% across the
three traces, but makespan falls only 11%, 4%, and 7%. Maximum waiting age reaches
593 epochs and the bid loop is slower than the 0.5% budget permits.

**Verdict: reject.** Exact marginal bids optimize spill but fail the makespan,
overhead, and bounded-starvation conjunction.

### e71 — Dormant auxiliary indexes

Multi-cue seeds halve seasonal memory-area versus retain-hot and avoid false
germination, but seasonal p95 remains 310 versus 12 for an awake index. It does
not awaken early enough to meet the query SLO.

**Verdict: reject.** The three-cue conjunction is too conservative; cheap
dormancy is useful, but the transferred germination rule is not.

### e72 — Seasonal decoded-column migration

The seasonal detector performs no migrations on periodic or drifting traces,
costing 1.41-1.50M versus recency's 0.27-0.28M. Random access remains safe, but
that guardrail cannot compensate for failure on the target workload.

**Verdict: reject decisively.** Repetition inside each season looks like short
recency, not a periodic arrival, so the chosen seasonal statistic is wrong.

### e73 — Host–microbiome intermediate exchange

Result and intermediate entries share one 12-entry cap (six each for donation
policies). Symbiotic admission raises saved work from result caching's 5.55M to
12.56M on related dashboards, 1.41M to 3.55M on one-offs, and 4.04M to 9.51M
across CDC versions. Producer work on the adversarial trace is 0.58% of total.
Version keys prevent stale reuse; 78 old intermediates are invalidated, not read.
Linux reproduces every work and lineage count.

**Verdict: simulation gate passes; real intermediate costs remain unvalidated.**
Advance to immutable aggregate/filter intermediates with PTSEG snapshot lineage
and measured materialization cost. Do not expose cross-query sharing yet.

### e74 — Fire-ecology cache reset

LRU, TTL, flush detection, and partial-reset detection have identical miss cost
and 669-request recovery on abrupt and gradual shifts. Neither reset fires: LRU
turns the 32-entry working set over before low productivity persists long enough.

**Verdict: reject.** The trigger correctly avoids false alarms but cannot recover
30% faster than a capacity-sized LRU churn that has already completed.

## 2026-08-12 biomimetic program — Wave 5

Wave 5 executed ten storage and integrity screens. e30, e39, e53, and e76
advance conditionally and reproduce on Linux; six ideas are rejected.

- **e30 Physarum bundles — conditional pass.** Flow+decay cuts modeled bytes
  from 6.00M to 3.31M, keeps storage at +7%, charges 9,440 rewrite units, and
  recovers in six queries. Frequency bundling ties it on stable/shifted traces,
  so the real trial must justify adaptation on cold PTSEG files.
- **e36 negative selection — reject.** All three detectors achieve 99% recall
  and zero false quarantine because the exact invariant veto dominates. The
  ensemble adds no value over fixed thresholds; ingest overhead is not earned.
- **e39 quarantine membranes — simulation pass.** All 1,323 overlapping queries
  fail loudly, unaffected availability is 100%, and verification falls from
  5.12M to 149,672 units (97.1%). Real persisted corruption semantics remain.
- **e42 predictive coding — reject.** Linear and product predictors do not shrink;
  seasonal residuals save only 11.8%; reconstruction is exact but slower; random
  data correctly falls back without expansion.
- **e53 coral partials — conditional pass.** Charged work is 4.09M versus 200M
  for clean scans and 20.08M versus 200M at 10% dirty overlap, with 4% modeled
  storage. The simulator does not validate actual snapshot lineage or aggregates.
- **e54 tombstone trails — reject.** Evaporation helps neither hotspot conjunction:
  moving/scattered reads rise roughly 42-44% versus size-tier and write reduction
  is only 14-15%. Append-only remains neutral.
- **e67 succession — reject.** The state machine makes 1,714-4,810 transitions on
  read/update traces and loses to heat-only tiers. The transferred lifecycle is
  unstable even before real format-transition cost.
- **e75 parity regeneration — reject.** XOR reconstructs a single 64 KiB loss
  bit-exactly, fails closed on two losses, costs 6.67% storage, and adds about 5%
  kernel time. But reading its 15-block stripe plus parity is 1.07x a modeled
  15-block restore, nowhere near the required 10x recovery-byte win.
- **e76 hierarchical reconciliation — conditional pass.** Exact sparse, clustered,
  and dense difference sets match. Sparse transfer is 4,096 rows versus 131,072
  for a full scan (32x); it equals flat 128-row chunks at every density. An initial
  XOR-like digest silently cancelled periodic differences and was rejected by the
  exact assertion; the replacement is order-sensitive, but a production digest
  and persisted-tree overhead still need validation.
- **e78 receptor Bloom ensemble — reject.** At equal bits and build probes, one PK
  Bloom produces 8,972 false reads, the ensemble 9,221, and partitioned filters
  29,564. All have zero false negatives; specialization does not beat one filter.

## 2026-08-12 biomimetic program — Wave 6

The final eleven necessary-condition models ran with deterministic exact-result
checksums. e44, e52, and e61 advance to executable kernel/integration trials;
the other eight fail at least one preregistered conjunction.

| ID | Candidate result | Verdict |
|---|---|---|
| e44 | 39-61% fewer payload bytes on wide shapes; +3.5% uncorrelated | conditional pass |
| e47 | clustered -27%, moving cluster only -11%, uniform +2.4% | reject |
| e49 | p95 -23/-23/-17%; throughput 97%, fairness 0.90+ | reject |
| e52 | join input -31/-26/-82%; no-FK neutral; metadata 1% | conditional pass |
| e56 | spill -28/-17/-25%; reversal recovery 44 epochs | reject |
| e58 | strings +8.4% runtime; aggregate +12% memory | reject |
| e60 | churn allocations -14.5%, resident slack 14% | reject |
| e61 | 92.4% doomed work stopped at 24%; false abort 0.007%; p99 -31% | conditional pass |
| e62 | within 5% fixed modes, but shifts -16.2% and transitions 6% | reject |
| e64 | periodic maintenance +36%, drifting +21%, p99 -12% | reject |
| e69 | non-invasive p99 -36/-38%; flash misclassification 4% | reject |

These are model-tier results, not engine benchmarks. The three survivors prove
only that the mechanism can clear its numeric gates under the declared cost and
classification assumptions; those assumptions must now be measured in Pintail.
