# Experiment lab TODO

Rule: **no approach gets adopted into the engine on reputation.** Each contested design
question gets a folder with competing implementations computing the identical answer
(checksum-verified), benchmarked on identical data on both reference machines. Winners
(and the margins) are recorded in `RESULTS.md` and drive the issue #3 implementation order.

## How to run

```bash
cd experiments
cargo run --release -p e01-filter-repr     # etc.
# All of them:
for e in e01-filter-repr e02-aggregation e03-topk e04-join e05-merge-on-read; do cargo run --release -p $e; done
# Remote docker host (Linux, CPU/memory-limited):
docker build -t pintail-experiments -f Dockerfile .
docker run --rm --cpus=8 --memory=8g pintail-experiments
```

Data: deterministic 20M-row orders table mirroring `benchmark/seed.sql`'s shape,
including the cyclic status column (defeats zone maps, as in the real benchmark).

## Implemented

- [x] **e01-filter-repr** — fused kernels vs byte mask vs bitmap words vs selection
  vector, count-only and sum-payload, selectivities 1/10/50/90% + dict predicate.
  (Photon position-lists vs ClickHouse byte-masks vs DaMoN 2021 hybrid.)
- [x] **e02-aggregation** — hash map vs direct-array dict-code accumulators (5 and 40
  groups); high-cardinality (200k): sequential map vs dense perfect-hash vs
  thread-local+merge vs shared relaxed atomics vs thread-local hashmaps+merge.
  (DuckDB/CH two-phase orthodoxy vs "Global Hash Tables Strike Back!" PVLDB 2025.)
- [x] **e03-topk** — full sort vs select_nth vs naive heap vs cutoff-guarded heap vs
  parallel per-chunk heaps + merge. (ClickHouse threshold prefilter / DuckDB boundary.)
- [x] **e04-join** — hashbrown vs Umbra-style unchained (offsets + bloom tags) vs dense
  direct-address; semi-join membership: HashSet vs dense bitmap vs blocked bloom.
- [x] **e05-merge-on-read** — the FINAL tax: fully-compacted floor vs naive 9-way heap
  merge vs sweep-classified 2-way merges vs scan+patch, at overlap 0.1/1/10%.

## Implemented (second wave, 2026-07-31 — see RESULTS.md)

- [x] **e06-decode** — FastLanes crate transposed bit-packing vs plain `Vec<i64>` scan
  vs lz4-block decode: is scanning compressed actually free on our targets?
- [x] **e07-strings** — German-string 16-byte views vs `Vec<String>` vs flat
  chars+offsets (CH ColumnString): filter, group-key hash, comparison workloads.
- [x] **e08-string-hash** — length-classed string hash tables (CH StringHashTable)
  vs generic hashbrown on `&str` group keys.
- [x] **e09-predicate-cache** — granule-bitmap condition cache hit path vs re-evaluating
  the filter (dashboard repeat-query shape).
- [x] **e10-parallel-scan** — morsel size sweep + core scaling curve (1..10 cores) for
  scan+filter+agg pipelines; measures whether tokio/rayon scheduling losses matter.
- [x] **e11-sweep-line** — granule-level (not segment-level) overlap classification on
  real PTSEG-shaped sparse indexes, including the level-0 memtable overlap case.
- [x] **e12-normalized-keys** — memcmp-able normalized composite sort keys + offset-value
  coding vs typed comparators for the k-way merge path.
- [x] **e27-adaptive-compression** — exact PTSEG payload layouts under always-LZ4,
  never-LZ4, and per-block keep-only-when-smaller policies. Global removal and
  encoding-class shortcuts both lose; Apple and Linux evidence supports the
  adopted PTSEG v3 per-block 5% selection rule.
- [x] **e28-fastlanes-real-scan** — A/B/A replacement of PTSEG's horizontal
  bitstream with FastLanes 1a inside the real 20M-row writer/reader path. The old
  0.14% estimate was stale; current gains are 7-10% for column scans and 4.5% after
  row materialization, still below the 15% format-change threshold.
- [x] **e29-metadata-demand** — grouped SMA sub-cubes and non-zone-map predicate
  block caches. Cubes win 52-65x in their fixed best case but cover none of the
  production-shaped workload; caches win 4.5-7.4x at 1-15% block coverage and
  lose when scattered matches cover every block.

## Decision criteria

1. Checksums must match across variants or the experiment is void.
2. A winner must win on **both** machines (Apple M2 Pro local; remote docker host with
   pinned CPU/memory limits) or the difference is treated as ISA-specific and both
   paths are kept behind the kernel-dispatch layer.
