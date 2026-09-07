# Update-aware core engine experiments

The requested 100 alternatives have been implemented and screened against real changing
TableStore snapshots. This completes an **algorithm screen**, not 100 installed engine
optimizations. No production acceleration or breakthrough is claimed.

## Results selected from changing data

Ratios are against the lab reference, not the current Pintail SQL executor. A ratio above
1 is faster. Each query includes the projected storage scan, conversion into the lab row
format, candidate construction and result production. The cycle also waits for the
concurrent writer/flush/compaction. All maintained structures are rebuilt and charged;
there is no free persistent index or cache.

| Workload | Selected approach | Screen query | Holdout query | Holdout cycle | Worst holdout distribution | Assessment |
|---|---|---:|---:|---:|---:|---|
| 1. Scan/filter/project | 1: selective-predicate-first | 1.02× | 1.00× | 1.01× | 0.98× | weak or distribution-dependent |
| 2. Version/tombstone resolution | 3: sorted-two-way | 1.09× | 1.12× | 1.04× | 1.08× | weak or distribution-dependent |
| 3. Low-cardinality grouping | 6: parallel-local-hash | 1.05× | 0.99× | 1.02× | 0.96× | weak or distribution-dependent |
| 4. High-cardinality grouping/top-K | 2: dense-heap | 1.10× | 1.05× | 0.98× | 1.00× | weak or distribution-dependent |
| 5. Join/group aggregation | 7: parallel-dense-probe | 1.07× | 1.01× | 1.07× | 1.01× | weak or distribution-dependent |
| 6. Exact grouped distinct | 5: parallel-local-bitmaps | 1.09× | 1.05× | 1.03× | 0.98× | weak or distribution-dependent |
| 7. Ordered LIMIT | 2: quickselect-prefix | 1.08× | 1.03× | 1.01× | 1.03× | weak or distribution-dependent |
| 8. Bounded rolling windows | 7: parallel-halo-windows | 1.11× | 1.11× | 1.05× | 1.07× | weak or distribution-dependent |
| 9. Nullable IN/NOT IN | 3: dense-membership-bitmap | 1.05× | 1.00× | 0.95× | 0.99× | weak or distribution-dependent |
| 10. Correlated aggregates | 10: demand-bitmap-dense-fold | 1.19× | 1.14× | 1.05× | 1.12× | candidate for integration |

The screen uses three seeds in each of three distributions at 100,000 invented rows.
The selected alternatives were fixed before confirmation, which uses one fresh seed
per distribution at 200,000 rows. Three holdout samples per arm are a scale/distribution
check, not a confidence interval or evidence for a production tail-latency SLO.

## Distribution-specific confirmations

An overall median can hide an improvement limited to hot-key data. Before these
extra runs, candidates were selected independently per distribution when the screen
query ratio was at least 1.20 and the cycle ratio at least 1.05. Each selected arm
was then compared with its control at 200,000 rows using three additional fresh seeds.
These ratios still compare external prototypes with lab controls, not installed SQL.

| Workload | Distribution | Approach | Screen query | Fresh query | Fresh cycle | Minimum fresh process query |
|---|---|---|---:|---:|---:|---:|
| Join/group aggregation | hot-key skew | factorized-fact-aggregate | 2.00× | 2.06× | 1.44× | 1.69× |
| Bounded rolling windows | hot-key skew | block-min-prefix | 1.21× | 1.15× | 1.07× | 1.06× |
| Correlated aggregates | hot-key skew | dense-decorrelation | 1.24× | 1.22× | 1.06× | 1.18× |

These add 18 processes and 144 checked snapshots. Selection is recorded in [regime-selection.json](regime-selection.json).

## Coverage and evidence

- 10 workloads × 10 alternatives, plus 10 separate controls. [Full inventory](APPROACHES.md).
- Screen: 990 sequential processes and 7,920 measured/validated snapshots.
- Confirmation: 60 processes and 480 measured/validated snapshots, selected before holdout.
- Every process additionally checks the old pinned view after a concurrent write, the
  newly committed view and restart/reopen equality. Compaction must actually merge inputs.
- Each trajectory covers inserts, NULL transitions, predicate/value and grouping/join-key
  updates, deletes, duplicate/stale versions before tombstone retirement, sparse/dense
  tails, overlapping flushed segments and compaction.
- [All 100 outcomes](evidence/RESULTS.md), [raw screen records](evidence/raw.jsonl),
  [source and binary provenance](evidence/provenance.json), [selection rule](confirmation-selection.json).

## Cost of changing storage

These are medians across the ten lab controls, three distributions and three seeds.
The scan figure includes storage decoding and the lab materialization adapter, so it
must not be presented as a measurement of storage decoding alone.

