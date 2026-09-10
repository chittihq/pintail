# Q05 join qualification

The unchanged SF1 Q05 query is qualified against live MySQL, with settled-result
memoization disabled. This is a warm-cache result for the deterministic
TPC-H-derived fixture, not a one-second promise for arbitrary joins or larger
scale factors. The separate eight-query fixture contains 20,000,000 rows.

## Revisions and configuration

The baseline executable is from `26f4c9f2cca79d137b20a3e54072a3550be46234`;
its engine sources are identical to parent `3046f545`. The candidate executable
is from `b2cb6f5bea0cb9bdfddae0815da4f11b2773bab3`. Raw reports include executable
SHA-256, substituted SQL SHA-256, answer SHA-256, row counts, settings and every
measured sample. Each replay executes MySQL again and compares the ordered
scalar/null answer matrix exactly; timing runs do not reuse Pintail results.

Both arms use a 4 GiB query-memory ceiling and a 4 GiB MySQL buffer pool.
The matched comparison allows 16 GiB per query / 32 GiB globally for spill.
The one-second target uses the unchanged default spill limits, 1 GiB / 8 GiB.
The 4 GiB query-memory ceiling is an explicit benchmark setting, not a claim
that every setting uses the product default. Loading and snapshot time are
excluded. The four-query comparison has one warmup and three measured runs;
the target has two warmups and fifteen measured runs, with p95 <= 1,000 ms.

## Measured TPC-H results

| Query | Baseline median ms | Candidate median ms | Speedup |
|---|---:|---:|---:|
| q01-pricing-summary | 9,420.36 | 2,359.56 | 3.99× |
| q03-shipping-priority | 5,463.63 | 563.59 | 9.69× |
| q05-local-supplier-volume | 47,955.18 | 906.29 | 52.91× |
| q10-returned-item-reporting | 4,091.55 | 952.91 | 4.29× |

With default spill limits, Q05's fifteen measured samples have a **910.73 ms median and 991.61 ms p95**. Every sample is below one second. The analyzed matched run writes no spill files; it retains the same 6,869 joined rows and five output rows as the baseline.

The measurement host has 16 logical CPUs and 60 GiB RAM. No competing builds
or harnesses ran during these TPC-H measurements. Raw reports are
[baseline](q05-join-qualification/tpch-baseline.json),
[candidate](q05-join-qualification/tpch-candidate.json), and
[default-spill target](q05-join-qualification/q05-target.json).

An additional comparison on a 32-logical-CPU, 30 GiB host returned exact
answers with a 31,443.60 ms baseline median and a 563.20 ms candidate median
(719.49 ms p95, fifteen runs). Other validation activity was present throughout
that run; it is supplementary evidence, not the idle-host regression baseline.
Its [baseline](q05-join-qualification/supplementary-baseline.json) and
[candidate](q05-join-qualification/supplementary-target.json) used a private native
MySQL 8.4 instance and a refreshed replica of the same synthetic source.

## Historical Q05 spill disclosure

The prior storage-scan baseline failed SF1 Q05 with the default 1 GiB query
spill allowance. The [original failed result](storage-scan-qualification/tpch-baseline-default-quota.json)
remains banked. Raising the allowance to 16 GiB allowed the old plan to finish
in about 48 seconds; it did not make execution efficient.

A new baseline profile identifies a 59,995,715-row customer/supplier intermediate,
followed by a 12,066,680-row dimension result, for a final 6,869-row join.
It records 32 spill files and 1,466,506,682 bytes written. Total bytes written
are not peak retained spill usage or the minimum sufficient spill quota.
The optimizer had selected the first connected pair in a cyclic graph,
creating dimension fanout before joining the fact table.

The final candidate profile keeps the same final join cardinality:

| Profile observation | Baseline | Candidate |
|---|---:|---:|
| Fact rows emitted to the intermediate join | 6,000,749 | 857,315 |
| Fact storage blocks decoded | 1,684 | 1,684 |
| Final joined rows | 6,869 | 6,869 |
| Output rows | 5 | 5 |
| Spill files | 32 | 0 |
| Total spill bytes written | 1,466,506,682 | 0 |

The fact scan still decodes the same blocks. Complete join membership removes
impossible matches before intermediate row copying; the final join retains
all matching rows. The speedup comes principally from execution order and
payload handling, rather than an additional storage-block skip.

The candidate chooses lower estimated intermediate work, folds literal date
intervals so existing scan bounds apply, propagates complete integer join
membership through inner joins, and avoids expanding or copying unused packed
payloads. Estimates never authorize dropping a row. Membership rejection only
uses a complete, budgeted set, and the final join still checks all keys and its
residual predicate. Unsupported paths retain their original execution.

## 20M paired comparison and qualification status

The Q05 target, four-query TPC-H comparison, [complete rc profile](q05-join-rc.md),
and 20M exact-answer benchmark passed. All eight canonical queries and the
novel-query families matched their MySQL reference answers in both arms.
Both engines reported zero concurrency errors at every tested client count.
The harness PASS covers exact answers and its memo-dashboard speed threshold;
it is not a cross-revision engine-regression gate. The explicit table below
is the before/after engine comparison.

