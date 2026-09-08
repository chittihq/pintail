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

> **Audit completed — 2026-08-15.** All 50 packages now pass local tests and Clippy
> and have been executed as release binaries. Execution alone did not rehabilitate a
> claim: source-level decision, budget, checksum, and benchmark review leaves only e39,
> e52, and e61 as prototype candidates. Ten additional former positive claims are invalid,
> and e43 was reversed after making its full policy outcome observable. The controlling
> per-experiment record and future evidence contract are in [`AUDIT.md`](AUDIT.md).

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

### 2026-08-15 re-audit — e62 operator fission/fusion

Global and eight-shard aggregators now execute and merge exact 1,024-group state; an
order-sensitive checksum covers every phase and group. Reversible sampling selects the
better mode on uniform/skew phases and transition work is 3.84% on the shifting trace.
Its shifting cost is 92,372 versus 96,000 for the best fixed mode, only 3.8% better, and
its 256-row sampling charge makes tiny work 25% worse than global.

**Re-audited verdict: reject.** It misses both the 20% cumulative-shift target and the
within-5% tiny control, despite exact merging and bounded transition work.

### 2026-08-15 re-audit — e64 circadian maintenance

The replacement executes 4,320 load/debt ticks, learns only a bounded 144-byte prior-cycle
history, caps work at current slack, and preserves the foreground checksum. Against a fair
debt-reactive controller that also consumes safe slack, forecast+reflex completes identical
work on periodic/missing traces and 3.8% less on drift and random traces. Its p99 is 1-4
units worse; forecast-only is substantially worse and breaches the modeled SLO.

**Re-audited verdict: reject.** The earlier advantage came from comparing prediction with
an artificially timid reactive threshold. Once both may use available slack, seasonal
history adds no maintenance or p99 value.

### 2026-08-15 re-audit — e69 invasive-template defense

The replacement runs every arrival through real invasive/diverse queues, drains every job,
and compares count/sum/xor result checksums. Harm must persist for 50 ticks before the
template cap falls to 20%; minimum contended share, completed work during arrivals, p99
from individual diverse-job latencies, and flash classification are all measured.

Harm feedback improves non-invasive p99 from 239 to 68 on the cheap flood and 120 to 24
on polluting scans, retains identical useful work, and preserves a 10-20% minimum share.
It never classifies the 40-tick flash as invasive—but flash p99 rises from FIFO's 16 to 38.

**Re-audited verdict: reject.** The sustained attacks clear the 30% target and flash
misclassification is zero, but the legitimate-flash latency guard fails by 137.5%.
Static quotas get p99 1 on attacks but misclassify every flash tick, confirming the real
tradeoff rather than validating harm feedback.

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
`[1, 4, 13, 3]`. The corrected candidate samples eight rows at equal strides
through each 4,096-row block (0.20% of rows), then greedily chooses marginal
newly-rejected rows per cost. “Hindsight”
enumerates all 24 static orders and does not charge enumeration to the reported
work, making it a deliberately strong baseline.

| Shape | Best static work | Per-block marginal work | Delta | Static / adaptive local median |
|---|---:|---:|---:|---:|
| independent | **4,704,825** | 5,232,864 | +11.2% | **33.9 / 68.7 ms** |
| correlated rejects | **6,925,765** | 7,369,758 | +6.4% | **38.5 / 148.2 ms** |
| anti-correlated rejects | **6,695,676** | 7,434,961 | +11.0% | **38.4 / 105.4 ms** |
| blockwise regime reversals | 4,972,925 | **3,647,939** | **-26.6%** | **31.1 / 58.8 ms** |
| misleading block prefixes | **1,160,434** | 1,186,809 | +2.3% | **22.7 / 57.8 ms** |

The original elapsed loop returned only a policy-independent result checksum,
allowing the optimizer to discard policy bookkeeping. The corrected benchmark
black-boxes the complete outcome. It also replaces prefix sampling with stratified
sampling and adds exact-answer, misleading-prefix, and drift regressions.

**Re-audited verdict: reject.** The candidate still reduces calibrated work 26.6%
under blockwise drift and resists a misleading prefix, but exceeds the 5% stable
guardrail in three controls and is 1.9-3.9x slower than the learned-static loop.
The earlier elapsed win was dead-code-elimination contamination.

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

**Re-audited verdict: reject.** The bounded candidate loses decisively to the
simpler heat-only policy on both hotspot traces and raises hotspot p95 above the
fixed layout. Beating fixed decoded rows while losing to the strongest control
is not a pass.

## 2026-08-12 biomimetic program — Wave 2

Wave 2 executed e38, e46, e50, e55, e59, and e63. Its original positive claims
for e59 and e63 were invalidated by the 2026-08-15 source audit; all six ideas
are rejected or unsupported.

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

**Re-audited verdict: invalid positive.** The candidate receives the current
ground-truth utility vector, while only the independent baseline pays a synthetic
storm penalty. The comparison cannot establish allocation value.

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

**Re-audited verdict: invalid positive.** Every protected task may consume the
same 140-unit reserve and the completion path defines its available memory as at
least its own burst. Zero protected spill is therefore true by construction.

## 2026-08-12 biomimetic program — Wave 3

Wave 3 executed e31, e35, e37, e40, e45, e57, e68, and e79. The 2026-08-15
source audit invalidated the former e35 and e57 positives; no Wave 3 idea survives.

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

**Re-audited verdict: invalid positive.** The learner opens a fresh model bank at
the exact known half-run phase boundary. Its reversal result therefore measures
oracle segmentation, not adaptation.

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

**Re-audited verdict: invalid positive.** `Policy::Atp` predicts with the exact
same `true_cost` formula used to select the oracle. The reported 100% is an
identity, not calibration evidence.

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

Wave 4 executed ten cache-and-sharing experiments. The 2026-08-15 source audit
invalidated the former e48, e51, and e73 positives; no Wave 4 idea survives.

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

**Re-audited verdict: invalid positive.** The harness performs no reads: “calls,”
bytes, and latency are arithmetic over intervals. The candidate is interval
unioning with a favorable hand-written latency equation.

### e51 — Mycelial decoded-block exchange

At a 96-unit cap, source/sink retention reduces decode work from LRU's 4.49M to
3.26-3.27M (27%) and p95 from 319 to 264 (17%). Phase recovery occurs on the
first post-shift hit. Linux reproduces all modeled counts.

**Re-audited verdict: invalid positive.** The claimed shifted trace adds three
modulo six to a uniform dashboard ID, preserving the same distribution. “First
post-shift hit” is not a recovery measurement.

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

**Re-audited verdict: invalid positive.** Symbiotic admission is the hard-coded
predicate `family < 16`; the recorded net-reuse field does not decide admission.

### e74 — Fire-ecology cache reset

LRU, TTL, flush detection, and partial-reset detection have identical miss cost
and 669-request recovery on abrupt and gradual shifts. Neither reset fires: LRU
turns the 32-entry working set over before low productivity persists long enough.

**Verdict: reject.** The trigger correctly avoids false alarms but cannot recover
30% faster than a capacity-sized LRU churn that has already completed.

## 2026-08-12 biomimetic program — Wave 5

Wave 5 executed ten storage and integrity screens. The 2026-08-15 audit
invalidated e30 and e53, rebuilt e39 as a passing byte-path prototype, and
rebuilt e76 as a reject. Only e39 remains a candidate.

- **e30 Physarum bundles — invalid positive.** The frequency baseline is a
  fixed `first_column < 4` rule, the candidate has unequal effective bundle
  capacity, and recovery also fires on traces without a phase change.
- **e36 negative selection — reject.** All three detectors achieve 99% recall
  and zero false quarantine because the exact invariant veto dominates. The
  ensemble adds no value over fixed thresholds; ingest overhead is not earned.
- **e39 quarantine membranes — simulation pass.** All 1,323 overlapping queries
  fail loudly, unaffected availability is 100%, and verification falls from
  5.12M to 149,672 units (97.1%). Real persisted corruption semantics remain.
- **e42 predictive coding — reject.** Linear and product predictors do not shrink;
  seasonal residuals save only 11.8%; reconstruction is exact but slower; random
  data correctly falls back without expansion.
- **e53 coral partials — invalid positive.** Work, build, and storage are
  hand-authored formulas; the harness executes no aggregate, partial, snapshot
  lineage, or invalidation.
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
- **e76 hierarchical reconciliation — re-audited reject.** Persisted exact indexes
  retain the sparse-transfer advantage but are about 9.9% slower than flat
  checksums at 33% drift, failing the dense-control gate.
- **e78 receptor Bloom ensemble — reject.** At equal bits and build probes, one PK
  Bloom produces 8,972 false reads, the ensemble 9,221, and partitioned filters
  29,564. All have zero false negatives; specialization does not beat one filter.

## 2026-08-12 biomimetic program — Wave 6 (void)

The original Wave 6 table was generated by one shared `include!` that printed
hand-authored literals and an experiment-number checksum. It executed no claimed
work and is removed rather than preserved as evidence. All eleven crates have
since been replaced and re-executed; their controlling results are the dated
2026-08-15 re-audit sections above and [`AUDIT.md`](AUDIT.md). Only e52 and e61
remain prototype candidates; the other nine are rejects.

## e61 — Where compressed execution actually breaks even (20M rows, 12 groups, ~1/5 kept)

Contested question: FastLanes reports up to 7x end-to-end on a RAM-resident SUM
scan and a break-even compression ratio of only 25%, while our own e23/e24/e28
measured bit-unpacking at ~1.4% of a scan and concluded encoding wins do not
transfer. Those measure different things — unpacking COST versus bytes moved —
so this separates them on the Q5 shape. Remote host (venus-003, i7-10700F,
8C/16T, 16MB L3). Minimums, checksums identical across non-permuted arms.

| min ms | 1 thread | 8 threads | 16 threads |
|---|---:|---:|---:|
| plain i64 (baseline) | 16.2 | 7.3 | 7.4 |
| **narrow u32, materialised** | **12.5** | **4.3** | **4.3** |
| packed 8-bit -> materialise i64 | 34.6 | 4.4 | 4.3 |
| packed 8-bit -> fused | 32.3 | 4.2 | 4.0 |
| packed 32-bit -> fused | 33.6 | 4.7 | 4.4 |

**Verdicts.**

(1) **The thread count decides the sign of the result, exactly as FastLanes
claims.** At one thread bit-packing LOSES badly — 32-38ms against 16.2ms plain,
so 2.1-2.3x slower, because unpacking cost dominates and nothing else is scarce.
At 8 and 16 threads the same packed arms WIN at ~1.7x, because RAM bandwidth
becomes the constraint. Our earlier "encoding wins do not transfer" verdict was
measured in the regime where it is true and does not generalise to the
production thread count.

(2) **But packing is not the lever — narrowness is.** Simply storing the column
as u32 instead of i64, with no packing and no unpacking, matches or beats every
packed arm at every thread count and every width: 4.3ms at 8 and 16 threads
(1.7x over plain), and 12.5ms at one thread where it is the ONLY arm that beats
the baseline. Packing buys nothing over a narrow native type here and costs
2.5x when threads are scarce. The actionable form of "execute on compressed
data" for this engine is **use the narrowest native integer that fits**, not
bit-packing.

(3) Fusing the unpack into the aggregate loop, so the expanded column never
reaches memory, is worth a consistent but second-order 5-7% over materialising
it (4.0 vs 4.3ms at 16 threads; 32.3 vs 34.6 at one). Real, and it agrees in
direction with the engine's bytes-moved rule, but it is not the factor — which
is consistent with Kersten et al. bounding fusion at +74% on the
aggregation-bound query and -32% on the join-bound one.