3. Performance margins < 15% are ties — prefer the simpler implementation and
   do not claim a throughput win. A separately stated storage invariant may
   choose between tied kernels, but its evidence and tradeoff must be recorded.
4. Results feed `docs/decisions.md` entries when adopted into `crates/pintail-*`.

## Pending (third wave)

- [ ] **e15-ovc** — offset-value coding in a loser-tree merge vs e12's packed-u128 winner.

## Specified (biomimetic research program)

- [ ] **e30-e79** — fifty non-duplicate adaptive, resilient, and biomimetic
  experiments specified in [`NEXT_50.md`](NEXT_50.md). Each item remains unchecked
  until its competing implementations have run, produced exact-answer evidence, and
  received a verdict in `RESULTS.md`.
  - [x] **e32-bone-index** — rejected: cumulative hotspot work improves by moving
    pivots out of the cold tail, but p95 seeks worsen 60% under the fixed byte budget.
  - [x] **e33-leaf-venation** — rejected: 96% fewer clustered metadata probes becomes
    only 10.9% elapsed and scattered data regresses.
  - [x] **e34-root-index** — rejected: corrected local/systemic feedback matches
    frequency allocation but causes 4.7-5.9x more rebuild work under drift.
  - [ ] **e43-lateral-predicates** — simulation passes on both targets (24.7% modeled
    work saved under blockwise drift); real unequal-cost typed kernel is required.
  - [ ] **e77-retinal-granules** — simulation passes on both targets (28% fewer rows
    touched on moving/stationary hotspots); real PTSEG A/B is required.
  - [x] **e38-fever-overload** — rejected: CDC is protected, but query p99 is
    20-157% worse and the result barely differs from a fixed conservative limit.
  - [x] **e46-quorum-compaction** — rejected: neighborhood reinforcement misses the
    exact painful interval and makes total work 4.9-48x worse.
  - [x] **e50-maintenance-ventilation** — rejected: no qualifying improvement over
    the simpler debt-only controller.
  - [x] **e55-stomatal-prefetch** — rejected: mixed pressure triggers 1,389
    reversals and 6.8x the offline-best cost.
  - [ ] **e59-endocrine-spill** — simulation passes on both targets (45-72% less
    modeled spill); real operator marginal-utility measurement is required.
  - [ ] **e63-glycogen-reserve** — simulation passes on both targets (zero protected
    spill and 15-32% lower makespan); real budget/spill integration is required.
  - [x] **e31-ant-join-paths** — rejected: evaporation recovers much more slowly than
    discounted UCB and misses the shift and worst-regret gates.
  - [ ] **e35-clonal-kernels** — simulation passes on both targets (1.7-1.8% from the
    contextual oracle and 29-35% below one global winner); real dispatch is required.
  - [x] **e37-immune-plan-memory** — rejected: affinity matching loses to simpler fixed
    parameter buckets in every workload.
  - [x] **e40-hippocampal-replay** — rejected: weak-trace replay improves frequency
    replay, but misses both preregistered 20% gates in every workload.
  - [x] **e45-cardinality-homeostasis** — rejected: bounded homeostatic estimates are
    less accurate and costlier than a simple EWMA.
  - [ ] **e57-atp-plans** — conditional simulation passes on both targets (100% fastest
    feasible, zero violations); real calibration accuracy and overhead are required.
  - [x] **e68-biodiversity-reserve** — rejected: rapid reversal detection does not
    offset exploration tax against periodic probing in the single-reversal trace.
  - [x] **e79-echolocation-plans** — rejected: triggered probes are safe but improve
    uncertain plans by only 13-22%, below the 25% gate.
  - [x] **e41-synaptic-cache** — rejected: graph reinforcement loses to
    GreedyDual-size while doing more bookkeeping.
  - [ ] **e48-flocking-reads** — simulation passes on both targets (94-97% fewer
    overlapping reads and 29% lower modeled median); real cold-file trial required.
  - [ ] **e51-mycelial-blocks** — simulation passes on both targets (27% less decode
    work and 17% lower p95 than LRU); real decoded PTSEG exchange required.
  - [x] **e65-predator-cache** — rejected: it fails to beat ARC by 15% consistently
    and the modeled class populations oscillate beyond the gate.
  - [x] **e66-cache-niches** — rejected: adaptive borders range from -8% to +5.5%
    against global GreedyDual, far below the 20% gate.
  - [x] **e70-forest-gap-auction** — re-executed with full equal budgets, exact marginal
    spill bids, and per-epoch cap assertions; rejected because auctions increase makespan
    6.1-13.1% versus equal redistribution and exceed the 0.5% overhead gate.
  - [x] **e71-dormant-indexes** — rejected: multi-cue germination saves memory and
    rebuild work but leaves seasonal p95 on the unindexed path.
  - [x] **e72-seasonal-columns** — re-executed with 24 distinct `u32` column bits,
    oracle-identical answers, and random/wide anti-churn regressions; rejected because
    corrected periodic and drifting gains are 18.4% and 19.4%, below the 20% gate.
  - [ ] **e73-symbiotic-intermediates** — simulation passes on both targets at one
    shared 12-entry cap; real immutable/versioned intermediate trial required.
  - [x] **e74-fire-cache-reset** — rejected: LRU recovers before the prolonged-low-hit
    detector fires, so controlled reset provides no gain.
  - [ ] **e30-physarum-access** — simulation passes on both targets; real cold PTSEG
    bundle/rewrite measurements required.
  - [x] **e36-negative-selection** — re-executed without label leakage: negative selection
    reaches 100% recall and zero false quarantine but exactly ties diagonal distance, so the
    added ensemble has no qualifying value.
  - [ ] **e39-granule-quarantine** — re-audited byte-path kernel passes on both targets: three
    injected corrupt granules are detected, all overlap fails, every disjoint answer is
    oracle-exact, and verified bytes fall 97.3%; real PTSEG persistence remains.
  - [x] **e42-predictive-columns** — rejected: only one shape shrinks (11.8%) and decode
    is slower, missing both correlated-shape gates.
  - [ ] **e53-coral-views** — conditional simulation passes; real lineage-aware aggregate
    partials and invalidation costs required.
  - [x] **e54-ant-tombstones** — rejected: evaporation increases moving/scattered read
    work and fails the write-amplification margin.
  - [x] **e67-segment-succession** — rejected: heat-only tiers dominate and the state
    machine makes thousands of needless transitions.
  - [x] **e75-parity-regeneration** — rejected: XOR repairs exactly within 8% storage,
    but a small stripe reads more bytes than the modeled segment restore.
  - [x] **e76-hierarchical-reconcile** — re-executed with persisted indexes and timed
    reconciliation calls; rejected because the exact tree path is about 9.9% slower than
    flat checksums at 33% drift, failing the dense-control gate.
  - [x] **e78-receptor-blooms** — re-executed across all declared query classes with
    independent routing/hash mixing, equal bits/probes, and addressability regressions;
    rejected because the ensemble has 3.16x the false reads of learned tuple allocation.
  - [x] **e44-foveated-topk** — re-executed over actual ordered Top-K and payload pages;
    rejected because the uncorrelated safe fallback is 11.8% slower by local median,
    despite 56-88% fewer payload bytes when proof-gated fine pages activate.
  - [x] **e47-waggle-morsels** — re-executed over observable morsel yield with exact
    results; rejected because completion changes by less than 1%, missing the 15% gate.
  - [x] **e49-schooling-control** — re-executed as a 16-slot scheduler with completed-query
    timelines; rejected because SJF p95 slowdown is 4.4-5.5x lower than schooling.
  - [ ] **e52-join-fibers** — re-executed on both targets over real FK arrays with exact
    join checksums and an actually executed build scan; the prototype clears all numeric
    gates, pending real PTSEG lineage/invalidation costs.
  - [x] **e56-vascular-memory** — re-executed with cap/floor assertions and observed
    utility updates; rejected because it ties, rather than beats, static weights when stable.
  - [x] **e58-enzyme-batches** — re-executed with charged curve probes and exact chunked
    checksums; rejected because saturation needs 6-7 probes and cannot repay on the
    unchanged filter control.
  - [x] **e60-autophagic-buffers** — re-executed with real zeroed byte buffers and cap
    accounting; rejected because churn slack is 13.9% and fragmentation grows 360 KiB.
  - [ ] **e61-apoptotic-queries** — re-executed on both targets with label-free decisions,
    actual early-stop/false-abort accounting, exact preserved answers, and computed healthy
    p99; all prototype gates pass pending real-telemetry calibration.
  - [x] **e62-operator-fission** — re-executed with exact global/sharded aggregation and
    charged transitions; rejected at 3.8% shift improvement and +25% tiny-work cost.
  - [x] **e64-circadian-maintenance** — re-executed with bounded learned history and a
    full-slack reactive control; rejected because forecasting ties or loses to reaction.
  - [ ] **e69-invasive-defense** — **INVALIDATED:** shared Wave 6 code prints literals.
