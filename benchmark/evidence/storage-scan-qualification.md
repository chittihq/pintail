# Storage scan qualification

This change caches immutable block directories and reuses predicate columns
that are already decoded when the output projection is identical. It changes
no on-disk format. How much it helps depends on the scan shape. Wide
arithmetic is not expected to get faster from avoiding a small amount of
storage work.

## 0.1.5 release requalification

This evidence was re-run for the 0.1.5 stable release. The baseline is the
previous stable release, tag `v0.1.4`
(`477ef1cb24f410075418101fab29a1ec90e86f3d`). The candidate is the 0.1.5
release tree (`7ce8559c87a4d1eed81cea70d0d6faedacde13d6`). `v0.1.4` branched
from `b019dae2`, which is this change's original baseline, and contains
neither this change (`26f4c9f2`) nor the later Q05 join change. The pair
therefore spans both changes and every other engine change between the two
releases.

Both release executables are the same builds used in the
[Q05 requalification](q05-join-qualification.md#015-release-requalification).
Pintail and the storage probes ran natively on the build host, which has 32
logical CPUs and 30 GiB of RAM. The harness MySQL and the 20M benchmark
engines ran on a separate docker host with 16 logical CPUs and 60 GiB of RAM.
**Neither host was idle.** Ten unrelated long-running containers ran on the
docker host throughout. On the build host, a server process left behind by an
earlier, unrelated validation run used close to one logical CPU (82.8% on
average in the samples where it appeared). This study did not create that
process and did not stop it. No other harness or build ran during
measurement. Whole-host
[load samples](q05-join-qualification/rel-0.1.5-host-load.json) were taken
every 15 seconds and include this study's own work.

## Skip condition

The directory records each block's half-open physical row interval. The
interval comes from the on-disk block row counts and is checked against the
segment row count. The reader validates that the requested ranges are
ordered and do not overlap. It skips ranges that end before a block, and
decodes the block exactly when the next range intersects it. If that range
starts at or beyond the block's end, every later range starts even farther
along and cannot intersect the block either. Predicate evaluation supplies
the selected ranges before any projected block is considered. The
adversarial comparison below tests that whole path, including ranges that
cross scan boundaries and partial blocks.

## Row preservation: 20 million adversarial rows

The fixture was seeded once by the `v0.1.4` probe and then opened by both
executables. Both checked every returned value, and its order, against
independently generated expected rows, including a check that no trailing
rows were missing. The four answer files match byte for byte (`cmp`). Their
SHA-256 digests are also identical to the original study's.

The fixture has 39 segments. Its directory totals 1,509,167,019 bytes,
including the manifest and WAL. Each segment holds at most 524,288 rows.
Matching text is clustered in one block out of every 32, with extra matches
at scan boundaries. The predicate column has one all-null block out of every
32 and scattered nulls every 97 rows. The projected binary and variable-width
text columns have nulls every 13 and 17 rows respectively. Each non-null
binary value holds 64 deterministically generated opaque bytes. The final
segment is partial.

| Selection / projection | Returned rows | v0.1.4 decoded | 0.1.5 decoded | v0.1.4 pruned | 0.1.5 pruned |
|---|---:|---:|---:|---:|---:|
| Clustered text, predicate-only output | 632,501 | 1,298 | 1,221 | 8,440 | 3,648 |
| Clustered text, reordered mixed output | 632,501 | 1,529 | 1,529 | 22,816 | 22,816 |
| Null predicate, reordered mixed output | 822,359 | 6,105 | 6,105 | 18,240 | 18,240 |
| No matches | 0 | 1,221 | 1,221 | 23,124 | 23,124 |

The counters are identical to the original study's in both arms. In the
clustered mixed projection, the 1,529 decodes are all 1,221 predicate blocks
plus 77 blocks for each of the four output columns. So 1,144 of the 1,221
blocks in each output column are skipped, while every expected matching row
is kept. The null predicate touches every block because its nulls are
scattered.

These are the scanner's cumulative physical block counters, not counts of
distinct blocks. Pruned counts include exclusions from repeated sliced reads,
so they must not be read as a percentage of the fixture's blocks.

**Probe source.** The `v0.1.4` store API predates the range wrapper that the
current examples return. The baseline therefore used the probe sources as of
`26f4c9f2`, the original probe revision, copied into the `v0.1.4` checkout
and removed after the build. The candidate used the examples in its own tree.
The only difference between the two sources is the return type of the
predicate range. The fixture, predicates, projections and checks are
identical. Raw output:
[baseline](storage-scan-qualification/rel-0.1.5-adversarial-baseline.txt),
[candidate](storage-scan-qualification/rel-0.1.5-adversarial-candidate.txt),
[answer digests](storage-scan-qualification/rel-0.1.5-adversarial-answers.sha256).

## Uniform 20-million-row probes

Both executables read the same fixture, which the `v0.1.4` probe seeded (20
segments). Each probe runs two warmups and seven measured iterations, with
SQL settled-result memoization disabled, and checks every expected result.
This is warm-cache evidence. All baseline probes ran before any candidate
probe.

| Probe | Case | v0.1.4 median ms | 0.1.5 median ms | Speedup | Decoded blocks, v0.1.4 → 0.1.5 |
|---|---|---:|---:|---:|---:|
| scan | narrow-last | 70.893 | 36.092 | 1.96× | 1,221 → 1,221 |
| scan | wide | 1,429.146 | 1,425.426 | 1.00× | 29,304 → 29,304 |
| scan | text-all | 108.359 | 18.496 | 5.86× | 2,442 → 1,221 |
| scan | text-selective | 129.164 | 17.700 | 7.30× | 2,442 → 1,221 |
| scan | mixed-selective | 201.972 | 129.725 | 1.56× | 3,663 → 3,663 |
| query | numeric-filter | 31.901 | 25.003 | 1.28× | — |
| query | text-filter | 29.588 | 24.239 | 1.22× | — |
| query | text-all | 39.076 | 34.191 | 1.14× | — |
| query | wide-expression | 18,388.322 | 14,969.474 | 1.23× | — |

The wide projected scan is unchanged. Wide arithmetic is 1.23× faster in this
release pair. That gain comes from other changes between the releases; this
storage change did not produce it (the original study measured 1.02×). The
build host is not the machine the original probes ran on, so compare absolute
times only within this table. Raw output:
[scan baseline](storage-scan-qualification/rel-0.1.5-uniform-scan-baseline.txt),
[scan candidate](storage-scan-qualification/rel-0.1.5-uniform-scan-candidate.txt),
[query baseline](storage-scan-qualification/rel-0.1.5-uniform-query-baseline.txt),
[query candidate](storage-scan-qualification/rel-0.1.5-uniform-query-candidate.txt).

## TPC-H SF1

The two binaries alternated for three passes each. Every pass loaded a fresh
replica of 8,660,779 rows across eight tables (6,000,749 line items). Each of
the four supported queries ran once per replica, in a fixed order, with
settled-result memoization disabled. All **24 comparisons were byte-exact
against MySQL**. This is the repository's four-query workload, not the full
22-query suite. Both arms used a 16 GiB per-query and 32 GiB process spill
allowance and a 4 GiB query-memory ceiling. The harness MySQL's buffer pool
was raised to 4 GiB before seeding in every pass and was confirmed before
the queries ran.

| Query | v0.1.4 median ms | 0.1.5 median ms | Speedup | v0.1.4 samples ms | 0.1.5 samples ms |
|---|---:|---:|---:|---|---|
| q01-pricing-summary | 5,213 | 3,563 | 1.46× | 5,213 / 5,245 / 5,170 | 3,541 / 3,597 / 3,563 |
| q03-shipping-priority | 3,462 | 266 | 13.02× | 3,462 / 3,449 / 3,464 | 281 / 263 / 266 |
| q05-local-supplier-volume | 31,731 | 322 | 98.54× | 31,691 / 31,950 / 31,731 | 327 / 320 / 322 |
| q10-returned-item-reporting | 2,532 | 331 | 7.65× | 2,515 / 2,595 / 2,532 | 331 / 338 / 325 |

Run `PINTAIL_BENCHMARK_BINARY=<binary> bun run benchmark/run-tpch.ts --profile sf1`
with the spill and memo variables below. A watcher outside the repository
raised the buffer pool on each harness MySQL container, because the harness
does not set it. Raw pass reports are
`storage-scan-qualification/rel-0.1.5-tpch-{baseline,candidate}-{1,2,3}.json`.

```sh
export PINTAIL_DISABLE_SETTLED_MEMO=1
export PINTAIL_QUERY_SPILL_LIMIT_BYTES=17179869184
export PINTAIL_GLOBAL_SPILL_LIMIT_BYTES=34359738368
PINTAIL_BENCHMARK_BINARY=/path/to/binary bun run benchmark/run-tpch.ts --profile sf1
```

Set `innodb_buffer_pool_size=4294967296` on the harness-created MySQL source
before the measured queries.

## Canonical eight-query benchmark

This is the same paired 20M run as in the
[Q05 requalification](q05-join-qualification.md#20m-paired-comparison), with
the same caveats: a shared docker host and one discarded out-of-disk
candidate attempt. Both runs used 20 million synthetic rows on the same
docker host (fingerprint `2c89ea59…`). Each database container was limited to
8 CPUs and 8 GiB, Pintail's query ceiling was 4 GiB, and each query ran two
warmups followed by 15 measured iterations. Both gates passed with exact
answers, and both engines reported zero concurrency errors.

Memo-disabled engine track:

| Query | v0.1.4 median ms | 0.1.5 median ms | v0.1.4 min ms | 0.1.5 min ms | Median speedup |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 4.59 | 2.37 | 4.41 | 2.34 | 1.94× |
| Q2: Filtered count | 51.32 | 42.19 | 50.36 | 39.78 | 1.22× |
| Q3: Group by status | 132.28 | 182.99 | 127.29 | 164.43 | **0.72×** |
| Q4: Region × status breakdown | 160.28 | 188.55 | 145.99 | 168.01 | **0.85×** |
| Q5: Monthly revenue (2023) | 98.39 | 102.37 | 92.89 | 94.27 | 0.96× |
| Q6: Top 10 spenders | 448.47 | 477.44 | 438.88 | 457.46 | **0.94×** |
| Q7: Regional analytics | 410.91 | 432.56 | 395.11 | 392.69 | **0.95×** |
| Q8: Join users + orders | 376.18 | 309.63 | 353.08 | 259.99 | 1.21× |

**Q3 (+38.3% median, +29.2% minimum) and Q4 (+17.6% median, +15.1% minimum)
regressed between the releases.** Both are larger than the repository's ±10%
noise allowance, and the minimums moved with the medians. The release chain's
own benchmark of the same candidate tree measured the same direction. Q6
(+6.5% median, +4.2% minimum) and Q7 (+5.3% median, minimum unchanged) are
also slower by more than 5%. This storage change is not established as the
cause of any of these; the pair spans the whole release. The raw reports are
in the Q05 raw directory:
[baseline](q05-join-qualification/rel-0.1.5-eight-query-baseline.json) and
[candidate](q05-join-qualification/rel-0.1.5-eight-query-candidate.json).

## Release gate

The release chain banked the correctness gate separately; it was not re-run
here. The stable-profile run at `a6d58102` passed all stages, including 1,908
oracle cases byte-exact against MySQL 8.4 and 7,015 E2E checks with 0
failures (29 warnings, 44 skipped) on MySQL 8.4 and 8.0. Commits after
`a6d58102` up to the candidate only bank evidence.

## Deviations from the original procedure

- The pair compares the previous stable release with this release, not this
  change with its parent.
- The probes and Pintail ran on a 32-logical-CPU build host, and neither host
  was idle.
- The baseline probe sources are the `26f4c9f2` versions, unmodified;
  the candidate's differ only in the range return type.
- A watcher outside the repository set the TPC-H buffer pool; it was confirmed
  on every pass.

## Historical: original change qualification (superseded)

This section records the change study at candidate
`26f4c9f2cca79d137b20a3e54072a3550be46234` against baseline `b019dae2`,
before the 0.1.5 requalification. Its numbers describe those revisions only.
It ran on an idle 16-logical-CPU measurement machine.

- Adversarial fixture: the same counters as the table above in both arms,
  and answer digests identical to this requalification's.
- Uniform probes: text-all and text-selective scans ran 4.84× and 5.67×
  faster, narrow-last 1.98×, mixed-selective 1.47× and wide 1.05×;
  scan-bound SQL ran 1.26–1.51× faster and wide arithmetic 1.02×. Raw:
  `storage-scan-qualification/uniform-{scan,query}-{baseline,candidate}.txt`.
- TPC-H SF1: three alternating passes with 16 GiB/32 GiB spill. Medians
  changed by between −0.36% and +0.73%: q01 9,252 → 9,219 ms, q03
  5,395 → 5,421 ms, q05 47,791 → 47,793 ms, q10 3,985 → 4,014 ms. Raw:
  `storage-scan-qualification/tpch-{baseline,candidate}-{1,2,3}.json`.
- Eight-query engine track: the largest median slowdown was Q4 at 5.8%, and
  its minimum improved. Raw:
  `storage-scan-qualification/eight-query-{baseline,candidate}.json`. One
  earlier baseline attempt was discarded after a separate native benchmark
  started during its timed queries.
- The rc gate passed at `26f4c9f2` ([report](storage-scan-rc.md)).

**Capacity disclosure (historical, still true of `v0.1.4`).** The unchanged
baseline failed Q05 with the default 1 GiB query spill quota. That
[failed result](storage-scan-qualification/tpch-baseline-default-quota.json)
remains banked. The
[Q05 spill disclosure](q05-join-qualification.md#historical-q05-spill-disclosure)
explains the later fix. The tested spill allowance does not measure peak
usage or the minimum quota required.

## Reproduction

Build `storage_adversarial_probe`, `storage_scan_probe` (pintail-store) and
`storage_query_probe` (pintail-exec) for both arms. If the baseline tree does
not have them, copy the example sources into a detached baseline checkout
before building. Keep the resulting executables separate. Seed with the
baseline, and run every baseline probe before any candidate probe:

```sh
baseline-adversarial DATA 20000000 baseline-answers
candidate-adversarial DATA 20000000 candidate-answers
for n in 0 1 2 3; do
  cmp "baseline-answers/case-$n.txt" "candidate-answers/case-$n.txt"
done
baseline-scan UNIFORM 20000000 --seed-only
PINTAIL_DISABLE_SETTLED_MEMO=1 baseline-scan UNIFORM 20000000
PINTAIL_DISABLE_SETTLED_MEMO=1 baseline-query UNIFORM 20000000
# then the candidate scan and query probes on the same UNIFORM directory
```

## Banked evidence and scope

The [machine-readable comparison](storage-scan-qualification.json) keeps the
original study under `historical`. The raw runs for this requalification carry
the `rel-0.1.5-` prefix in [the raw directory](storage-scan-qualification/),
and the earlier files remain. The freshness registry tracks this
qualification against the engine crates and the benchmark harness inputs.

The supported claim is narrow. Dense text-predicate scans decode half as many
blocks. The sparse, nullable fixture returns identical answers while skipping
most projected blocks. 0.1.5 answers every TPC-H and 20M benchmark query
exactly. 0.1.5 is much faster than `v0.1.4` on the four TPC-H queries. It is
slower on the Q3 and Q4 20M aggregates, which is a regression and is reported
here as one.