(4) **The lane-parallel layout did NOT reproduce.** It was 4.8-7.1ms against the
naive layout's 4.0-4.5ms at 16 threads. This is reported as a failure to
reproduce rather than a refutation of FastLanes: the arm permutes row order, so
its group and filter columns are read through that permutation, and the strided
access is a plausible confound that the original does not have. Anyone retesting
should fix the confound before drawing a conclusion about layout.

CAVEAT ON SCOPE: this is a microbenchmark over in-memory arrays. It isolates the
aggregate loop's relationship to value width and does not include the engine's
block reads, checksums or typed adoption, so the 1.7x is an upper bound on what
the query would see — the same gap already observed between the profiling
harness and the server benchmark.

DIRECTLY ACTIONABLE: the engine stores date units as Vec<i64> (days since epoch
needs 16 bits) and, after e3f4cbc, decimal units as i64 (a DECIMAL(10,2) of
cents needs 32). Narrowing those carriers to 32-bit is the same change that
already paid when decimals went i128 to i64, applied one step further, and this
says it is worth about 1.7x on the aggregate loop at production thread counts.

## e62 — The speed of light for Q5 (20M rows, venus-003, i7-10700F 8C/16T)

Every optimisation this session was aimed at a gap measured only against
ClickHouse, which says a gap exists but not how much of it is real work. This
measures the floor: a hand-written loop doing exactly what Q5 needs, climbing
one layer at a time toward the engine.

| min ms, 16 threads | | ratio to floor |
|---|---:|---:|
| 1. floor (i32 date, group ordinal known) | 15.9 | 1.00x |
| 2. + civil date arithmetic per row | 17.8 | 1.12x |
| 3. + i64 date column | 17.8 | 1.12x |
| 4. + 65k batch granularity | **7.9** | 0.50x |
| ClickHouse | 40.0 | **5.1x** |
| Pintail engine | 159.0 | **20.1x** |

(Arm 4 beats arms 1-3 because iterating batches amortises what rayon spends
splitting a 20M-element range per row; it is the realistic floor, not an
anomaly. Checksums identical across all four.)

**Verdicts.**

(1) **The gap to ClickHouse is machinery, not work.** The query's genuine work -
filter, per-row date arithmetic, sum and count into twelve slots, at the
executor's own batch granularity - is 7.9ms. ClickHouse carries 5.1x of
general-engine overhead over that floor, which is what a real engine costs.
Pintail carries 20.1x. We are not fighting memory bandwidth or arithmetic: we
carry 4x more overhead than ClickHouse, and 148ms of our 159ms is machinery
(decode 59, slicing 26, ingest 29, drain 20, adopt 14).

(2) **Narrowing carriers to 32-bit would buy nothing, and this kills that plan
before it was written.** Arms 2 and 3 differ only in whether the date column is
i32 or i64 and measure identically at 17.8ms. e61's 1.72x for u32 does not
transfer to this query: e61 varied the column summed on every kept row, while
Q5's filter rejects four rows in five before the wide column is touched. A
technique's benefit is a property of the access pattern, not of the width.

(3) **Date arithmetic is 12% of the floor, not a bottleneck** - consistent with
the earlier finding that a civil-from-days lookup table returned ~1% on the real
query. Confirmed twice now by different routes.

CAVEAT: the floor reads pre-materialised in-memory arrays and does not decode
from storage, verify checksums, or handle nulls, so some of the 20.1x is
irreducible. It bounds what is reclaimable; it does not promise it. The useful
comparison is not Pintail against the floor but Pintail's 20.1x against
ClickHouse's 5.1x, since both are general engines paying general-engine costs.

DIRECTION: stop optimising the 7.9ms of work. Attack the 148ms of machinery -
per-batch setup, memory accounting, operator dispatch, the decode path's
materialisation - which is where a 2-4x lives.

## e62 follow-up — inside the 148ms of machinery: it is block decode, not I/O

Instrumented the store's integer fast path on venus-003, Q5, 20M rows, three
iterations, CPU time summed across threads.

| phase | CPU |
|---|---:|
| block file read + allocate + copy | 65 ms |
| block decode into builders | **1,897 ms** |

Decode is 29x the I/O. Per iteration that is ~632ms of CPU for 40M values, so
about **16 nanoseconds per value** - against e62's specialised loop, which does
the ENTIRE query in 126ms of CPU. Block decode alone costs five times the whole
ideal query.

This locates the machinery precisely. It is not disk, not the file format, not
the read pattern: it is the per-value work of turning a decoded block into the
engine's in-memory columns - checksum, unpack, validity construction and the
per-row pushes into builders. e24 separately measured unpacking at ~1.4% and the
checksum at 4% of a scan, which leaves the builder path as the residue, and that
matches the shape here.

DIRECTION: the highest-value remaining work is the block-to-column path, not the
aggregate, not the encoding, and not the operator model. Attack what happens per
value between a verified block and a ColumnVector.

## e62 follow-up 2 — typed flat-array aggregation slots: measured negative

Replaced the dense date kernel's per-row lane machinery (column resolution,
lane dispatch, AggregateState update per row per lane) with flat typed
arrays - counts, sum units, seen, occupancy - folded into real states once
per group at drain end. The floor loop's own shape, inside the engine.

Q5, venus-003, arms interleaved, order reversed, minimums: 118-121ms before,
125-128ms with the typed kernel, 120-122ms after also porting the
single-civil key front into it. Slower or equal in every pairing; reverted.

Mechanism, consistent with every other instruction-shaped negative this
session: the dispatch being removed overlaps memory stalls, so deleting it
frees nothing - while the replacement pays real new traffic. Twelve occupied
groups keep the existing per-slot state vectors L1-resident, whereas the
flat arrays touch four separate cache lines per row and zero-fill ~540KB per
worker per window before any row lands.

The lane dispatch is NOT where the remaining aggregate time goes. What
remains ahead of it: the window buffering between pull and drain, and the
serial batch pull itself (Codex located structural serial work there:
batches pulled and accumulated serially before folding).

Addendum: overlapping the window drain with the next batch pull via
rayon::join (disjoint state, fifteen lines) measured noise on Q5 (118 to
117ms) and Q6 (equal). Most pulls return an already-decoded batch from the
ready queue; the heavy sixteen-chunk prefetch decode rarely lands inside a
drain, so there was little serial time to reclaim. Reverted. The pull-side
serial structure Codex flagged is real but its cost concentrates in the
prefetch call one pull in sixteen, not in the drain boundary.

Addendum 2: a word-skipping SelectedRows iterator (skip zero words whole,
pop set bits via trailing_zeros) measured q5 flat and q3 REGRESSED 10%
(120-123ms to 132-137ms). On dense all-ones masks - every unfiltered scan -
the trailing_zeros/clear-lowest-bit chain carries a loop-serial dependency
through the current word, where the old test-per-row loop had none and
vectorised better. Sparse masks were the motivating case, but the dense fold
work dwarfs the bit tests there, so nothing was won where it was supposed to
win and real time was lost where masks are dense. Reverted. A hybrid (run
mode for u64::MAX words) might salvage it; measure q3 first if attempted.

Addendum 3: borrowing uncompressed payloads (Cow in decompress_block instead
of to_vec for Compression::None) measured neutral on q5 and q3 - the
benchmark dataset's integer blocks evidently store LZ4, so the None-path
copy Codex flagged is not on this workload's hot path. Reverted for now;
worth re-testing if a deployment shows None-heavy blocks, where it is a
strict bytes-moved win.

## e63 — Clustered dates win nothing today: non-key pruning does not exist

Added PINTAIL_CLUSTERED_DATES to the profile harness: dates follow insertion
order (the layout real CDC order data has) instead of the scattered
(id*7)%1825. If block-level min/max pruning worked for the date column, Q5's
one-year filter would skip ~80% of blocks on clustered data.

Measured, venus-003, 20M rows: scattered 105-109ms, clustered 105-108ms.
IDENTICAL. Perfect clustering buys zero.

Verdict: the engine has no per-block column-statistics pruning for non-key
columns. The blocks_pruned counter counts PRIMARY-KEY range pruning, and the
prewhere path decodes predicate columns before filtering - it cannot skip a
block it has not read. e09's 10-18x for pruning on clustered data is
therefore entirely unrealised product headroom, and benchmark/README's note
that "the seeded dates make pruning unmeasurable" was true but incomplete:
pruning is unmeasurable AND unimplemented off the key.

This scopes the real feature (issue #6 already names it: per-block SMAs in
segment footers): write min/max per block per native-unit column at flush,
push comparison predicates to block skipping before decode, cover with the
oracle since skipping wrongly is silent corruption. The clustered harness
variant is the measuring instrument, kept for that work.

## e64 — Removing compression makes the whole suite slower (venus-002, 20M rows)

The owner asked directly: does removing compression buy performance? Two
write-side knobs answer it end to end with real segments - not the e61
microbenchmark. A = today (LZ4 + bit-packed), B = raw blocks still packed,
C = raw fixed-width. Interleaved, order reversed, minimums, n=6 per arm.
Host is venus-002 (its own baseline; only within-host arms compare).

| min ms | A today | B no-LZ4 | C raw |
|---|---:|---:|---:|
| q3 | 127 | 127 | 194 |
| q5 | 97 | 109 | 227 |
| q6 | 364 | 368 | 494 |
| q8 | 306 | 333 | 444 |

**Verdicts.** (1) Removing LZ4 buys nothing anywhere and costs q5 12% and
q8 9% - decompress is cheap and overlapped, while raw blocks push more
bytes through the page cache, which is RAM traffic on the saturated pipe.
(2) Removing packing as well is 1.5-2.3x SLOWER on every query - q5 more
than doubles. At sixteen threads the engine is bandwidth-bound and packed
data is fewer bytes moved, exactly as e61 and the FastLanes break-even
predicted. (3) Compression is not overhead in this engine; it is load-
bearing. The decode cost lives in per-value machinery (largely removed
this session), not in the codec.

The knobs were REMOVED from the engine once this answered (rc15). They were
write-side scaffolding for a question now settled, and a released binary
should not carry a switch that makes its own storage slower. Reinstating them
is a ten-line change against compress_block_for_storage and select_encoding
if a RAM-rich deployment ever reopens the question; this table is the reason
it should not need to.

## e65 — Scan and execution pools controlled separately, with the per-operator profile (20M rows, shared docker host, 8 CPUs)

The first use of `EXPLAIN ANALYZE`'s profile on the benchmark replica:
`benchmark/profile.ts` rebuilds the image from the tree, copies a finished
run's pintail data volume, and restarts the container per thread
configuration with the settled memo off. Three runs each; the engine
column is the profile's own total, the wall column the HTTP round trip
of the same call. The host was shared with other stacks, so the spread
between runs is real and the minimum is the number to read.

| query | pools scan x exec | engine ms (profile) | wall ms (min of 3) |
|---|---|---:|---:|
| Q2 filtered count | 8x8 | 66 | 96 |
| Q2 | 16x8 | 57 | 79 |
| Q2 | 2x8 | — | 159 |
| N1 filtered count + `id >= 1` | 8x8 | 162 | 174 |
| N1 | 16x8 | 146 | 154 |
| N1 | 2x8 | — | 468 |
| Q5 monthly revenue | 8x8 | 116 | 154 |
| Q5 | 16x8 | 98 | 127 |
| Q5 | 2x8 | — | 223 |
| Q8 join + group | 8x8 | 463 | 513 |
| Q8 | 8x4 | — | 669 |
| Q8 | 8x2 | — | 1042 |
| Q8 | 8x16 | — | 523 |
| Q8 | 16x8 | 437 | 495 |

Where the time goes, at 8x8:

- **Q2**: the scan is the query. 65 ms of scan self time over 200
  batches; the aggregate takes 0.5 ms. Sixteen scan threads on eight CPUs
  take it to 57 ms, two scan threads more than double it: the scan pool
  is the lever, and eight is not yet its ceiling on this host.
