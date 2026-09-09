# Storage scan comparison: 20 million synthetic rows

> Historical measurements before the qualification rebase. See
> [the rebased qualification](storage-scan-qualification.md) for the adversarial
> fixture, complete rc gate, and controlled performance comparison.

The same block-directory and predicate-buffer changes tested in
[the smaller experiment](storage-scan-addressing.md) retain their storage-work
benefit at 20 million rows. Narrow scans and text-only filtered scans improve
substantially. Full SQL filter/count queries improve less, and the wide
arithmetic aggregate shows no material improvement.

## Fixture and method

- Exactly **20,000,000 rows**, with keys 0 through 19,999,999.
- **24 columns**: 23 unsigned integer columns generated as `id * column + 7`,
  and one text column cycling through eight labels. No nulls in this fixture.
- **20 immutable segments**, seeded in batches of at most 1,048,576 rows.
  The last segment contains 77,056 rows.
- **153,815,892 segment bytes (146.69 MiB)** on the build machine's filesystem,
  outside its memory-backed temporary directory. This fixture is deliberately
  reproducible and highly compressible; it is not a large low-compression or
  disk-throughput workload.
- Warm OS page cache. Both binaries read the same persisted files; seeding
  occurs in a separate process and is excluded from query measurements.
- Identical probe sources from `70c6cddf` for both binaries. Baseline engine:
  `40486955`; candidate engine: `70c6cddf` (the runtime changes are `2b8175a8`
  and `d421aba9`). Release builds, rustc/cargo 1.97.0, Linux x86_64.
- Three process pairs, ordered baseline/candidate, candidate/baseline,
  baseline/candidate. Each process performs two warmups and three measured
  iterations per workload. Reported figures are medians of process medians.
- The storage probe drains one chunk at a time; the SQL probe uses normal
  executor parallelism and includes parse, bind, planning and execution.
  It excludes network transport and result serialization.
- SQL runs set `PINTAIL_DISABLE_SETTLED_MEMO=1`, verify every scalar result
  against an independently computed answer, and assert physical block decode
  on every iteration. Storage runs verify every output value in the first,
  unmeasured warmup and verify output counts in every iteration.
- Filtered storage cases keep 256 rows per 4,096-row interval. The final
  partial interval means **1,250,048 retained rows**, rather than rounding
  the answer to exactly one sixteenth of 20 million. Matches touch every
  block, so the text-only improvement does not come from skipping payload
  blocks.

The run waited for an existing validation process to exit. New compilation
and another local benchmark then started during the first timing round.
**These are shared-server observations, not isolated-server latency claims.**
The adjacent JSON retains all process medians, minima and memory observations.
No compilation or extra validation was initiated by this benchmark during its
measurement phase.

## Results

| Workload | Baseline ms | Candidate ms | Observed ratio |
|---|---:|---:|---:|
| Narrow projected scan | 70.601 | 33.789 | 2.09x |
| Wide projected scan | 1462.554 | 1414.507 | 1.03x |
| Text predicate, all rows retained | 112.585 | 18.668 | 6.03x |
| Text predicate, partial selection | 130.339 | 18.817 | 6.93x |
| Mixed projection, partial selection | 192.578 | 126.567 | 1.52x |
| SQL numeric filter + count | 32.799 | 17.946 | 1.83x |
| SQL text equality + count | 27.970 | 21.713 | 1.29x |
| SQL text inequality + count | 38.991 | 32.628 | 1.20x |
| SQL wide arithmetic + sum | 18078.308 | 18285.956 | 0.99x |

The numeric-filter SQL process medians span 31.260–33.121 ms for baseline
and 16.393–27.461 ms for candidate. Their best measured iterations are
27.949 and 16.246 ms respectively. The direction is consistent across the
three process pairs, but the magnitude varies with shared-server conditions.

Wide projected scans overlap substantially between variants, as do the wide
arithmetic queries. The latter's best iterations are 17709.095 ms for baseline
and 17529.638 ms for candidate. Neither establishes a reliable wide-workload
speedup or regression.

The exact storage-work result is independent of timing noise:

| Storage case | Baseline decoded blocks | Candidate decoded blocks |
|---|---:|---:|
| Narrow scan | 1221 | 1221 |
| Wide scan | 29304 | 29304 |
| Text-only predicate cases | 2442 | 1221 |
| Mixed projection | 3663 | 3663 |

Predicate-buffer reuse halves text-only block decoding. Mixed projections
still decode the predicate column again as part of their output projection;
their improvement comes from block addressing. Larger isolated scan ratios
must not be presented as full SQL query speedups.

## Resource use and correctness

Seeding took 33.54 seconds and peaked at 1,019,040 KiB RSS (995.2 MiB), with
no swaps reported for that process. The maximum observed scan-process RSS
was 28,524 KiB (27.9 MiB); maximum query-process RSS was 424,344 KiB
(414.4 MiB). These are process RSS measurements, not the engine's query
memory-accounting counters, and exclude the OS file cache.

All six scan processes and all six SQL processes completed their checks.
Across both engines the SQL answers were exactly:

- Numeric-filter count: 19,956,522.
- Text-equality count: 2,500,000.
- Text-inequality count: 20,000,000.
- Wide arithmetic sum: 55,200,000,460,000,000.

The complete development profile passed on clean commit `70c6cddf`, run
`2026-09-09T17-20-08-299Z-development`: formatting/workspace clippy, dashboard
typechecking, **1,033 unit/integration tests passed (43 skipped)**, and the
parser corpus. Profiling-only environment switches were removed before
validation so tests exercised their normal memoization behavior. This is a
complete development gate, not an rc/stable release gate.

## Reproduce

Build the same examples for each engine revision as described in the smaller
experiment. Keep baseline and candidate executables separately, then seed
once on a filesystem with sufficient space:

```sh
./target/release/examples/storage_scan_probe target/storage-probe-20m 20000000 --seed-only
PINTAIL_PROBE_ITERATIONS=3 \
  ./target/release/examples/storage_scan_probe target/storage-probe-20m 20000000
PINTAIL_DISABLE_SETTLED_MEMO=1 PINTAIL_PROBE_ITERATIONS=3 \
  ./target/release/examples/storage_query_probe target/storage-probe-20m 20000000
```

Alternate fresh baseline and candidate processes over the same directory.
Use a quiet machine for an isolated latency comparison. Reproducing this
fixture verifies row-processing scale under strong compression; it does not
establish cold-cache I/O, update-heavy performance, or gains on unrelated
query shapes.