| Snapshot state | Scan + adapter ms | Query ms | Concurrent writer/maintenance ms | Full cycle ms |
|---|---:|---:|---:|---:|
| settled | 2.23 | 5.69 | 0.75 | 5.83 |
| sparse-memtable | 36.47 | 40.09 | 1.23 | 40.19 |
| sparse-flushed-overlap | 39.35 | 42.64 | 16.03 | 42.78 |
| dense-hot-memtable | 38.24 | 41.92 | 21.52 | 42.08 |
| dense-flushed-overlap | 39.29 | 42.90 | 1.61 | 43.02 |
| mixed-overlap | 40.80 | 44.24 | 0.04 | 44.37 |
| stale-replay | 40.80 | 44.53 | 350.65 | 351.07 |
| compacted | 2.55 | 5.53 | 0.00 | 5.64 |

Actual writer/query overlap was nonzero in 5,647 of 5,940 changing-state readings. The raw records include overlap duration; a short writer is not represented as sustained load.

## Actual engine and MySQL checks

`engine-evidence/` contains the ten equivalent SQL queries through the real parser,
binder, optimizer and executor at all eight states and three distributions, with
settled-result memoization disabled. SQL anchors run between mutation batches, not
concurrently with their writer. Those timings include SQL planning and textual
result rendering; they are contextual baselines, not paired speedup comparisons with
the external kernels. `oracle-evidence/result.json` records the isolated MySQL 8.4
transactional comparison (240 exact checks); the factorized spelling has a separate
24-check transactional comparison in `oracle-factorized-verified/`. Native binlog delivery and source-to-query lag are not covered
by direct decoded ingestion, and no integrated optimization is certified by this oracle.

### Resource-limited SQL outcomes

At 100,000 invented rows and a fixed 256 MiB query cap, 200 of 240 SQL answers were exact and 40 executions refused with a resource error. A refusal is not counted as a successful timing.

| SQL workload | Exact answers / 24 | Resource refusals | Median successful query ms |
|---|---:|---:|---:|
| Scan/filter/project | 24 | 0 | 33.24 |
| Version/tombstone resolution | 24 | 0 | 59.31 |
| Low-cardinality grouping | 24 | 0 | 29.34 |
| High-cardinality grouping/top-K | 24 | 0 | 32.66 |
| Join/group aggregation | 8 | 16 | 290.80 |
| Exact grouped distinct | 24 | 0 | 29.53 |
| Ordered LIMIT | 24 | 0 | 29.40 |
| Bounded rolling windows | 24 | 0 | 231.98 |
| Nullable IN/NOT IN | 0 | 24 | — |
| Correlated aggregates | 24 | 0 | 232.48 |

An additional native SQL experiment filters the dimension in a derived table before
the equality join, preserving duplicates, grouping, NULL handling and the expected
answer. It uses the same memory cap and changing fixtures; it does not change the
production optimizer or install the factorized prototype.

That spelling produced 24 exact answers and 0 resource refusals across 24 states. The unmodified query and any refusals remain in the evidence.

### Factorization through the real SQL executor

The selected join mechanism was also expressed as SQL: aggregate the fact input by
join key, then join the filtered dimension and sum the partial counts/sums. All 24
states returned exact answers at the same 256 MiB cap. This is a query-shape experiment
in the actual engine, not an installed optimizer rule.

| Distribution | Prefiltered join median dirty ms | Factorized join median dirty ms | Total dirty query ratio |
|---|---:|---:|---:|
| uniform | 76.79 | 82.40 | 0.94× |
| hot-key skew | 144.22 | 80.33 | 2.43× |
| key-clustered | 72.88 | 77.82 | 0.93× |

These are single-seed, sequential SQL anchor timings between mutation batches. The
concurrent writer/cycle evidence belongs to the independently confirmed external
prototype above. The uniform and clustered regressions rule out blanket adoption.
A production rule must establish type/NULL/duplicate semantics, reserve memory and
choose based on expected fanout; none of those integration claims is made here.

## Unresolved correctness boundary

[F1](FINDINGS.md) preserves a minimal reproducer: an ancient version submitted directly
to the store after full compaction has retired a deletion marker can resurrect the key.
Native CDC reachability has not been established. The performance matrix excludes that
invalid state and never counts it as a passing replay test.

## Interpretation limits

These are finite batches with one reader and one writer, not a sustained update-rate
soak, multi-client admission test or cross-table transaction test. Join inputs are mutable
views of one table. Integer domains are bounded; dense strategies need guarded fallbacks.
String collations, DECIMAL, ENUM, schema evolution and spill budgets need integration
coverage. Peak RSS includes fixture, independent model and correctness checks; it is not
candidate-only memory. The system allocator differs from the shipped allocator.
Automatic background compaction is disabled for repeatable phase boundaries; explicit
compaction races the query on the writer thread. Cycle time is finite-batch service
cost, not sustainable update throughput or native CDC lag.
Reference computation precedes each timed query and can warm data/cache state.
Every kernel receives a full four-column scan. These measurements do not test physical
predicate pushdown, operator/scan fusion, a maintained persistent index or incremental
aggregate repair. The common adapter can hide gains that require such integration.

Measurements ran as native processes in an isolated checkout on an otherwise idle build
host, with no Docker benchmark on that host: CPU model, toolchain, affinity and hashes
are in provenance. A deployment address is intentionally not part of public evidence.