- **N1**: the same scan with `id >= 1` added decodes 2,800 blocks instead
  of 1,400 and takes 160 ms, 2.5x Q2, for a predicate that excludes
  nothing. That is G2 of the hardening todo: the second predicate's cost
  is the second column's decode, not the comparison, which now stays on
  the packed kernel.
- **Q5**: 85 ms in the scan, 31 ms in the aggregate. The scan retains
  272 MiB at its peak, which is G1: every prefetched segment is adopted
  at once.
- **Q8**: 463 ms, of which the two scans are 59 ms. The rest is the
  fused join-and-aggregate loop probing 20M rows, about 20 ns a probe,
  and it scales with the execution pool up to the CPU count (8x2 1042 ms,
  8x4 669 ms, 8x8 513 ms, 8x16 no better). This is where Q8's gap to
  ClickHouse lives, and it is not a scan problem.
- **API overhead**: the wall time of the call exceeds the engine's own
  total by 25-40 ms on every query. On Q2 that is a third of the banked
  number. The benchmark times `/api/query`, so a third of the reported
  gap on the fast queries is authentication, the replica cache and JSON,
  not execution.

Verdicts: raise the scan pool above the CPU count for scan-bound queries
(it costs nothing measurable on the others); the execution pool should
stay at the CPU count; the fused probe loop and the API path are the
next two things to profile inside, in that order. Numbers are not banked;
the shared host puts a 2x spread between runs of the same
configuration.

## e66 — Aggregate rounds as row-range morsels (10M rows in-process, 10 threads, memo off)

`crates/pintail-exec/tests/morsel_bench.rs`, minimum of seven runs, two
rounds interleaved with the previous build (base = after the chunk
capacity fix, new = morsel rounds at two morsels per thread). Milliseconds;
"fail" is a query memory limit error under the 512 MiB default ceiling.

| query | base 1 | new 1 | base 2 | new 2 |
|---|---:|---:|---:|---:|
| two-pass int key (50 groups) | 145 | 136 | 132 | 137 |
| two-pass text key (5 groups) | 99 | 101 | 89 | 105 |
| general int+text keys (50 groups) | 496 | 525 | 493 | 517 |
| general expression key (10 groups) | 412 | 262 | 404 | 277 |
| general 200K groups | 7,540 | 5,476 | 7,684 | 5,684 |
| fused join + group (8 groups) | fail | 85 | fail | 84 |
| 150K rows: two-pass int key | 3.7 | 3.0 | 4.3 | 3.0 |
| 150K rows: general int+text keys | 11.4 | 8.2 | 11.4 | 8.1 |
| 150K rows: general expression key | 9.7 | 4.2 | 11.1 | 4.9 |
| 150K rows: general 150K groups | 136 | 99 | 146 | 96 |
| 150K rows: fused join + group | 3.4 | 1.9 | 3.3 | 2.1 |

The int+text row above was measured at four morsels per thread; at two it
read 422 ms in a later single run, which is the setting kept. The general
int+text query at a 64 MiB ceiling failed on the base, took 31.6 s with
waves sized to the spill pressure line (one spill per wave), and 0.8 s
with waves sized to half the ceiling (no spill).

**Verdict: keep.** Width no longer depends on how many batches a round
holds; the wins are where rounds were short, and the ten-million-row
two-pass paths are unchanged within the host's noise.

## e67 — Historical memory and concurrent-query screening (2M rows, memo off)

**Verdict: keep the findings and compact evidence; discard the experiment
code.** These measurements were taken on `d267f05`, before the chunk-capacity
fix (`73dc0fa`) and morsel aggregation (`abb79e2`). The fused-workspace finding
is already fixed unconditionally on `dev`; the experimental alternative adds
nothing to that implementation. No engine switches, Python harness, separate
validation image, or compressed raw request stream land with this entry.

**Every RSS comparison below is historical.** The measured tree still had the
sliced-prefix capacity retention defect fixed by `73dc0fa`. In particular, the
4.88 GB prefix-consumption result cannot establish a cost of prefix consumption
on current `dev`. Re-measure on the corrected engine before using any of these
memory, throughput, or latency observations to choose a policy. This entry makes
no recommendation to enable or reject a policy on today's code.

This section has its own protocol, distinct from the file header and the banked
benchmark: a separate Linux host, two million synthetic fact rows over 20
segments, 100 dimension rows, six physical engine cores, eight scan threads,
and eight execution threads. The driver used other physical cores. These
numbers are not comparable with the published benchmark host's results and
make no ClickHouse claim.

The mixed HTTP workload alternated an exact numeric primary-key lookup, a
filtered low-cardinality aggregate, a 100,000-group aggregate with top-100
output, and a dimension join with SUM/COUNT. MySQL 8.4.11 established ordered
value-level reference answers. Settled-result memo was disabled. Each trial
used a fresh process and copy of the same replica, with warmup and a three-second
allocator purge allowance before each cell. Data and spill were disk-backed.

Normal limits were an 8 GiB container, 2 GiB per query, and 6 GiB shared query
budget; tight limits were 2 GiB, 256 MiB, and 1 GiB respectively, with swap
disabled. The primary comparison used five alternating baseline/candidate
trials at each concurrency, 128 requests per cell. Predeclared promotion
required 15% more useful throughput or 20% lower peak memory, no more than 5%
regression in the other primary metric or short p99, and no new wrong answers
or increased failure rate. No candidate met every criterion.

### Already-fixed finding: per-probe-row fused workspace

The old fused join aggregate requested 620,800,000 bytes of local workspace
despite having only 100 build-side groups. The experimental candidate estimated
workspace from the known groups with allocation headroom. On current `dev`,
`abb79e2` instead reserves per-morsel, per-plan-group storage based on what the
builder allocates, with a per-row allowance for growing states. The old
env-gated estimate and its regression test are not part of this change.

Below are historical medians of five cells; failures are totals out of 640
requests per row and variant. The candidate changes only that workspace
estimate. RSS is in MiB and short-query p99 in milliseconds.

| Memory | Clients | Baseline useful QPS | Candidate useful QPS | Baseline / candidate failures | Baseline / candidate RSS | Baseline / candidate short p99 |
|---|---:|---:|---:|---:|---:|---:|
| Normal | 1 | 20.54 | 20.86 | 0 / 0 | 365 / 371 | 6.7 / 6.8 |
| Normal | 4 | 30.24 | 30.66 | 0 / 0 | 741 / 698 | 22.5 / 24.4 |
| Normal | 8 | 31.81 | 32.14 | 0 / 0 | 1248 / 1300 | 33.2 / 29.5 |
| Normal | 16 | 32.08 | 32.41 | 0 / 0 | 2341 / 2319 | 50.0 / 59.8 |
| Normal | 32 | 32.07 | 32.41 | 160 / 0 | 3606 / 3705 | 207.2 / 349.2 |
| Tight | 1 | 16.81 | 19.15 | 160 / 0 | 227 / 222 | 6.2 / 6.7 |
| Tight | 4 | 25.21 | 28.79 | 160 / 0 | 455 / 447 | 18.6 / 20.7 |
| Tight | 8 | 26.58 | 30.48 | 160 / 0 | 633 / 616 | 29.5 / 30.6 |
| Tight | 16 | 27.06 | 30.98 | 160 / 0 | 963 / 964 | 52.8 / 59.4 |
| Tight | 32 | 26.97 | 30.92 | 160 / 0 | 1489 / 1520 | 292.0 / 232.7 |

Across tight-memory confirmation cells, the baseline failed 800/3,200 requests
and the candidate failed none. At 32 clients useful throughput rose 14.7%, but
there was no consistent RSS reduction and normal-memory short p99 worsened.
Latency percentiles describe successful responses: early failures reduce the
baseline's competing work. These were resource-admission findings, not proof
of a general 15% executor speedup.

Single-repetition screening also found a substantial solo-throughput loss with
a 32 MiB scan chunk cap (20.41 to 15.04 QPS). Prefix consumption completed the
normal 32-client cell but reported 4.88 GB RSS on the defective capacity path.
Sharing scan width still produced allocation failures. A 512 MiB admission
estimate lowered recorded RSS but raised short p99 to 1.38 seconds; a fixed
eight-general-slot control reached about 1.56 seconds with seven failures.
These observations explain why no experimental code was selected, but are
not confirmed policy verdicts for the corrected engine.

Scheduled arrivals used five cells per variant at 32 clients under tight limits.
At 25 offered requests/s, baseline/candidate useful QPS was 18.60/24.70, with
160/0 failures per 640 requests. At 50 offered requests/s, useful QPS was
26.68/30.79, short p99 was 245/874 ms, and all-query p99 was 2,649/3,451 ms.
Driver dispatch delay is included, reaching 879 ms for the candidate. Completing
more useful work did not solve overload queueing. Unlike the primary comparison,
these five cells ran sequentially within each variant rather than alternating.

### Actionable follow-up: metadata writes defeat reserved admission

User-query auditing and API-key last-used writes modify the shared metadata
database/WAL stamp used by `ReplicaEngine::replica_stamp`. Reserved admission
requires an exact match in `short_query_replica`, so unrelated metadata activity
can send an otherwise eligible cached lookup back to general capacity. Both
HTTP principal types showed poor reserved-slot behavior in screening. Per-request
admission-class/cache-hit counters were not banked, so latency alone does not
prove the rejection reason for each request.

