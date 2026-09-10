# Storage scan qualification

The candidate is rebased onto `b019dae2`. Engine and probe revision:
`26f4c9f2cca79d137b20a3e54072a3550be46234`.

This change caches immutable block directories and reuses already decoded
predicate columns when the output projection is identical. It changes no
on-disk format. Its benefit depends on the scan shape; wide arithmetic is
not expected to become faster from avoiding a small amount of storage work.

## Skip condition

The directory records each block's half-open physical row interval, derived
from the on-disk block row counts and checked against the segment row count.
The reader validates ordered, non-overlapping requested ranges. It advances
past ranges ending before a block, then decodes the block exactly when the
next range intersects it. If that range starts at or beyond the block end,
all later ranges start still farther along and cannot intersect either.
Predicate evaluation supplies the selected ranges before projected blocks
are considered. The adversarial comparison below tests that complete path,
including ranges across scan boundaries and partial blocks.

## Row preservation: 20 million adversarial rows

The same fixture was seeded by the baseline and opened by both executables.
Both checked every returned value and its order against independently generated
expected rows, including a check for missing trailing rows. The four complete
answer files also passed `cmp`; their SHA-256 digests are in the accompanying JSON.

The fixture has 39 segments (1,509,148,456 bytes). Each segment contains at most
524,288 rows. Matching text is clustered in one block per 32 blocks, with extra
matches at scan boundaries. The predicate has an all-null block per 32 blocks
and scattered nulls every 97 rows. Projected binary and variable-width text
columns have nulls every 13 and 17 rows respectively. Each non-null binary value
contains 64 deterministically generated opaque bytes, avoiding the earlier
fixture's uniformly repetitive projected data. The final segment is partial.

| Selection / projection | Returned rows | Baseline decoded | Candidate decoded | Baseline pruned | Candidate pruned |
|---|---:|---:|---:|---:|---:|
| Clustered text, predicate-only output | 632,501 | 1,298 | 1,221 | 8,440 | 3,648 |
| Clustered text, reordered mixed output | 632,501 | 1,529 | 1,529 | 22,816 | 22,816 |
| Null predicate, reordered mixed output | 822,359 | 6,105 | 6,105 | 18,240 | 18,240 |
| No matches | 0 | 1,221 | 1,221 | 23,124 | 23,124 |

For the clustered mixed projection, 1,529 decodes comprise all 1,221
predicate blocks plus 77 blocks for each of the four output columns. Thus
1,144 of 1,221 blocks per output column are omitted while every expected
matching row is preserved. The null predicate touches every block because
of scattered nulls and decodes 6,105 blocks with the same output projection.
This contrast distinguishes real selected-range skipping from merely leaving
unrequested columns unread.

These are the scanner's cumulative physical block counters, not unique block
identities. Pruned counts include exclusions from repeated sliced reads; they
must not be interpreted as a percentage of the fixture's blocks. Reusing a
predicate-only fetch also eliminates the second traversal and its skip counters.
The sparse case avoids only 77 repeated decodes, rather than halving all decodes:
the baseline already restricts its second read to selected ranges. The mixed
projection genuinely skips most projected blocks and returns identical rows.

The regular store integration suite includes the same adversarial probe at
262,161 rows. The existing physical-representation regression also covers
signed/unsigned integers, floats, dictionary/variable text, dates, booleans,
binary values, nulls, empty selections, and partial scan slices.

## Reproduction

Build `storage_adversarial_probe` in the store crate for the baseline and
candidate. The example is new, so copy the same example source into a detached
baseline checkout before building it. Keep the resulting executables separate.
Run on the build server, using the repository's normal Cargo environment:

```sh
baseline-probe DATA 20000000 baseline-answers
candidate-probe DATA 20000000 candidate-answers
for n in 0 1 2 3; do
  cmp "baseline-answers/case-$n.txt" "candidate-answers/case-$n.txt"
done
```