The 20M pair used a different, 16-logical-CPU, 30 GiB host with background
services, at the owner's request after repeated harness collisions on the
first host. Each benchmark engine had the same eight-CPU / 8 GiB container
limits; Pintail's query-memory ceiling was 4 GiB. This is **not an idle-host
regression qualification**. MySQL references from 2026-09-03 were reused only
after exact host and workload fingerprints matched. A fresh partial MySQL
pass also confirmed the first five canonical answers and SQL hashes before
cache reuse; both Pintail timing runs below are new.

The control checkout is `20ef46cc`, with engine sources identical to the
parent; the candidate checkout is `415aaa2c`, with engine sources identical
to `b2cb6f5b`. Their additional commits only bank the matching MySQL reference.
Raw reports: [baseline](q05-join-qualification/eight-query-shared-baseline.json)
and [candidate](q05-join-qualification/eight-query-shared-candidate.json).
The table uses the memo-disabled engine track, fifteen measured samples after
two warmups, not the repeated-result memo track.

| Query | Baseline median ms | Candidate median ms | Baseline minimum ms | Candidate minimum ms |
|---|---:|---:|---:|---:|
| Q1: Full table count | 5 | 4 | 4 | 4 |
| Q2: Filtered count | 44 | 44 | 42 | 42 |
| Q3: Group by status | 133 | 131 | 121 | 120 |
| Q4: Region × status breakdown | 146 | 158 | 141 | 148 |
| Q5: Monthly revenue (2023) | 99 | 94 | 91 | 86 |
| Q6: Top 10 spenders | 436 | 407 | 388 | 384 |
| Q7: Regional analytics | 376 | 370 | 314 | 356 |
| Q8: Join users + orders | 386 | 370 | 338 | 338 |

Seven medians held or improved. **Q4's median increased 8.2% (146 → 158 ms)**,
and its minimum increased 5.0% (141 → 148 ms). Q7's median improved 1.6%,
but its minimum increased 13.4%. These observations are retained rather than
asserted to be noise. The shared host prevents attributing the differences
confidently to the engine or claiming regression-free performance. The Q05
one-second target is independently established on the idle host above.

Background sampling recorded 169 observations, averaging 24.96% of one logical CPU outside the harness, with a maximum of 98.62%. This includes setup; named benchmark engine containers are excluded, while setup helpers may be included. [Load samples](q05-join-qualification/background-load.json) and the raw latency samples are banked.

Earlier candidate setup attempts were interrupted by overlapping harnesses
and produced no usable timings. An earlier baseline on the first host had
exact eight-query answers, but another oracle started during the last two
seconds of its final concurrency test; that concurrency result is excluded.
The original [baseline report](q05-join-qualification/eight-query-baseline.json)
is retained separately and is not the control for the table above.

## Correctness

The cyclic-join fixture independently computes expected results with sparse
selectivity and nullable keys and projected values. It checks exact values
before asserting intermediate row counts. Additional tests cover packed nulls,
ENUM ordinals, scalar formatting, projection masks and duplicate projections,
and literal date-interval bounds. The complete rc gate also passed through the real snapshot,
CDC and wire paths against MySQL 8.4 and 8.0: 1,231 byte-exact oracle cases,
1,039 unit tests, and 5,754 E2E checks per version, with zero failures. Both
versions retain the same 30 documented-gap warnings and 44 skips as the baseline.

## Reproduction

Build baseline and candidate release binaries on Linux. Use the schema,
seed-42 SF1 generator and snapshot setup in `benchmark/run-tpch.ts` to create
an isolated synthetic replica and source. Keep their API token and connection
settings in a private session file, outside the repository, in the format
specified at the top of `benchmark/replay-tpch.ts`. Retain the same replica
between binaries, with one server process at a time and no competing builds
or harnesses on the measurement host.

```sh
export PINTAIL_DISABLE_SETTLED_MEMO=1
export PINTAIL_QUERY_MEMORY_LIMIT_BYTES=4294967296
export PINTAIL_QUERY_SPILL_LIMIT_BYTES=17179869184
export PINTAIL_GLOBAL_SPILL_LIMIT_BYTES=34359738368
# Start the selected binary against the synthetic replica, then:
bun run benchmark/replay-tpch.ts --session /tmp/tpch-session.json \
  --revision "$ENGINE_REVISION" --pid "$PINTAIL_PID" \
  --runs 3 --warmups 1 --analyze --out /tmp/tpch-comparison.json
# Restart the candidate with the default spill limits (1 GiB / 8 GiB), then:
bun run benchmark/replay-tpch.ts --session /tmp/tpch-session.json \
  --revision "$ENGINE_REVISION" --pid "$PINTAIL_PID" \
  --queries q05-local-supplier-volume --runs 15 --warmups 2 \
  --target-ms 1000 --out /tmp/q05-target.json
```

Run `bun run benchmark/run.ts` from clean baseline and candidate checkouts,
serially, for the canonical 20M eight-query comparison. Keep each checkout
frozen until the harness exits. Use `bun run scripts/validate.ts --profile rc`
for the complete correctness gate. A subset is not that gate.