Track the independent fix in [issue #34](https://github.com/chittihq/pintail/issues/34).
The reproduction must satisfy current short-query limits, including the tiny
replica ceiling; it must not depend on the discarded large-replica classifier.
Separate replica-affecting metadata from audit/auth bookkeeping while preserving
schema/data invalidation, pinned snapshot correctness, auditing, and request
authorization. Broadening admission eligibility is separate work.

### Deferred observation: repeated decoding

Eight identical concurrent scans each reported decoding the same 336 blocks,
or 2,688 decodes. Sequential repetitions also repeated their decode counts.
That establishes repeated work, not its overlapping allocation lifetime or an
achievable cache speedup. An untimed profile put scan work at 7.4 ms of 48.2 ms,
with most time in aggregation. Sharing needs immutable backing with appropriate
schema/segment identity, bounded retention, and accounting that charges shared
allocations once. Reset, replacement, compaction, and cancellation need coverage.
No cache should be bolted onto the owned, destructively split `DecodedColumn`
representation on the strength of these counters alone.

### Evidence and scope

Keep [cells.csv](e67-memory-concurrency/cells.csv) and
[summary.json](e67-memory-concurrency/summary.json) unchanged from the historical
bank at `a6d2e13`. They cover 200 cells and 25,600 requests: 23,201 successful
responses, 2,399 failures counted as findings, and zero incorrect successful
responses. The summary retains the raw stream's hash for provenance; that
stream is not included in this writeup. Without it, individual request latency
distributions cannot be recomputed from the compact evidence.

Labels distinguish single-run screening (`screen-`, replacement tight runs
`screen2-`), API-key screening (`key-`), five alternating confirmation trials
(`confirm-`), scheduled arrivals (`arrival-`), and forced spilling (`spill`).
`groups` means the historical workspace estimate, `cap32` a 32 MiB scan cap,
`pull32` prefix consumption with that cap, `fair` active-scan width sharing,
`demand512` estimated 512 MiB admission demand, and `fixed8` eight general slots
plus two reserved slots. `reserve8` additionally enables the historical narrow
lookup classifier; combined labels combine those controls. `fair32` combines
width sharing and the cap. Unspecified controls are baseline behavior.

The old branch's development profile passed 894 selected tests, with 25 skipped,
at `52eb88b8`. Separate spill checks matched all 256 answers, wrote 6.4 GB per
cell, and released active spill storage. A 300-row CDC update/delete/insert
sequence converged to MySQL and survived restart. Those checks validate the
historical experiment only; they are neither a validation of current `dev` nor
a release gate. No new engine validation or performance remeasurement is
claimed for this documentation-only entry.

## e68 — Snapshot workers as tasks, composite-key seeks as prefixes (1M synthetic rows, local source)

From the `experiment/snapshot-throughput` branch (PR #32), three-run
medians, row counts, key sums and payload checksums verified against MySQL
on every run. Full methodology, limits and evidence in
[`snapshot-throughput/`](snapshot-throughput/README.md).

| Case | Before | After | Speedup |
|---|---:|---:|---:|
| Four tables, four workers | 6.445 s | 2.220 s | 2.90× |
| One table as four disjoint ranges (prototype, not shipped) | 6.778 s | 2.392 s | 2.83× |
| Composite key, 10,000-row pages | 23.575 s | 6.766 s | 3.48× |

The gains are independent and must not be multiplied. The composite figure
is a paging-count demonstration at 10,000-row pages, not a claim about the
default chunk size; a late page examined 910,000 rows with the row-tuple
predicate and 10,000 with the expanded one.

**Verdict: keep both, without the switches.** The worker parallelism shipped
as spawned tasks under a `JoinSet` with `block_in_place` around each chunk's
conversion and write rather than the branch's thread-per-worker with a
private runtime, which gave up cancellation; the expanded seek shipped as the
only predicate form. The range prototype is recorded and not shipped: it needs
a range planner, one-table segment assembly and a resumable range journal.

## e69 — Backup and restore transfers streamed with bounded concurrency (10 GiB synthetic, loopback MinIO)

From PR #35. Segments above 8 MiB upload as multipart with two parts in
flight and a whole-object digest computed as the parts are read; restores
stream each object to disk while its size and SHA-256 are checked; four
objects transfer at a time. Medians of three fresh-process trials after a
warm-up, client RSS only. Full report and the smaller earlier datasets in
[`backup-transfers/`](backup-transfers/README.md).

| Segment layout | Full backup, before → after | Restore, before → after |
|---|---:|---:|
| 160 × 64 MiB | 47.74 s → 26.08 s (1.83×) | 22.35 s → 13.60 s (1.64×) |
| 40 × 256 MiB | 42.91 s → 26.77 s (1.60×) | 20.20 s → 10.79 s (1.87×) |

Peak client RSS at 256 MiB segments fell from 264 to 97 MiB on backup and 267
to 29 MiB on restore; at 64 MiB segments backup RSS rose from 72 to 107 MiB,
the cost of four objects in flight. Loopback MinIO on local NVMe with the page
cache warm: these are transport figures, not cloud S3 figures, and the 1 GiB
datasets overstated the gain (3.29× against 1.83× here).

**Verdict: keep.** Format, incremental reuse, manifest-last publication and
staged restore are unchanged; the concurrency is a code default of four with
no operator knob yet.

## e70 — Segment slices as the scan's work unit (10M rows in 1M-row segments, 10 threads, memo off)

`crates/pintail-exec/tests/morsel_bench.rs`, two rounds interleaved with the
previous build, minimum of five runs, milliseconds. Before = whole segments
per scan thread bounded by the whole remaining ceiling; after = 131,072-row
slices, four per scan thread, half the remaining ceiling per round.

| query | before 1 | after 1 | before 2 | after 2 |
|---|---:|---:|---:|---:|
| two-pass int key (50 groups) | 135 | 129 | 128 | 127 |
| two-pass text key (5 groups) | 95 | 106 | 92 | 107 |
| general int+text keys (50 groups) | 460 | 457 | 446 | 462 |
| general int+text keys, 64 MiB | 887 | 881 | 884 | 865 |
| general expression key (10 groups) | 255 | 253 | 240 | 260 |
| general 200K groups | 4,974 | 4,851 | 4,775 | 4,473 |
| fused join + group (8 groups) | 111 | 95 | 86 | 101 |

One slice per scan thread read 119/119 ms on the text key and 144/147 on
the int key; four per thread is the setting kept. The text-key loss is the
memory bound: the old scan pulled all ten million rows at once under the
512 MiB ceiling, the sliced scan takes two rounds and idles between them.

**Verdict: keep.** Rows in flight no longer scale with segment size, the
scan keeps its width on large segments, and the one regression is the
bound itself; overlapping the next round's decode with the consumer is the
follow-up that would recover it.

## e71 — Skipping unread blocks (production-shaped wide table, in-process, memo off)

A copy of a two-segment table whose rows carry several wide text and JSON
columns (a few hundred megabytes on disk), opened read-only through the
exec harness; three runs each, milliseconds, warm page cache, laptop.

| query | before | after |
|---|---:|---:|
| ten-row primary-key range, key only | 58–96 (first 432) | 1.4 |
| equality on an unindexed integer column, three columns | 56–57 | 1.5 |
| range pruned by the manifest | 0.0 | 0.0 |

The sampler put the time in `read`, xxh3 hashing and zeroing of freshly
allocated payload buffers under the row-header and projected-row readers:
every block of every column was loaded and checksummed, whether or not the
scan wanted it. The same table on the deployment's host cost 225–495 ms per
scan before the fix. On the narrow benchmark-shaped table
(`morsel_bench`, four small columns, ten threads) the change is within
noise: the columns a query leaves out are cheap to read there.

**Verdict: keep.** Narrow reads now cost what they decode.

## e72 — One buffered write per wire response (loopback, release build, mysql2 client)

A local database of 100K three-column rows served over the wire on
loopback; three runs each, milliseconds for the whole batch. Before = the
packet writer wrote each packet's header and body straight to the socket;
after = packets accumulate and reach the socket in one write when the
response is flushed (or at 64 KiB); nodelay = the same plus `TCP_NODELAY`.

| batch | before | after | after + nodelay |
|---|---:|---:|---:|
| 2,000 × one-row query | 279–343 | 220–255 | 239–287 |
| 500 × 200-row range | 280–304 | 96–120 | 107–115 |
| 10 × 100K-row full result | 225–230 | 53–59 | 54–55 |

**Verdict: keep the buffering, leave `TCP_NODELAY` off.** The per-row
system calls were the cost; with one write per response there is nothing
left for Nagle to delay, and the option is neutral within noise.

## e73 — Direct decode with a memtable mask (300K-row segment, executor, release, memo off)

One unique-key segment of 300K rows and four columns, then 40 and then
2,000 scattered memtable updates; minimum of five runs, milliseconds.
Before = the memtable overlap sends the segment through the row-wise
merge; after = the overlay masks the superseded rows from a direct decode.

| query | no memtable | 40 updates before | 40 after | 2,000 before | 2,000 after |
|---|---:|---:|---:|---:|---:|
| `COUNT(*)` with a trivial predicate | 0.7 | 51.7 | 4.3 | 52.2 | 7.4 |
| five-key `IN` on a non-key column, three columns | 60.5 | 127.3 | 64.9 | 128.6 | 67.5 |
| three-key `IN` on the key, `SUM` | 36.0 | 92.5 | 39.4 | 93.3 | 41.8 |
| full four-column scan | 38.6 | — | 42.7 | — | 46.1 |

**Verdict: keep.** A table under live replication now scans at direct
speed plus one packed key column; the merge remains for the shapes the
overlay declines (stale versions, composite keys, partial segments,
version-retaining segments, key-ordered consumers).


## e74 — Dense packed aggregate lanes (synthetic table, in-process, memo off)

Ten million generated rows in 1M-row segments, 32 execution threads,
release build, five runs per case on the build host. Milliseconds below
are minimum / median, with operator profiling enabled through
`PINTAIL_BENCH_PROFILE=1` in `morsel_bench`. The settled memo is disabled.
Before is the development baseline; after includes bounded integer slots,
packed count/SUM lanes, and explicit persistent and worker slab bounds.

| case | before min / median | after min / median |
|---|---:|---:|
| two-pass text key, five groups | 86.8 / 90.7 | 22.3 / 25.4 |
| two-pass int key, fifty groups | 105.3 / 131.2 | 32.6 / 39.0 |
| general int+text keys | 505.5 / 508.5 | 519.2 / 543.0 |
| general int+text keys, 64 MiB | 784.5 / 801.7 | 764.9 / 927.9 |

The text path already had a dense table. Its remaining cost was repeated
column access and generic lane/state dispatch per row. Packed count and
integer SUM kernels resolve those choices once per batch; the integer
key path also avoids scattering and hashing for a bounded domain. The
mixed-key control keeps the general path and does not establish a speedup.
Its median varies more than its minimum on the shared machine.

For the five text-key profiles, aggregate self time changed from
78.2/83.0 ms minimum/median to 15.2/18.1 ms; scan self time from 7.7/7.8
to 7.1/7.3 ms. Scan peak reservation changed from 133.0 to 133.4 MiB,
including the newly reserved dense state bound. The aggregate still costs
more than the scan in this profile; the total-time target is met, rather
than a claim that their self times are equal.

The generated NULL/duplicate/domain-overflow comparisons pass for text,
signed and unsigned keys. One concurrent test run exposed a split group
in the general CONCAT-keyed reference (two rows with the same key whose
counts sum to the dense result); twenty isolated repeats and the subsequent
complete crate run passed. This intermittent reference-path observation
is not claimed fixed by the dense kernels. The final slice checks passed:
workspace clippy with warnings denied, formatting, and 464 executor/store
tests (three ignored measurements). No spill implementation was added.

**Verdict: keep.** Both single-key minima and medians improve materially;
the text aggregate's total is below the original 120 ms target. The fused
profile attributes later direct input pulls to the aggregate, so its self
time is not an isolated CPU-kernel measurement.


## e75 — Multi-column packed predicates and direct delta decode (synthetic, in-process)

The morsel fixture's integer grouping column supplies the selective
predicate; its increasing unsigned key supplies the range that excludes
nothing. Ten million generated rows, 32 scan and execution threads, release
build, memo disabled, five profiled runs per case. Values are milliseconds,
minimum / median. The baseline includes the dense-fold slice, which does
not change filtered COUNT execution.

| case | before min / median | after min / median |
|---|---:|---:|
| count, one predicate | 9.6 / 10.4 | 7.9 / 9.4 |
| count, two predicates | 37.5 / 38.4 | 9.7 / 10.5 |
| two-column result, one predicate | 187.8 / 214.2 | 198.3 / 202.0 |
| two-column result, two predicates | 175.4 / 187.3 | 17.6 / 18.2 |
| 150K rows: count, one predicate | 0.4 / 0.5 | 0.3 / 0.4 |
| 150K rows: count, two predicates | 2.8 / 2.9 | 0.9 / 0.9 |

The increasing key used delta bit packing, which the direct integer
reader declined. It decoded temporary normalized values and cells before
copying into its packed destination. Streaming deltas directly into that
destination first reduced the conjunction to 14.0/14.8 ms, but let prefetch
retain more slices: peak reservation rose from 74.9 to 151.8 MiB.
That intermediate result did not meet the memory target.

The complete change fuses a multi-column integer conjunction when all
projected columns are predicate inputs. It borrows the decoded columns
for exact signed/unsigned comparisons and compacts their survivors before
retaining the prefetch round, without decoding them again. Single-column
scans retain their existing path; non-integer or unsupported predicates
retain their existing evaluator. The outer filters still verify survivors.
The eligibility flag is computed at use rather than enlarging every scan.
The workspace report-shape gate also exposed a pre-existing join admission
double count: binned keys were reserved individually and added again to
the transient estimate. Direct decoding admitted larger inputs and exposed
that overcount at a 24 MiB ceiling. The check now includes the live batch
and incoming key, with previous keys counted once through the tracker.
A batch occupying more than half the remaining headroom also bins at most
1,024 rows before inserting, allowing the existing resident-map spill valve
to run before the temporary key list fills the ceiling.
The unchanged report-shape regression is checked with the full workspace build;
crate-only builds had passed even before this correction. Slice and round
sizing are unchanged, and G12's missing aggregate transient floor is untouched.
The count-conjunction scan self time
is now 9.7/10.4 ms, down from 37.3/38.2 ms; its peak reservation is 3.1 MiB.
The single-predicate baseline reserves 76.5 MiB. Before the footprint-only
correction, the same kernels measured 7.6/8.6 ms for the conjunction and
8.5/9.0 ms for the single predicate. The controls varied between rounds;
no speedup is claimed for either single-predicate control. Small-table conjunctions
remain slower than a single predicate, while improving over their baseline.

Tests compare signed and unsigned delta kernels against generic decoding
with duplicates, NULLs and disjoint ranges, reject overflow even in an
unselected suffix, and compare packed predicate scans with general
expressions over nullable generated columns and values above i64::MAX.
Final workspace clippy and formatting passed, followed by all 941 workspace
tests (28 ignored tests, including measurements and external harnesses).

**Verdict: keep.** The large-table conjunction costs less than 1.5 times
the single predicate at both minimum and median, and peak reservation falls.


## e76 — G2 and G3 on the benchmark replica (20M rows, 8 CPUs, memo off)

The synthetic morsel table established both closures; this is the same
pair of changes measured on the benchmark replica through
`benchmark/profile.ts`, scan and execution pools at 8x8, five runs, the
settled memo off. Milliseconds are the wall time of `EXPLAIN ANALYZE`
through the HTTP API, minimum of five, with the profile's own operator
self times beside them. Before is the rc9 tree; after is that tree plus
the two kernels.

| query | before min | after min | before aggregate self | after aggregate self |
|---|---:|---:|---:|---:|
| one predicate, filtered count | 48 | 46 | — | — |
| two predicates, filtered count | 155 | 74 | — | — |
| five-group aggregate | 131 | 118 | 86.9 | 73.7 |

The two-predicate count is the shape G2 was opened on. Its replica gain
matches the synthetic one in direction and lands inside the 1.5x target
against the single-predicate control, which did not move.

The five-group aggregate did not behave as the synthetic case predicted,
and the reason is that the synthetic case was the wrong shape. It groups
with COUNT and an integer SUM, which is exactly the packed fold; the
replica's query averages a decimal, whose lane the fold declines. So the
replica query never reached the new kernel, and it still got slower: 131
ms before, 156 ms after, with aggregate self time rising from 86.9 to
120.4 ms.

The cause was the fold's window chunking, not the fold. It cut a window
into exactly one chunk per pool thread, which bounds the partial slabs
but leaves rayon nothing to steal, so a window ends when its slowest
chunk does; batch cost varies with the groups a batch touches. Four
chunks per thread restores the balance, keeps the slab bound (computed
from the chunk count, and still declining the fold when it does not
fit), and the query runs 118 ms with 73.7 ms of aggregate self time,
ahead of the 131 ms and 86.9 ms it started at.

**Verdict: keep, and measure closures on the replica.** A synthetic case
that takes a different code path from the query it stands for can report
a fourfold gain while the real query regresses by a fifth. Both figures
above are now in the hardening todo's closure notes.


## e77 — What a materialized row costs, and what sorting on keys first would save

`crates/pintail-types/tests/layout.rs`, release, minimum of 200 runs on the
build host. The row is the shape a paginated report returns: nineteen
columns, mostly integers and short text, one wider text column. The counts
are the ones the delivery-report list actually meets, three thousand two
hundred candidates for fifty returned.

Prompted by a published account of shrinking a DNS cache's per-entry
footprint, which ranked its wins as: drop capacity fields from immutable
data, consolidate separate allocations, box oversized enum variants, and
stop building the structured representation at all in favour of raw bytes
parsed on demand. The last of those was worth the most there, and the
question here was which of the four transfers.

| fact | bytes |
|---|---:|
| `Value` | 32 |
| one row's `Value` structs | 608 |
| its text on the heap | 104 |
| its `Vec` header | 24 |
| **total per row** | **736** |
| what the row carries | 168 |

So a row costs 4.4 times what it holds, and three thousand two hundred of
them are 2.3 MiB. Two of the four techniques apply to that directly and
neither is large: boxing the text variants takes `Value` from 32 bytes to
24, a quarter of the inline cost, and `Box<[Value]>` in place of `Vec`
saves the 8-byte capacity field once per row.

The fourth technique is the one that transfers, in the form this engine
needs it: do not build the structured representation for rows nobody will
read.

| shape | ms |
|---|---:|
| build every candidate row, then sort, then keep fifty | 0.691 |
| sort the keys beside a row identity, keep fifty, build those | 0.007 |

**Verdict: late materialization, not a smaller `Value`.** Ordering keys
before building rows is worth about ninety-five times on this shape, and
shrinking `Value` is worth a quarter of one of its terms. The engine
currently takes the first shape: profiled against a 500,000-row mirror,
the delivery-report list spends 13.1 ms of a 20.5 ms query in a join that
materializes 3,200 rows so a top-50 sort can discard 3,150 of them, and
carrying fifteen more columns through that costs 11 ms of the 17.5 ms the
same query takes without its join.

Two changes follow, and they compose: sort on the keys and a row identity
and fetch the remaining columns only for the survivors, and push a top-K
through a left join whose build key is unique, which is what makes the
join's own output unnecessary for the rows that lose. Neither is a layout
change, and the layout changes are not worth doing first.


## e78 — A grouped aggregate served from per-segment partial states

`crates/pintail-exec/tests/segment_subcube.rs`, release, ten million rows
in ten one-million-row segments, five groups, minimum of five runs, memo
disabled so every run executes.

Prompted by reading how another engine keeps materialized views current:
a view is a trigger that runs over the block being inserted, and it stores
partial aggregate states rather than finished numbers so later inserts can
merge into them. Its refreshable variant instead recomputes the whole
query on a schedule and swaps the result atomically. Two things about that
design matter here. The partial-state idea is the valuable half. And the
incremental form is documented as not handling updates or deletes at all,
which a mirror of a mutable source cannot assume.

This engine already folds per-segment aggregates, but only when there is
no GROUP BY (`try_sma_fold`); the grouped case is a recorded gap in
`docs/limitations.md`. Segments are immutable, so a grouped sub-cube
written beside one can never go stale. The measurement asks what that
would be worth.

| shape | ms |
|---|---:|
| scan and aggregate, as today | 28.455 |
| merge ten segments' partials | 0.000 |
| merge partials, then 50,000 live memtable rows | 0.391 |

The settled ratio is not the interesting number; a whole-result memo
already serves a settled repeat. The interesting one is the last row.
Today an ingest invalidates the memo and the next query pays the full
28 ms again. Merging immutable per-segment partials and walking only what
is still in the memtable answers the same query in 0.391 ms, seventy-three
times faster, and that number holds under continuous replication because
a flush adds one more segment's partials rather than invalidating
anything.

**Verdict: worth building, in the grouped form, for additive aggregates
only.** COUNT and SUM merge from partials, and AVG follows from the two.
MIN and MAX cannot survive a delete, and COUNT(DISTINCT) cannot merge
without a sketch, so those decline the fold as the ungrouped path already
declines DISTINCT. Unlike the append-only design that prompted this, a
mirror sees updates and deletes: the memtable pass already carries the
tombstones and superseded versions, so correctness comes from the same
merge-on-read rule the scan uses, not from assuming an append-only source.

Where the state should live follows from what it costs to build. One
segment's partials take 6.8 ms to compute from its rows, so a cache built
lazily in memory pays that once per segment and nothing after:

| strategy | first query after a flush | every query after |
|---|---:|---:|
| scan, as today | 22.5 ms | 22.5 ms |
| build partials lazily, keep them in memory | 7.2 ms | 0.34 ms |
| build them during the flush | 0.34 ms | 0.34 ms |

The third row is not a faster algorithm; it is the same work moved to
where the rows are already in hand. A flush reads every row it writes, so
folding a sub-cube into that pass costs almost nothing, and the first
query after a flush stops paying for it.

That argues for both, in order. An in-memory cache keyed by segment needs
no format change and no migration, is bounded by eviction, and can be
dropped whole under memory pressure because it is only ever a cache of
something the segment can recompute. Building at flush time is the second
step and removes the remaining cost. Persisting it beside the segment is
the third, and only that one survives a restart.

The open question this does not answer is which group columns deserve a
sub-cube. Writing one per column per segment is unbounded; the shapes
worth it are low-cardinality columns that reports group by, which is what
the dense-fold work already identified as the common grouping key. A
bounded in-memory cache makes that question self-limiting in a way a
persisted format does not, which is a further argument for starting
there.


## e79 — What the overlay's mask costs, by how much changed

`crates/pintail-store/tests/mask_cost.rs`, release, ten million sorted
keys, minimum of five runs. All three masks are compared for equality at
every rate and every shape, so a faster one is not taking a shortcut the
others refuse.

**This entry replaces an earlier reading that was wrong in a way that
reached the shipped code.** The first version of this measurement timed a
mask built block by block - skip a block whose key range holds no change,
binary-search inside the block otherwise - while `overlay_positions`
actually does a plain lookup of each changed key against the whole key
column. The blocked variant searches thirteen levels in a warm 64 KiB
block; the real one searches twenty-three levels across eighty megabytes.
Timing the wrong algorithm put the crossover at about one row in five,
and `SEARCHED_OVERLAY_SHARE` was set from it, so every table between one
and five percent changed took the slower path. Corrected below and in the
constant.

The first version also changed only keys the segment already held, so the
insert path - a changed key that supersedes nothing and has to be placed
after the survivors and the inserts below it - was never timed, and it
scattered every change evenly, which is the shape the search does worst
on.

Three masks. **Linear** walks both sorted sides at once, visiting every
segment row, so its cost follows the table. **Searched** looks each
changed key up in the whole key column: what ships. **Narrowed** does the
same but starts each lookup where the last one landed, since the changed
keys are sorted.

Times in milliseconds, minimum of five runs:

| shape | changed | linear | searched | narrowed |
|---|---:|---:|---:|---:|
| scattered | 2 | 1.85 | 0.001 | 0.000 |
| scattered | 2,000 | 4.01 | 0.14 | 0.62 |
| scattered | 20,000 | 4.15 | 1.57 | 11.98 |
| scattered | 50,000 | 4.28 | 3.15 | 18.80 |
| scattered | 100,000 | 4.12 | 4.16 | 22.15 |
| scattered | 200,000 | 4.09 | 4.91 | 20.06 |
| scattered | 2,000,000 | 5.78 | 19.63 | 92.28 |
| clustered | 20,000 | 3.84 | 0.19 | 0.40 |
| clustered | 100,000 | 3.91 | 0.90 | 2.35 |
| clustered | 200,000 | 3.97 | 1.78 | 5.03 |
| clustered | 500,000 | 4.16 | 4.49 | 13.61 |
| half inserts | 20,000 | 4.04 | 1.58 | 11.65 |
| half inserts | 100,000 | 4.43 | 4.17 | 22.63 |
| all inserts | 20,000 | 4.09 | 1.58 | 11.55 |
| all inserts | 100,000 | 4.24 | 4.14 | 22.41 |

Four readings.

**The linear walk is flat at about four milliseconds whatever changed.**
That is the property worth removing: a table that took two updates pays
the same mask cost as one that took two million.

**The real crossover is one percent, not five.** Scattered changes, which
is the worst shape, break even at 100,000 of ten million rows. The
constant is now one in two hundred rather than one in twenty, choosing
the search only where it clearly wins rather than where the two are
level.

**Inserts cost what updates cost.** The half-insert and all-insert arms
track the scattered arm to within a few percent at every rate, so the
placement arithmetic is not a hidden cost - which also means the equality
assertion, not the timing, is what those arms are worth.

**Narrowing the search makes it far worse, and that is the useful
surprise.** Resuming each lookup from the previous match looks like a
strict improvement: a shorter search over a shrinking slice. It is three
to seven times slower than searching the whole column. A binary search
from a fixed base touches the same first ten levels every time and they
stay in cache; moving the base makes every search start on a cold line.
The obvious optimization is refuted, and the blocked variant that the
first version of this measurement accidentally timed is the shape worth
revisiting instead - it keeps a fixed base within each block.

**Clustered changes are far cheaper than scattered ones** - 20,000
changes cost 0.19 ms clustered against 1.57 ms scattered, and the
crossover moves out past two percent. Updates in practice cluster towards
recent rows. The threshold does not look at the shape, and could: the
memtable keys are sorted, so the span between the first and the last,
against the segment's, separates the two cases for the price of two
comparisons. Not built; recorded as measured.

**Verdict: pick the mask by how much changed, not by range overlap** -
with the crossover taken from the algorithm that ships. A membership
filter is the answer to a different question, one where the changed set
is too large to hold and too large to sort, and this measurement says
that is not the regime a replicated table sits in.


## e80 — Maintaining the supersession mask instead of rebuilding it

`crates/pintail-store/tests/supersession_bitmap.rs`, release, ten million
rows, twenty thousand changed, minimum of five runs. Both masks are
compared for equality, so the maintained one marks exactly the rows the
rebuilt one does.

e79 made the mask cost follow the change rather than the table. It left
one thing untouched: every scan still rebuilds it. A row's position in a
segment does not move, so the work of finding it can be done once when
the row arrives instead of once per query.

Two shapes of change, because the first version of this measurement had
only the scattered one and read the pessimistic end as the answer.
Scattered changes touch every part of the key column and no lookup reuses
a line the last one warmed; clustered changes, which is what a burst of
recent activity leaves, reuse the same lines repeatedly.

| step | scattered, ms | clustered, ms |
|---|---:|---:|
| rebuild the mask, per scan | 1.572 | 0.169 |
| mark all twenty thousand as they arrive | 0.718 | 0.184 |
| the same, per changed row | 36 ns | 9 ns |
| read a mask already built | 0.062 | 0.062 |

At two thousand updates a second, work per second by query rate:

| queries/s | rebuild, ms/s | maintain, ms/s | rebuild clustered | maintain clustered |
|---:|---:|---:|---:|---:|
| 1 | 1.6 | 0.1 | 0.2 | 0.1 |
| 5 | 7.9 | 0.4 | 0.8 | 0.3 |
| 10 | 15.7 | 0.7 | 1.7 | 0.6 |
| 50 | 78.6 | 3.2 | 8.5 | 3.1 |

**The clustered column is the one that changes the verdict's size.**
Rebuilding a clustered mask costs 0.169 ms, nine times less than a
scattered one, so maintaining it is worth 1.7x at ten queries a second
rather than 22x. The scattered figures are the ceiling on the value here,
not the expectation, and the earlier version of this entry quoted the
ceiling.

Rebuilding still scales with queries and maintaining with changes, so the
lines cross either way; how far apart they run afterwards depends on a
shape this measurement now reports instead of assuming.

Two pieces of outside reading shaped this. A survey of incremental view
maintenance in semiring terms gives the rule for which aggregates can be
kept current under deletion: the payload must have an additive inverse,
so the effect of a row can be undone. COUNT and SUM have one and MIN and
MAX do not, which is the split e78 arrived at by argument and this
supplies the reason for. Separately, lakehouse formats moved from
rewriting files on delete to carrying a per-file bitmap of removed row
positions, which is the same shape as this mask; the difference here is
that a mirror supersedes rather than deletes, and the bitmap has to be
rebuilt when a flush changes the segment set.

**Verdict: maintain it, and derive it from what CDC already knows.** The
apply path already has the key of every row it writes and already reads
the segment key column to place it, so the position lookup is work that
path can absorb. The scan then reads a bitmap. The open question is the
lifetime: a bitmap belongs to a (segment, memtable generation) pair, so a
flush retires it, and the cost of rebuilding one after a flush is the
0.797 ms measured above, paid once.


## e81 — What a merging scan costs against a direct one, on the same rows

`crates/pintail-store/tests/merge_output.rs`, release, two million rows of
four columns, twenty thousand of them changed and flushed so the scan
meets two overlapping segments. The comparison is the same rows and the
same columns with nothing to merge.

**Re-timed.** The first reading built and measured the merging store, then
built and measured the direct one, so the two arms met different allocator
and page-cache states and a hundredfold ratio rested on the order they ran
in. Both stores are now built before either is measured, a warm-up round
of each is discarded, and the arms alternate - swapping which goes first
inside each round. The result is unchanged, which is what the re-timing
was for.

| scan | ms |
|---|---:|
| direct, one segment, packed columns | 13.6 |
| merging, 1% of rows changed | 1445.4 |
| the merge costs | 106x |

One row in a hundred changing makes the scan a hundred times slower. Two
things account for it and neither is the winner-selection logic, which is
a cheap walk of already-sorted heads.

The first is the gather. A merging chunk asks its segment for scattered
row indices, so it decodes per row rather than per block, and gives up
every advantage the block layout has. The direct path reads ranges.

The second is the representation. The direct path hands back packed typed
columns; the merging path hands back `Value` per cell, which e77 measured
at 32 bytes to carry 8, with an allocation for every string.

Removing the two transposes the merging path used to do between those
steps, turning its fetch into rows and back into columns, was measured at
1546.7 against 1433.7 ms on the earlier, ordering-sensitive setup: about
6%. Worth keeping, since the work was
pure waste, but it is not the cliff and this records that plainly.

**The fix is to make a merging scan look like a direct one.** At one
percent churn the winning rows form long contiguous runs, so the winner
indices can be expressed as ranges and read with the same ranged, packed
reader the direct path uses, with the few memtable winners placed into the
resulting typed columns. That is a contained change with a clear target:
this scan should cost nearer 14 ms than 1547.

The number also reframes the compaction gap recorded as G10. An
update-heavy table that has flushed once sits on a base and an overlapping
tail and merges on every scan until two more flushes arrive; at these
figures that is not a tidiness problem, it is a hundredfold slowdown
persisting until compaction happens to run.

## e82 — One execution answering a burst of identical reads

`crates/pintail-wire/tests/shared_query_burst.rs`, ignored. A local
database of 200,000 rows, sixteen threads released together on one
grouped aggregate, five bursts, the best reported.
`PINTAIL_DISABLE_SHARED_QUERIES=1` runs the arm where each request
executes for itself; `PINTAIL_DISABLE_SETTLED_MEMO=1` crosses it with the
settled aggregate memo, to show the two are independent.

| host shape | each request executes | one execution answers all | ratio |
|---|---:|---:|---:|
| 32 cores | 71.4 ms | 42.7 ms | 1.67× |
| 32 cores, settled memo off | 72.5 ms | 45.2 ms | 1.60× |
| 4 cores (`taskset -c 0-3`) | 288.4 ms | 80.9 ms | 3.57× |

Executions, in every shared run: sixteen requests, one execution. Across
five bursts the counters read five led and seventy-five answered by
another, with nothing falling back or refused.

Two readings matter more than the ratio.

The first is that the ratio is a function of how much spare CPU the host
has. On 32 idle cores the sixteen executions mostly run at once, so
deleting fifteen of them saves less than a third of the wall clock. On
four cores they queue, and the same deletion is worth 3.6×. A server
doing nothing else gains little; a server under load - the case a refresh
storm creates, and the case that produced the 503s - gains most. Quoting
a single speedup for this would be quoting the idle host.

The second is that it composes with the settled aggregate memo rather
than duplicating it. The memo answers a repeat of a settled query; this
answers a *simultaneous* copy, settled or not, and the memo-off row shows
the gain is the same size without it.

What is not measured here: the admission permit. A waiting request keeps
the permit it took, so this removes executions rather than freeing slots,
and the throughput it buys is the queue draining faster rather than more
queries being admitted at once.

## e83 — The resource sampler was racing an SSH connection, not the query

`benchmark/run.ts`'s CPU/memory sampler ran `docker stats --no-stream`
once every 250 ms in a loop. Over the ssh:// docker context that call pays
a fresh SSH round trip each time; on a sub-second query the sampler could
start and stop without a single `--no-stream` call completing, which is
why `benchmark/results.md`'s CPU column reads 0% on six of the eight
queries in the "Engine speed (memo DISABLED)" row set despite one of them
(Q6) also showing 39% from a run where a call happened to land.

Fix: one `docker stats <container>` (streaming, not `--no-stream`) spawned
per container the first time it is sampled and left running for the rest
of the process; `sampled()` now marks a start/stop index into that
stream's growing sample list instead of spawning a process per tick. The
streaming format turned out to interleave cursor-home/clear-line/clear-
screen escape codes with each refresh (a mode built for a redrawn
terminal, not a pipe), so a data line opens with `\x1b[H` and closes with
`\x1b[K`; the reader strips `\x1b\[[0-9;]*[A-Za-z]` before parsing.

Verified locally (Docker Desktop, not the shared benchmark host — this
checks the mechanism, not a query's real CPU%): a container running four
CPU-bound loops under `--cpus=4`, sampled for 4 seconds. Before the fix,
`--no-stream` in a loop over a local (non-SSH) daemon still occasionally
returns zero samples within a short window because the loop's own 250 ms
`Bun.sleep` plus process-spawn latency can outlast the window; after the
fix, the same window reliably reads several samples with peak CPU near
the container's 400% ceiling:

| approach | window | samples seen | peak CPU read |
|---|---:|---:|---:|
| `--no-stream` loop (`benchmark/run.ts` before) | 4 s | 0 | 0% |
| long-lived stream (after) | 4 s | 7 | 401% |

The SSH round-trip cost that motivated this — and that produces the 0%
rows in `benchmark/results.md` — only reproduces on the shared remote
docker host; re-running the full benchmark to confirm the fixed column is
the owner's call (`benchmark/run.ts` is the stable-release gate, not a
mid-flow tool).

**Verdict: keep.** No engine code changed; this only makes the evidence
the harness already collects honest.

## e84 — The HTTP path's fixed cost: one engine, one auth cache (loopback, release build, local database)

`execute_query` (`crates/pintail-api/src/query.rs`) built a fresh
`ReplicaEngine` per request and mapped every value through an intermediate
`serde_json::Value` tree; `authenticate_api_key`
(`crates/pintail-api/src/auth.rs`) hashed and looked the key up in
metadata on every call. e65 measured this at 25-40 ms outside the engine
per query on the 20M-row benchmark.

Fixes: one `ReplicaEngine` held on `ApiState` and cloned per request
(shares its metadata-signature memo and signature-reader connection,
which a fresh instance loses); an in-process API-key cache keyed by the
presented secret's SHA-256, TTL 30s, cleared immediately on
disable/delete; rows serialize straight from `Value` into the response
writer via a manual `Serialize` impl (`JsonRows`) instead of building a
`Vec<Vec<serde_json::Value>>` first. Timings behind `PINTAIL_API_DEBUG`.

Measured against a release build on loopback with a LOCAL database
(`POST /api/databases/local` - no MySQL/CDC involved, so this isolates
the HTTP/auth/engine path from scan or aggregate cost) running `SELECT
1`, five calls:

| call | engine ms | spawn_blocking ms | reshape ms |
|---|---:|---:|---:|
| 1st (cold) | 0.00 | 1.20 | 0.00 |
| 2nd-5th | 0.00 | 0.08-0.16 | 0.00 |

`engine=0.00ms` on every call: building the engine is now an `Arc` clone.
`spawn_blocking` drops after the first call because the metadata-signature
memo and signature-reader connection now survive between requests instead
of being rebuilt every time. API-key auth: 0.43 ms on the first request
(a real metadata hit), 0.00 ms on the next two (cache hit); disabling the
key made the very next request 401 immediately (TTL invalidation is not
what caught it - the explicit `invalidate_api_key` call on disable was).

Not measured here: the JSON-tree-vs-direct-serialize difference on a real
row set (`SELECT 1` is one row) and the full 20M-row benchmark's HTTP
column, both of which need the containerized replica and are the owner's
next full run to bank.

**Verdict: keep.** `crates/pintail-api/src/query.rs` and `state.rs` gained
unit tests for the new serialization shape and the cache's TTL/invalidation;
the crate's existing HTTP integration suite (which exercises the full
request path) passes unchanged.

## e85 — Dense join table extended to every reader; batching the probe measured negative (10M rows, 100K-key build, 32 threads, memo off)

`crates/pintail-exec/tests/morsel_bench.rs`, new case "fused join + group,
100K-key dim": a 100,000-row dimension table (8 distinct region names, like
`benchmark/queries.ts`'s Q8) joined to the 10M-row fact table and grouped
by region - the shape the fused join-aggregate's dense probe already had
in reach, at Q8's real cardinality rather than the existing 50-row-dim
case's. Minimum of 7-9 runs each; the host's spread across runs was real
(medians moved more than the effect being measured), so the minimum is
the number read, matching this file's convention elsewhere.

| build | min | median |
|---|---:|---:|
| before (fused-only dense table, per-row `plan.buckets` address lookup) | 156.5 ms | 162.6 ms |
| after (`PartitionedBuild` finalizes dense in place; group indexes resolved once per key) | 143.2-151.1 ms (three runs) | 148.6-170.1 ms |
| after, plus batching the probe into two passes | 143.6-180.9 ms | 148.6-184.5 ms |

The single-pass version is a real, modest win (~5-9% at the minimum,
consistent across three separate runs never exceeding the before
figure). The two-pass version - precompute every row's dense offset in
one pass, fold in a second - was tried because the brief called for it
directly; measured, it made the same case slower on one run (180.9 ms)
and no better than the single-pass version on the others. At this
build size (100K distinct keys, comfortably inside cache) the dense
table gather was not the bottleneck the two-pass split was written to
fix, and the extra `Vec` allocation plus a second full traversal per
morsel cost more than it saved. Reverted; see "The dense join table
lives inside `PartitionedBuild`, not beside it" in `docs/decisions.md`
for what was kept.

Full crate suite (350 existing + 3 new `dense_join_table_tests`) passes
unchanged, including the fused-join-and-spill, mixed-collation-join, and
join-accounting tests that already exercised this path.

**Verdict: keep the single-pass dense extension; drop the two-pass
batching.** `PartitionedBuild::get` is now dense-aware for every caller,
not only the fused aggregate, closing that part of item 2 in
`docs/design/production-hardening-todo.md` section H; the probe-batching
half of that item did not survive measurement.

## e86 — Bitset `COUNT(DISTINCT)`: a real 25% win, after a thrashing bug measured 1.5-30x slower

`crates/pintail-exec/tests/morsel_bench.rs`, new case "count distinct,
100K-value column": Q7's shape (`benchmark/queries.ts`) - a handful of
groups, each counting `COUNT(DISTINCT id % 100000)` over its share of
10M rows, so the column's real cardinality (100,000) is far above the
group count and comfortably inside the new bitmap's span cap. Minimum of
7-9 runs.

`DistinctSeen`'s existing `Ints` variant (a `HashSet<i128>`, already
faster than the general `Value`-keyed path per e16) now promotes to a
bitmap once a group's distinct integers pass 64 in count and span fewer
than `DISTINCT_BITMAP_MAX_SPAN` (2^20) values - trading the hash-and-probe
per key for one bit test/set.