No timing claim is taken from this correctness run. The performance comparison
uses a separate idle measurement machine, with baseline and candidate run
sequentially. Its private identity is represented by the benchmark's host
fingerprint in committed evidence.

## Rebased rc gate

The complete `--profile rc` passed at `26f4c9f2`: all ten stages green,
1,231 oracle queries byte-exact against MySQL 8.4, and 1,034 unit tests
passed (43 skipped). Each MySQL 8.4/8.0 E2E run passed 5,754 checks, with
44 skips and 30 warnings. Warning categories match the baseline. See
[the banked rc report](storage-scan-rc.md), including the initial missing
Prisma-client setup failure and the subsequent full successful rerun.

## Canonical eight-query benchmark

Both runs use 20 million synthetic rows, the same measurement machine,
8 CPUs and 8 GiB per database container, a 4 GiB Pintail query ceiling,
and two warmups followed by 15 measured iterations. The runner also executes
novel queries and concurrency checks. Both gates passed with exact answers.
MySQL's existing same-host reference timings were reused; no newly measured
MySQL speedup is claimed here.

The first baseline attempt was discarded when a separate native benchmark
started during timed queries. Only this harness's containers and temporary
volumes were removed. The replacement baseline waited for that process to
finish and for a quiet interval. Process/container samples every 15 seconds
were retained alongside the successful runs; no competing workload was
observed during their timing sections.

The table uses raw samples from the memo-disabled engine track:

| Query | Baseline median ms | Candidate median ms | Baseline min ms | Candidate min ms | Median speedup |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 4.38 | 4.45 | 4.23 | 4.24 | 0.98× |
| Q2: Filtered count | 51.04 | 47.35 | 48.76 | 46.63 | 1.08× |
| Q3: Group by status | 131.70 | 126.09 | 125.78 | 120.29 | 1.04× |
| Q4: Region × status breakdown | 148.43 | 157.06 | 145.41 | 143.88 | 0.95× |
| Q5: Monthly revenue (2023) | 101.86 | 100.72 | 98.03 | 94.81 | 1.01× |
| Q6: Top 10 spenders | 444.53 | 439.91 | 436.73 | 433.75 | 1.01× |
| Q7: Regional analytics | 395.92 | 409.74 | 373.07 | 383.46 | 0.97× |
| Q8: Join users + orders | 377.29 | 363.32 | 333.68 | 333.75 | 1.04× |

The largest median slowdown is 5.8% (Q4), where the minimum improves.
The largest minimum slowdown is 2.8% (Q7). Those changes are within the
repository's documented ±10% measurement-noise allowance. These measurements
do not establish a substantial regression or a general speedup. The memo-on
track's medians change by −3.7% to +5.6% in latency; its raw samples are banked too.

The harness's automatic previous-run regression check requires identical
source fingerprints, so it deliberately does not compare these revisions.
The assessment above compares their raw samples explicitly. Resource limits
and host fingerprint match; source fingerprints differ by design.

## Uniform 20-million-row probes, repeated on the idle machine

The existing fixture is identical for both executables. Each probe uses two
warmups and seven measured iterations; SQL settled-result memoization is
disabled. Every expected result is checked. This is warm-cache evidence.

| Probe | Case | Baseline median ms | Candidate median ms | Speedup | Decoded blocks, baseline → candidate |
|---|---|---:|---:|---:|---:|
| scan | narrow-last | 115.488 | 58.428 | 1.98× | 1,221 → 1,221 |
| scan | wide | 2226.787 | 2128.410 | 1.05× | 29,304 → 29,304 |
| scan | text-all | 172.152 | 35.543 | 4.84× | 2,442 → 1,221 |
| scan | text-selective | 202.583 | 35.751 | 5.67× | 2,442 → 1,221 |
| scan | mixed-selective | 325.317 | 221.423 | 1.47× | 3,663 → 3,663 |
| query | numeric-filter | 45.555 | 30.184 | 1.51× | — |
| query | text-filter | 49.859 | 38.709 | 1.29× | — |
| query | text-all | 56.485 | 44.747 | 1.26× | — |
| query | wide-expression | 27818.334 | 27331.020 | 1.02× | — |

