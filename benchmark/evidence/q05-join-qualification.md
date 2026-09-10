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

The candidate chooses lower estimated intermediate work, folds literal date
intervals so existing scan bounds apply, propagates complete integer join
membership through inner joins, and avoids expanding or copying unused packed
payloads. Estimates never authorize dropping a row. Membership rejection only
uses a complete, budgeted set, and the final join still checks all keys and its
residual predicate. Unsupported paths retain their original execution.

## Qualification status

The Q05 target and four-query TPC-H comparison above are complete. The full rc
gate and 20M candidate benchmark are still pending; this evidence is not a
completed release gate. The 20M baseline's eight-query answers are exact.
Another oracle started during the last two seconds of its final concurrency
test, after the eight-query measurements, so that concurrency result is excluded.
Candidate setup attempts interrupted by overlapping harnesses produced no
usable timings. These are setup failures, not successful benchmark runs.

## Correctness

The cyclic-join fixture independently computes expected results with sparse
selectivity and nullable keys and projected values. It checks exact values
before asserting intermediate row counts. Additional tests cover packed nulls,
ENUM ordinals, scalar formatting, projection masks and duplicate projections,
and literal date-interval bounds. The full rc gate must also cover the real snapshot,
CDC and wire paths against both MySQL versions before qualification is complete.

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
