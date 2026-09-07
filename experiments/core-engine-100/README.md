# Ten core workloads × ten optimization approaches

Status: implementation and measurement in progress. Isolated experiments only;
no production path is changed. Base: `e8e7af0`.

Workloads, in execution order:
1. Filter and project a columnar scan.
2. Resolve latest versions and tombstones across overlapping segments.
3. Low-cardinality grouped counts and exact sums.
4. High-cardinality grouping followed by top-K.
5. Equality join followed by grouped aggregation.
6. Exact grouped COUNT(DISTINCT).
7. ORDER BY with LIMIT and deterministic ties.
8. Bounded rolling SUM and MIN windows.
9. IN/NOT IN membership with SQL NULL outcomes.
10. Correlated grouped aggregates with repeated outer keys.

Each has a separate reference and ten executable alternatives. These are
algorithm prototypes over invented typed fixtures, not ten production patches
or a replacement for the MySQL differential oracle. Candidate setup, index
construction, sorting, intermediate allocation, and result construction are
inside the timer. Fixture creation and full-result equality checks are outside.
No persistent index, precomputed aggregate, or cache gets free construction.

Three distributions: uniform, skewed, and key-clustered; nullable values and
negative exact integer amounts; independent data seeds. Complete ordered outputs
are compared, never just row counts or checksums. Four Rayon workers; Linux
processes are sequential, arms shuffled within each repetition. Process peak
RSS includes fixtures, reference, warmup and validation: it is not operator-only
memory and is not comparable to Pintail MemoryTracker reservations. Allocator is
Rust's system allocator, not the shipped jemalloc; this is a stated experiment
boundary. No production throughput or RSS claim follows from kernel results.

Build/test on the remote build host only:
```
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --manifest-path experiments/core-engine-100/Cargo.toml
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo clippy --manifest-path experiments/core-engine-100/Cargo.toml --all-targets -- -D warnings
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --release --manifest-path experiments/core-engine-100/Cargo.toml
```
The executable arguments are `CASE VARIANT ROWS SCENARIO SEED ROUNDS`; variant
zero is the reference and 1..10 are alternatives. It emits one JSON record.

## Required live-update contract — owner direction, 2026-09-08

**An optimization has no demonstrated product value until measured while the
source changes. Settled-only improvements do not qualify as breakthroughs.**

Every workload and every candidate must cover:
- interleaved query/commit cycles with inserts, updates to projected and predicate
  columns, updates to grouping/join keys, NULL transitions, and deletes;
- sparse and dense churn, uniform and hot-key distributions;
- live memtable tails, forced flushes producing overlapping immutable segments,
  and reads after compaction;
- stale and duplicate event replay; old pinned snapshots must remain unchanged;
- complete result comparison against the latest committed reference state;
- update/maintenance cost, query median/tail latency, total cycle cost and memory,
  including index/cache build, invalidation and repair cost;
- a real TableStore/WAL/memtable scan path and a live MySQL differential check
  before claiming an integrated engine win. Pure algorithm screens are preliminary
  evidence only and must never be reported as production acceleration.

Run settled controls alongside the changing-data workloads, but select winners
from changing-data evidence. Charge all maintained state to its lifecycle: moving
query work into ingestion, flush or compaction is not free. Document any stage
not reached and any unsupported shape rather than calling the matrix complete.

This requirement supersedes the original static-only timing plan above.