First attempt, measured: **slower**, not faster.

| build | 10M rows, min | 150K rows, min |
|---|---:|---:|
| before (HashSet only) | 710.1 ms | 14.2 ms |
| bitmap, demote-and-immediately-retry past the window | 1,102.6-1,262.2 ms | 440.8-458.8 ms |
| bitmap, grow the window to the exact new bound | (not separately measured - same failure mode) | |
| bitmap, grow with doubling headroom | **523.5-533.0 ms** | **11.8-12.7 ms** |

Cause: `id % 100_000` seen in roughly ascending order widens a group's
observed span by a handful of values at a time for a long stretch before
it has covered the column's real range. The first design demoted a
bitmap back to `Ints` the moment a new key fell outside the window it was
built with, then immediately re-promoted at the (slightly) wider span on
the very next `insert_int` call inside the same retry - converting the
member set to a `HashSet` and back to a fresh array on nearly every new
distinct value, for as long as the range kept widening. Growing the
window in place to the exact new bound instead of demoting has the same
failure shape one level down: reallocating and copying the whole bitmap
on every insert that pushes the bound out by one. Growing with doubling
headroom (in the direction that just grew, capped at the span limit) is
what fixed it - the same amortized-growth trick `Vec` itself uses - and
is what is banked here: a handful of reallocations total instead of one
per insert. The 150K-row case is the sharper signal: fewer real rows
means the per-insert reallocation overhead so dominated the first two
attempts that they were 30-70x slower than doing nothing at all.