The disputed wide shapes remain modest: projected scanning is 1.046× and
wide arithmetic is 1.018×. Neither supports a broad query-speedup claim.
The scan-bound SQL cases improve 1.26–1.51×. Dense text-only storage scans
halve decoding, while sparse selection avoids fewer repeated decodes as
shown in the adversarial fixture. Historical measurements remain available
but are superseded by this controlled comparison.

## TPC-H SF1

Three alternating passes per binary loaded fresh replicas, each with
8,660,779 rows across eight tables (6,000,749 line items). Each of the four
supported queries ran once per replica, in fixed order, with settled-result
memoization disabled. All **24 comparisons were byte-exact against MySQL**.
This is the repository's four-query workload, not the complete 22-query suite.

| Query | Baseline median ms | Candidate median ms | Speedup | Baseline range ms | Candidate range ms |
|---|---:|---:|---:|---:|---:|
| q01-pricing-summary | 9,252 | 9,219 | 1.004× | 9,136–9,372 | 9,139–9,301 |
| q03-shipping-priority | 5,395 | 5,421 | 0.995× | 5,374–5,452 | 5,418–5,422 |
| q05-local-supplier-volume | 47,791 | 47,793 | 1.000× | 47,737–48,060 | 47,666–47,892 |
| q10-returned-item-reporting | 3,985 | 4,014 | 0.993× | 3,951–4,009 | 3,952–4,162 |

Median latency changes range from 0.36% faster to 0.73% slower. These results
show no material TPC-H regression or gain in the tested shapes.

**Capacity qualification:** the unchanged baseline failed Q05 with the default
1 GiB query spill quota. That [failed result](storage-scan-qualification/tpch-baseline-default-quota.json)
is retained. The successful comparison uses **16 GiB per-query / 32 GiB process
spill limits in both arms**, with a 4 GiB query-memory ceiling. This does not
claim SF1 passes with the default spill quota. Product defaults were not changed.
The [historical Q05 disclosure and subsequent join fix](q05-join-qualification.md#historical-q05-spill-disclosure)
retain this failure and explain the later improvement. The tested spill allowance
is not a measurement of peak usage or the minimum required.
The test MySQL source used a 4 GiB buffer pool, configured before query timing
in every pass. Dataset loading and snapshot time are excluded from the comparison.

To repeat the SF1 comparison with separate baseline/candidate binaries:

```sh
export PINTAIL_DISABLE_SETTLED_MEMO=1
export PINTAIL_QUERY_SPILL_LIMIT_BYTES=17179869184
export PINTAIL_GLOBAL_SPILL_LIMIT_BYTES=34359738368
PINTAIL_BENCHMARK_BINARY=/path/to/binary bun run benchmark/run-tpch.ts --profile sf1
```

Set `innodb_buffer_pool_size=4294967296` on the harness-created MySQL source
before measured queries. Run sequentially on a quiet measurement machine.
The short unrelated process observed during replacement-baseline setup had
exited before Pintail started; none was observed from that startup through
the final TPC-H query. The separate contaminated attempt was discarded.

## Banked evidence and scope

[Machine-readable comparison](storage-scan-qualification.json),
[raw runs](storage-scan-qualification/), and the canonical benchmark/TPC-H
result files are banked together. The freshness registry explicitly tracks
this qualification against the engine crates and benchmark harness inputs.
The complete rc gate is banked separately; this work does not claim a stable
release-chain pass.

The supported claim is narrower than a general speedup: dense text-predicate
scans halve block decoding, scan-bound SQL improves 1.26–1.51× in the controlled
20-million-row probes, and wide aggregates show no material gain. The sparse,
nullable fixture establishes identical answers while genuinely omitting most
projected blocks.
