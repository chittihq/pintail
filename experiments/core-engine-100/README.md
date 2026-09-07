# Ten core workloads × ten optimization approaches

Status: the 100-approach changing-data screen and independent confirmations are
complete. [Results and resource failures](RESULTS.md) include native SQL follow-ups
and transactional MySQL checks. No production engine path is changed. The measured
engine base is `51665c5`; each evidence directory records its experiment commit.

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
The optional static kernel executable accepts `CASE VARIANT ROWS SCENARIO SEED
ROUNDS`; it is a development aid, not the update-aware evidence path. The measured
`live` binary accepts `CASE VARIANT ROWS SCENARIO SEED`, and `run.py` drives it.
Variant zero is the reference and 1..10 are alternatives.

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

## Implemented measurement path

`run.py` runs use cases sequentially, with 11 arms (reference + 10 alternatives),
three distributions and three independent seeds. Each process creates its own real
TableStore and an independent key/version model. A pinned query races one fixed
writer batch or maintenance operation; the barrier and actual overlap duration
are recorded, so a faster writer is not assumed to have overlapped a whole query.
The eight observed states are settled, sparse memtable, sparse flushed overlap,
dense hot memtable, dense flushed overlap, mixed overlap, stale replay, compacted.
The writer applies inserts, deletes, key/group changes and NULL transitions,
synchronizes the WAL, and flushes/compacts at specified boundaries. A restart is
verified after the last phase. Maintenance is timed even when queries finish first.

The prototype consumes a real projected storage scan, with the store's documented
materialized fallback where streaming is unavailable. It materializes those
columns into the lab's typed row fixture before running the alternative. This
adapter cost is included, reported separately, and shared by every arm. These
are **external operator prototypes**, not production SQL-operator replacements.
Case 2 additionally resolves the actual version history; its input coalescing and
cloning are charged, and its full output is checked against the committed model.
The other nine cases consume the already resolved current snapshot. The join's
small side is a mutable subset of the same source table, so mutations can change
both join inputs and duplicate multiplicity.

The source base was advanced to `51665c5` before timed runs to include the newly
landed overlapping-pair compaction fix. Builds and measurements use an isolated
checkout; `provenance.json` records actual source/binary hashes and the measurement
commit. The experiment lockfile is independent of the release lockfile.

Run the full matrix, then summarize (on the build server):
```
python3 experiments/core-engine-100/run.py --rows 100000 --repeats 3
python3 experiments/core-engine-100/summarize.py
```
`--out` selects a new evidence directory. `--resume` refuses changed source or
binaries. Partial runs retain raw records but never print CORE-100-DONE.

`anchors` executes all ten SQL shapes through the actual parser, binder, optimizer,
executor and TableStore at every mutation state. `oracle.py` checks the same
answers against an isolated MySQL 8.4 container, applying real source transactions
between queries (no table reload between mutation phases). Set DOCKER_HOST in the
invoking environment; no deployment address belongs in this repository. It removes
only the uniquely named container it created. This verifies SQL and committed
state semantics; it does **not** exercise native binlog transport or certify an
integrated optimization. No prototype is promoted on these numbers alone.

```
python3 experiments/core-engine-100/oracle.py --rows 1000
```

Remaining scope boundaries: finite concurrent batches, not a duration-based CDC
soak; integer join/group keys and nullable integer amounts, not the full collation,
ENUM, DECIMAL, timezone or schema-evolution matrix; one analytical reader plus one
writer, not a multi-client fairness test; system allocator; no per-query tracked
memory budget in the external algorithms. RSS is whole-process high-water usage.
Dense strategies rely on bounded fixture domains and need guarded fallbacks before
integration. The explicit post-retirement replay failure remains in FINDINGS.md.


## Reproduce the follow-up checks

After building the binaries on the build host:
```
python3 experiments/core-engine-100/confirm.py
python3 experiments/core-engine-100/engine.py
python3 experiments/core-engine-100/engine.py --prefilter-join --out engine-prefilter-evidence
python3 experiments/core-engine-100/engine.py --factorized-join --out engine-factorized-evidence
PINTAIL_DISABLE_SETTLED_MEMO=1 python3 experiments/core-engine-100/oracle.py --rows 1000
PINTAIL_DISABLE_SETTLED_MEMO=1 PINTAIL_LAB_JOIN_FACTORIZED=1 python3 experiments/core-engine-100/oracle.py --rows 1000 --cases 5 --out oracle-factorized-verified
python3 experiments/core-engine-100/report.py experiments/core-engine-100
```
Use a fresh checkout/output location for new measurements; confirmation refuses to
overwrite existing raw records. `engine.py` explicitly records resource refusals
as `correct: false` and reports counts, never a passing gate. The fixed SQL memory
ceiling is 256 MiB. SQL anchors run between commits; the prototype matrix races its
writer. Automatic background compaction is disabled in the controlled fixture,
and explicit compaction runs on the writer thread. Cycle time is finite-batch cost,
not sustained update throughput or source-to-query CDC lag.

The distribution-specific confirmations are selected in `regime-selection.json`:
for each selection, run `run.py` with its case/scenario and variants `0,<variant>`,
`--rows 200000 --repeats 3 --seed-base 17011 --out regime-<case>-<scenario>`.
The complete scan includes four columns before external kernels execute; physical
pushdown/fusion and maintained indexes require separate integrated experiments.
Oracle SQL/TSV exports remain local and reproducible; committed manifests retain
their SHA-256 hashes alongside the query records and differential outcomes.