Full crate suite (357 tests, four of them
`distinct_bitmap_tests` new for this) passes, including the existing
distinct-under-spill test.

**Verdict: keep the doubling-headroom version.** ~25% faster at the
minimum on the 10M-row case, consistent across three separate runs. Not
banked here: `docs/decisions.md` records the alternative (demote vs.
grow vs. grow-with-headroom) for section H item 3 in
`docs/design/production-hardening-todo.md`.

Addendum, caught by `--profile rc`: the `min`/`max` fields this entry's
design added to `DistinctSeen::Ints`, and the `min` field on `Bitmap`,
are each an `i128` sitting directly in an enum variant - which forces the
WHOLE enum to 16-byte alignment and pads its size up, in every
`AggregateState` a query holds, whether or not that group's distinct set
ever touches the bitmap path. `tests/sqllogic/tests/two_pass_spill.rs`
holds hundreds of thousands of `AggregateState`s live under a tight
24 MiB ceiling specifically to exercise its spill path; the padding was
enough to push it past a spill the unboxed version used to make cleanly,
and the gate caught it (`unit` stage, `a_spilled_two_pass_aggregation_
matches_the_in_memory_groups_exactly`). Boxing both payloads
(`Ints(Box<IntsSeen>)`, `Bitmap(Box<BitmapSeen>)`) removes every inline
`i128` from the enum and restored `size_of::<AggregateState>()` to
exactly its pre-entry value (192 bytes, measured directly); the test
passes again. The 25% figure above was measured before this fix and is
unaffected by it - boxing only removes memory the design never needed to
spend, and does not change the insert path's instruction count.

