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
3. Margins < 15% are ties — prefer the simpler implementation.
4. Results feed `docs/decisions.md` entries when adopted into `crates/pintail-*`.

## Pending (third wave)

- [ ] **e15-ovc** — offset-value coding in a loser-tree merge vs e12's packed-u128 winner.