## e87 — Q6's real shape is already on the two-pass streaming path; the naive-materialization premise was stale (10M rows, 32 threads, memo off)

The brief for section H item 4 described Q6 as "the general partitioned
aggregate, a full materialization, then sort.rs `materialize_top_k`" and
asked for radix-partitioned parallel aggregation feeding a streaming
top-K heap. Measured instead of assumed:

`crates/pintail-exec/tests/morsel_bench.rs`, new case "top 10 by sum,
200K-value column" - `GROUP BY grp` on a bare, stored integer column with
200,000 distinct values (Q6's own cardinality), `COUNT(*)` and `SUM`,
`ORDER BY total_spent DESC, grp LIMIT 10` - Q6's exact shape
(`benchmark/queries.ts`), unlike the existing "general high cardinality"
case, which groups by the EXPRESSION `id % 200000`: `column_index()`
cannot resolve an expression to a plain column, so that case never
reaches `build_direct_column_aggregate`'s direct/two-pass routing at all
and measures a different, slower path (4.0-6.1 s in e85's matrix) that
Q6 does not run.

| case | shape | min |
|---|---|---:|
| "general high cardinality" (pre-existing) | `GROUP BY id % 200000` (expression) | 4,205 ms |
| "top 10 by sum, 200K-value column" (this entry) | `GROUP BY grp` (bare column, Q6's shape) | 223.0 ms |

`build_buffered_hash_aggregate`/`build_direct_column_aggregate` already
route a single bare int-typed group column with `COUNT`/`SUM`-shaped
aggregates to `build_streaming_two_pass_aggregate` (e13: banked
4.2-8.9x), not the general `HashMap<Vec<Value>, AggregateGroup>` path.
223 ms for 10M rows scales close to linearly to the 20M-row benchmark's
banked 420 ms for real Q6 - the existing optimization already accounts
for most of the gap the brief attributed to "full materialization."

A comment already in `build_buffered_hash_aggregate` (above
`build_direct_column_aggregate`'s call site) records that this exact
question was tried before: routing the single-int-column case through
the parallel morsel/merge path "regressed Q6 (2M groups over 20M rows)
from seconds to minutes," because the dense-array parallel win that
helps LOW-cardinality keys does not transfer to sparse high-cardinality
ones, and closes with "Parallel high-cardinality aggregation needs a
partitioned design and its own experiment first." Radix-partitioning the
build key so each worker owns a disjoint range and can safely call its
own groups finished - the precondition for streaming them into a top-K
heap without a cross-worker merge - is a new execution-model capability
(a hash-based shuffle/exchange stage), not a local change to
`sort.rs::materialize_top_k` or to the two-pass aggregate: no query in
the current planner or executor partitions its parallel work by key
rather than by row range. Attempting it inside this brief's remaining
scope, on top of the correctness surface a change like that touches
(admission, memory tracking across N partition buffers, interaction with
existing spill/two-pass paths) risked exactly the kind of regression the
comment already describes, without the dedicated measurement budget the
comment says it needs.

**Verdict: not attempted.** The measured baseline for Q6's actual shape
(223 ms at 10M rows, this entry) is the number future work on this item
should compare against - not the 4,205 ms "general high cardinality"
case, which is a different, already-slow path Q6 does not take.
`docs/design/production-hardening-todo.md` section H keeps item 4 open
with this finding attached, so it is not re-discovered from a stale
premise.

## e88 — Scan pool default doubled; no gain reproduced on bare metal, and why that is expected (10M rows, in-process, memo off)

e65 measured Q2's shape 16 scan threads against 8 (66ms -> 57ms) on the
`--cpus=8`-limited container the release benchmark and a typical
deployment run under, and recommended defaulting the scan pool to twice
the CPU count. Implemented: `projected_scan_pool` now defaults to
`available_parallelism() * 2`, still overridable by
`PINTAIL_SCAN_THREADS`.

Measured on this machine (32 real, unthrottled CPUs, local NVMe) with a
new `crates/pintail-exec/tests/morsel_bench.rs` case, "scan: filtered
count" (Q2's shape, `WHERE status = 'open'`), minimum of 9 runs:

| scan threads | min | median |
|---|---:|---:|
| 32 (= CPU count, old default) | 10.6 ms | 10.9 ms |
| 64 (= 2x CPU count, new default) | 11.1 ms | 11.5 ms |

No gain here - if anything, slightly worse at the median, from
scheduling more runnable threads than there are cores with nothing to
overlap. This is the expected result, not a contradiction of e65: e65's
win comes from a scan thread parked by a CPU quota tick still having
another one ready to run, which only exists under a CPU-limited
container; a bare-metal host with a real core per thread has no quota
stall to hide behind, so doubling the pool only adds contention on a
purely CPU-bound decode. Reproducing e65's own container conditions to
confirm the win still holds was not attempted here (would need a
throttled container on the shared docker host, which this brief's
protocol reserves for the release benchmark, not an ad hoc check).

**Verdict: keep the default change**, on the strength of e65's original
container measurement (the actual release/deployment shape), with this
entry as the honest record that the bare-metal dev host shows no
benefit - `PINTAIL_SCAN_THREADS` remains the escape hatch either way.

## e89 — Overlapping the next scan round's decode: investigated, not attempted

e70 measured the sliced scan's own regression (a two-round text-key
query losing 95ms -> 106ms to idling between rounds) and named the
follow-up: prefetch the next round's decode while the consumer works the
current one. Investigated instead of implemented, because the shape of
`ProjectedScanStream::next_column_chunks_inner`
(`crates/pintail-store/src/store/scan.rs`) does not allow it without a
larger change first:

- The parallel decode call (`decode_slice` over the round's slices) is a
  method on `&self`, called synchronously inside the same function that
  will be called again with `&mut self` for the NEXT round. Starting that
  decode on a background thread so it can run while the caller consumes
  the current round's chunks means that background thread's borrow of
  `self` would need to outlive the current call - the same class of
  problem item 2's join dense table hit, and for the same reason
  (`unsafe_code = "forbid"`) not solvable by holding a raw reference
  across the boundary.
- `decode_slice` also takes `prewhere: Option<(&[u32], PrewhereSelect<'_>)>`,
  a borrowed predicate scoped to the CURRENT call by its caller in
  `pintail-exec` - not a struct field, so even an `Arc`-based redesign of
  the scan state would still need this cloned or restructured into
  something `'static` and `Send` before a background task could hold it
  across calls.

Both are solvable - the general shape is "give the decode context to a
background task instead of borrowing it," which likely means the slice
decode's dependencies (segment, directory, schema, prewhere) need to be
extracted into an owned, `Send` unit callable from a free function rather
than a `&self` method - but that is a restructuring of the scan's
internals, not a bounded follow-up to the change this brief's scan
threading item already made. Deferred rather than attempted under time
pressure on a path this exact test suite's oracle depends on for every
predicate shape.

**Verdict: not attempted.** `docs/design/production-hardening-todo.md`
section H keeps this half of item 5 open with the specific blocker
recorded, so a future attempt starts from the ownership question rather
than rediscovering it.

## e90 — A pre-existing, nondeterministic wrong answer in `AVG` on a decimal column, found chasing the scan-pool default

First seen with the scan pool defaulted to twice the CPU count (e88's
change): `--profile rc`'s `e2e` stage failed on `tests/e2e/queries.ts`'s
"decimal column average beyond simple sum" -
`SELECT customer_id, ROUND(AVG(total), 4), ROUND(SUM(total) / COUNT(*),
4) FROM orders GROUP BY customer_id HAVING COUNT(*) >= 2 ORDER BY
avg_total DESC, customer_id LIMIT 20` - returned `330.8824` at row 3
where MySQL and this engine's own `SUM(total) / COUNT(*)` column both
read `330.8823`, on a plain `GROUP BY` with no join. Reverting
`PINTAIL_SCAN_THREADS` to the CPU count made that run pass, which read
at the time as confirmation that the doubled pool was the cause.

It was not, or not only: a later `--profile rc` run, on the same commit
with the scan pool already reverted to the CPU count, failed the exact
same check again - this time at a different row (11, not 3) and a
different value (`324.2510` against MySQL's `324.2509`). Same query
shape, same mismatch pattern (`AVG` wrong, `SUM(total)/COUNT(*)` on the
same rows correct), different data point each time. That rules out the
scan-pool width as the cause: this is a pre-existing, run-to-run
nondeterministic defect that the wider pool very likely made MORE
frequent (Rust's default hasher reseeds every process, so `HashMap`
iteration order - and with it, morsel-to-worker assignment and merge
order in anything built on rayon's work-stealing scheduler - differs
between runs of the identical binary on the identical data regardless of
thread count; a wider pool gives that nondeterminism more ways to land on
whatever ordering triggers this), but did not introduce.

The AVG lane itself is exact by construction: `TwoPassLane::DecimalUnits`
rescales each row's decimal units by a fixed power of ten
(`decimal_units_from_int`, an exact `checked_mul`) chosen once from the
aggregate's planned output scale (`decimal_average_scale`, a property of
the bound query, not of runtime data or thread count), and
`update_decimal_average_units` accumulates the rescaled units with
`checked_add` - exact integer addition, order-independent by definition.
That `SUM(total) / COUNT(*)` came back byte-correct on the same rows
both times points away from a data completeness problem (a dropped or
duplicated row would move both columns) and toward `AVG` specifically
taking a run-to-run-varying computation path - most plausibly the general
aggregate's own average, which (unlike the two-pass exact-units lane) may
accumulate through `f64`. Not confirmed by tracing an actual run with
instrumentation, and not established whether this reproduces on `dev`
before any of this brief's commits - time did not extend to a control run
against a bisected base commit.

**Verdict: the scan-pool default stays reverted** (back to the CPU count,
`docs/design/production-hardening-todo.md` H5a) regardless - e88 already
found no benefit from doubling it on bare metal, so there is no upside to
weigh against even a possible (not confirmed) increase in how often this
pre-existing defect surfaces. The defect itself is unrelated to anything
else in this brief and is recorded as new work (section G, G14) rather
than worked around here.

## e91 — Compacting an overlap, against paying for it on every scan

`crates/pintail-store/tests/merge_output.rs`, release, two million rows of
four columns with one percent changed and flushed, so the table holds one
base segment and one small overlapping tail. That is the shape a table
takes for as long as it takes two more flushes to arrive.

e81 measured what the overlap costs a reader. This measures the other
side: what removing it costs a writer, so the policy can be argued from
both.

| | ms |
|---|---:|
| scan while the two segments overlap | 1430.5 |
| compacting them, once | 1780.5 |
| scan afterwards | 18.9 |
| **the rewrite repays after** | **1.1 scans** |

**The policy declined to do it.** Before this entry's change,
`compaction_status()` on exactly this store read `segment_count: 2,
eligible_segments: 0, debt_bytes: 0`, and `compact()` returned
`input_segments: 0` - a no-op. `compaction_plan` returned `None` before
overlap was ever considered, because two segments is fewer than the
default fan-in of four. A table in this state stays a hundredfold slow to
read until two more flushes arrive, however often it is queried.

The fan-in is the right instinct when merging only saves file handles:
rewriting a base to absorb a tail a hundredth its size is poor value for
fewer files, and `admits_window`'s size tier refuses that pairing for good
reason. An overlap is a different prize. A key in two segments puts every
scan on the merging path, so the rewrite buys back 1411 ms per scan and
costs 1780 ms once.

With overlap admitted below the fan-in and outside the size tier - the
per-pass row budget still applies, so one pass stays bounded - the same
store plans `eligible_segments: 2`, compacts in 1780.5 ms, and reads in
18.9 ms. **A 76x improvement on the scan, from a policy change rather
than a rewrite of the merge path.**

What this does not settle: a table written far more often than it is read.
Every flush creates a fresh overlap, so the trigger fires per flush, and
1.1 scans of payback is only a bargain if those scans happen. The bound
that exists is `max_compaction_input_rows` per pass; a read-rate-aware
trigger is not built and is the thing to reach for if a write-heavy table
is seen compacting without being queried.

The merge path itself is untouched and still costs what e81 says. This
narrows how long a table sits on it; it does not make it cheaper.

## e92 — What a table pays while it is being written to

`crates/pintail-store/tests/merge_output.rs`, release, two million rows,
one stamped segment plus a memtable. Ignored measurements.

**The overlay does its job.** One projected column, varying only how many
rows sit in the memtable, against a direct scan of the same rows at
6.6 ms:

| rows changed | overlay ms | against a direct scan |
|---:|---:|---:|
| 1 | 6.9 | 1.0x |
| 10 | 10.0 | 1.5x |
| 100 | 11.8 | 1.8x |
| 1,000 | 12.1 | 1.8x |
| 10,000 | 14.8 | 2.2x |
| 20,000 | 16.4 | 2.5x |

Over all four columns at twenty thousand changed rows the overlay reads in
162.5 ms against 23.1 ms direct, 7.0x - the wider projection carries more
of the memtable's rows into the output, so the ratio grows with what is
projected as well as with what changed.

A mirrored table under continuous replication reads at close to its
quiescent speed, and the cost grows with what actually changed rather than
with the size of the table. That is what the overlay was built for and it
is worth recording as confirmed rather than assumed.

**The first version of this entry claimed the opposite** - a flat 63x
whatever changed - and was wrong in a way worth writing down. The overlay
is opt-in: `enable_memtable_overlay` must be called before the first
chunk, and a scan that does not call it falls back to merging the segment
with the memtable row by row. `pintail-exec/src/storage.rs` calls it; the
measurement did not. So the flat 63x was real, but it was the merge
fallback, measured against a path production never takes and reported as
the path it always takes.

**What survives is narrower and still worth having.**
`enable_memtable_overlay` refuses unless EVERY key column is an integer
type, and a scan it refuses gets no `ScanPart::Overlay` at all. So a table
whose primary key has a text, decimal or temporal part pays the merge path
on every scan for as long as its memtable is non-empty - which under
replication is always. The accidental measurement quantifies that case:
63x on one projected column, 84x on four, flat from one changed row to
twenty thousand, because the fallback is a property of the key's type
rather than of how much changed.

That is worth confirming with a text-keyed fixture before it is acted on,
which this entry does not do.

Three explanations for the flat cost were measured and refused before the
opt-in was found, and they stay refuted for the merge path they were
actually describing: it is not the per-row `Value` materialization in
`interleave` (giving text, float and dictionary columns typed paths moved
nothing), not the fragmentation of the mask (one excluded row costs what
twenty thousand do), and not the slice width (one slice costs what
sixteen do).

## e93 — When the per-segment fold can serve a query at all

`crates/pintail-store/tests/fold_eligibility.rs`, release, one hundred
thousand rows in one segment. Ignored: an eligibility measurement.

e78 measured a grouped aggregate served from per-segment partials at
seventy-three times the scan, and argued the number holds under continuous
replication because a flush adds a segment's partials rather than
invalidating a result. Before building that, this asks the prior question:
on which tables does the fold engage?

`Snapshot::sma_fold_state` is the gate, and it is stricter than "the
segments are immutable". Every memtable row must be an insert ABOVE the
segment key space; a row at or below the segments' maximum key returns
`None` for the whole table.

| what is in the memtable | fold eligible |
|---|---|
| nothing | yes |
| one insert above the segment | yes |
| fifty thousand inserts above the segment | yes |
| **one update of a row the segment holds** | **no** |
| **one delete of a row the segment holds** | **no** |

**So the fold serves append-only tables, and one update anywhere in the
table disqualifies it entirely.** e78's fixture inserts with increasing
keys, which is why its seventy-three times looked general. A mirrored
table whose rows are inserted and then updated in place - a record that
gains timestamps as it progresses through states, which is an ordinary
shape - is disqualified by its first update and stays disqualified.

Building grouped partials on this eligibility would therefore buy nothing
for an update-carrying mirror, which is the case that motivated it.

**The segment half of the gate is already satisfied, and compaction is
what satisfies it.** The same fixture, asking about segment disjointness
rather than the memtable:

| state | fold eligible |
|---|---|
| one segment, empty memtable | yes |
| after flushing 1,000 scattered updates | no - the segments overlap |
| after compaction merges them | **yes** |

So a flush disqualifies a table and a compaction re-qualifies it, and e91
made that compaction prompt rather than something that waits for a fourth
segment. Only the memtable condition is left.

**What a version that served updates would need**, recorded so the design
is not re-derived: partials per segment, plus a correction per memtable
row that supersedes a segment row - read the superseded row, subtract its
contribution, add the new one. That bounds the work by the memtable rather
than the table, and the residual cap already keeps it small. It restricts
the aggregates to those whose payload has an additive inverse: COUNT and
SUM can be corrected, MIN and MAX cannot, which is the same split e80
arrived at. Not built.
