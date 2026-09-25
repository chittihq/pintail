# Q05 join qualification

The unchanged SF1 Q05 query is qualified against live MySQL, with settled-result
memoization disabled. This is a warm-cache result for the deterministic
TPC-H-derived fixture, not a one-second promise for arbitrary joins or larger
scale factors. The separate eight-query fixture contains 20,000,000 rows.

## 0.1.5 release requalification

This evidence was re-run for the 0.1.5 stable release. The baseline is the
previous stable release, tag `v0.1.4`
(`477ef1cb24f410075418101fab29a1ec90e86f3d`). The candidate is the 0.1.5
release tree (`7ce8559c87a4d1eed81cea70d0d6faedacde13d6`). `v0.1.4` was cut
before the Q05 join change this file first qualified (`b2cb6f5b`) merged, so
the baseline still has the old join order. The pair covers that change and
every other engine change between the two releases. It does not isolate any
single commit.

Both executables are release builds from clean detached checkouts, with the
dashboard prebuilt. Their SHA-256 digests are `2377a9de…` for the baseline and
`3a0b42c7…` for the candidate. The raw reports record them in full.

### Configuration and host

The Pintail server ran as a native process on the build host, which has 32
logical CPUs and 30 GiB of RAM. The harness MySQL 8.4 source ran in a
container on a separate docker host, with 16 logical CPUs and 60 GiB of RAM.
Both arms used a 4 GiB query-memory ceiling. `innodb_buffer_pool_size` on the
harness MySQL was raised to 4 GiB before seeding and was confirmed before
timing. The matched comparison allows 16 GiB of spill per query and 32 GiB
in total. The one-second target uses the default spill limits of 1 GiB and
8 GiB.

One SF1 replica (8,660,779 rows, 6,000,749 of them line items) was created
with the baseline executable and kept. Each executable was then started on
that replica in turn, one at a time, and replayed with
`benchmark/replay-tpch.ts`. Each replay runs the query against MySQL again and
compares the ordered answers exactly. Loading and snapshot time are excluded.
The four-query comparison uses one warmup and three measured runs. The target
uses two warmups and fifteen measured runs.

**The hosts were not idle.** Ten unrelated long-running containers ran on the
docker host for the whole study. One server process left behind by an earlier,
unrelated validation run also stayed alive on the build host, using on average
82.8% of one logical CPU when it appeared among the four busiest processes (315 of 369 samples). This study did not create that process and did not
stop it. No other harness or build ran during measurement. The
[load samples](q05-join-qualification/rel-0.1.5-host-load.json) were taken
every 15 seconds. They count whole-host load, including this study's own work.

### Measured TPC-H results (retained replica)

| Query | v0.1.4 median ms | 0.1.5 median ms | Speedup |
|---|---:|---:|---:|
| q01-pricing-summary | 5,233.13 | 3,551.02 | 1.47× |
| q03-shipping-priority | 3,578.71 | 254.17 | 14.08× |
| q05-local-supplier-volume | 31,950.63 | 319.32 | 100.06× |
| q10-returned-item-reporting | 2,568.35 | 327.43 | 7.84× |

Every sample in both arms matched MySQL exactly. The two arms also produced
identical answer and SQL hashes.

With the default spill limits, the candidate's fifteen Q05 samples have a
**318.84 ms median and a 347.18 ms p95**, and the slowest sample is 347.18 ms.
Every sample is under one second, and every answer is exact (answer SHA-256
`917d70a3…`, the same as the original study's).

The `EXPLAIN ANALYZE` profiles show the same shape as the original change
study:

| Q05 profile observation | v0.1.4 | 0.1.5 |
|---|---:|---:|
| Fact rows emitted to the intermediate join | 6,000,749 | 857,315 |
| Fact storage blocks decoded | 1,684 | 1,684 |
| Largest intermediate | 59,995,715 | — |
| Final joined rows | 6,869 | 6,869 |
| Output rows | 5 | 5 |
| Spill files | 32 | 0 |
| Total spill bytes written | 1,466,506,682 | 0 |

The raw reports are the
[baseline](q05-join-qualification/rel-0.1.5-tpch-baseline.json),
[candidate](q05-join-qualification/rel-0.1.5-tpch-candidate.json) and
[default-spill target](q05-join-qualification/rel-0.1.5-q05-target.json)
replays, each with every sample and its profile. The fresh-replica passes in
[the storage-scan qualification](storage-scan-qualification.md#tpc-h-sf1)
measure the same direction independently.

### 20M paired comparison

`bun run benchmark/run.ts` ran from a clean `v0.1.4` checkout and then from a
clean candidate checkout, one after the other. Each tree was left untouched
during its run. Both runs used the same docker host, with host fingerprint
`2c89ea59…`. Each engine container was limited to eight CPUs and 8 GiB, and
Pintail's query-memory ceiling was 4 GiB. Each checkout reused the MySQL
reference already banked in its own tree: `v0.1.4`'s from 2026-09-09 and the
candidate's from 2026-09-24. Both references carry the same workload and host
fingerprints, and only Pintail samples are compared here. In both arms, all
eight canonical queries and all novel-query families matched MySQL, and both
engines reported zero concurrency errors at every client count.

The first candidate attempt stopped with no timings: the docker host ran out
of disk during the Pintail snapshot. That run's containers and volumes were
removed. Space was then freed in two ways: the gate's own preflight
reclamation (build cache and dangling images), and removal of one abandoned
per-run volume that an earlier benchmark run had left behind. The candidate
was then run again. The table below comes from that rerun.

The table uses the memo-disabled engine track, with fifteen measured samples
after two warmups:

| Query | v0.1.4 median ms | 0.1.5 median ms | v0.1.4 min ms | 0.1.5 min ms | Median change |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 4.59 | 2.37 | 4.41 | 2.34 | −48.4% |
| Q2: Filtered count | 51.32 | 42.19 | 50.36 | 39.78 | −17.8% |
| Q3: Group by status | 132.28 | 182.99 | 127.29 | 164.43 | **+38.3%** |
| Q4: Region × status breakdown | 160.28 | 188.55 | 145.99 | 168.01 | **+17.6%** |
| Q5: Monthly revenue (2023) | 98.39 | 102.37 | 92.89 | 94.27 | +4.0% |
| Q6: Top 10 spenders | 448.47 | 477.44 | 438.88 | 457.46 | **+6.5%** |
| Q7: Regional analytics | 410.91 | 432.56 | 395.11 | 392.69 | **+5.3%** |
| Q8: Join users + orders | 376.18 | 309.63 | 353.08 | 259.99 | −17.7% |

**Q3 and Q4 are regressions.** Their minimums rose by 29.2% and 15.1% as
well as their medians. The release chain's own benchmark of the same
candidate tree on the same host, earlier the same day, measured Q3 at 173 ms
and Q4 at 194 ms, the same direction. The banked `v0.1.4` benchmark
measured 129 ms and 155 ms. Q6's median rose 6.5% and its minimum 4.2%.
Q7's median rose 5.3%, but its minimum held. Q1, Q2 and Q8 improved. On the
memo track, every candidate median is lower (−2% to −53%).

The raw reports are the
[baseline](q05-join-qualification/rel-0.1.5-eight-query-baseline.json) and
[candidate](q05-join-qualification/rel-0.1.5-eight-query-candidate.json)
runs. The harness PASS covers exact answers and its memo-dashboard speed
threshold. It is not a cross-revision regression gate.

### Release gate

The release chain banked the correctness gate separately; it was not re-run
here. That stable-profile run at `a6d58102` passed with 1,908 oracle cases
byte-exact against MySQL 8.4, and 7,015 E2E checks passed with 0 failed, 29
documented-gap warnings and 44 skipped against both MySQL 8.4 and 8.0. The
commits between `a6d58102` and the candidate only bank evidence; the engine
crates and the benchmark harness are identical.

### Deviations from the original procedure

- The pair is release against release, not one change's parent against that
  change.
- Pintail ran on a 32-logical-CPU build host and MySQL on a separate docker
  host. The original TPC-H measurements used one 16-logical-CPU host.
- Neither host was idle; see *Configuration and host*.
- The retained replica was built with a copy of `benchmark/run-tpch.ts`, kept
  outside the repository, that skipped the query suite and teardown. It uses
  the same schema, seed-42 generator and snapshot path. The session file was
  kept outside the repository and deleted afterwards.
- `benchmark/replay-tpch.ts` does not exist at `v0.1.4`, so the candidate
  tree's copy drove both arms. `benchmark/run-tpch.ts` and `benchmark/run.ts`
  are the same in both trees.

## Historical: original change qualification (superseded)

This section records the change study for the Q05 join fix, before the 0.1.5
requalification above. Its numbers describe those revisions only.

The baseline executable was built from `26f4c9f2cca79d137b20a3e54072a3550be46234`,
whose engine sources are identical to its parent `3046f545`. The candidate
executable was built from `b2cb6f5bea0cb9bdfddae0815da4f11b2773bab3`. Both arms
used a 4 GiB query-memory ceiling, a 4 GiB MySQL buffer pool and the same
spill settings as above. The measurement host had 16 logical CPUs and 60 GiB
of RAM, and no competing builds or harnesses ran during it.

| Query | Baseline median ms | Candidate median ms | Speedup |
|---|---:|---:|---:|
| q01-pricing-summary | 9,420.36 | 2,359.56 | 3.99× |
| q03-shipping-priority | 5,463.63 | 563.59 | 9.69× |
| q05-local-supplier-volume | 47,955.18 | 906.29 | 52.91× |
| q10-returned-item-reporting | 4,091.55 | 952.91 | 4.29× |

With default spill limits, Q05 had a 910.73 ms median and a 991.61 ms p95
over fifteen samples. The raw reports are the
[baseline](q05-join-qualification/tpch-baseline.json),
[candidate](q05-join-qualification/tpch-candidate.json) and
[target](q05-join-qualification/q05-target.json) replays. There are also
supplementary [baseline](q05-join-qualification/supplementary-baseline.json)
and [target](q05-join-qualification/supplementary-target.json) replays from a
busier 32-logical-CPU host.

That study's 20M pair ran on a shared 16-logical-CPU, 30 GiB host. It
compared control checkout `20ef46cc` with candidate checkout `415aaa2c`, and
measured an 8.2% Q4 median increase. See
[baseline](q05-join-qualification/eight-query-shared-baseline.json),
[candidate](q05-join-qualification/eight-query-shared-candidate.json) and
[load samples](q05-join-qualification/background-load.json). An earlier
[baseline report](q05-join-qualification/eight-query-baseline.json) is also
kept. Its final concurrency result was excluded because another oracle
overlapped it. That study's rc gate passed at `b2cb6f5b`
([report](q05-join-rc.md)).

## Historical Q05 spill disclosure

The storage-scan baseline before the join fix failed SF1 Q05 with the default
1 GiB query spill allowance. The [original failed result](storage-scan-qualification/tpch-baseline-default-quota.json)
remains banked. With a 16 GiB allowance, the old plan finished in about 48
seconds, which did not make execution efficient. `v0.1.4` still has that
plan: at a 16 GiB allowance it writes the same 32 spill files and
1,466,506,682 bytes.

The old optimizer chose the first connected pair in a cyclic graph. That
created fanout across the dimension tables before the fact table was joined:
a 59,995,715-row customer/supplier intermediate, then a 12,066,680-row
dimension result, feeding a final join of 6,869 rows. The total bytes written
are neither the peak spill retained nor the smallest spill quota that would
suffice.

The fixed optimizer chooses lower estimated intermediate work. It folds
literal date intervals so the existing scan bounds apply, and it propagates
complete integer join membership through inner joins. It also avoids
expanding or copying packed payloads that are never used. Estimates never
authorize dropping a row. Membership rejection only uses a complete,
budgeted set, and the final join still checks every key and its residual
predicate. Unsupported paths keep their original execution. The fact scan
still decodes the same blocks. The speedup comes mainly from execution order
and payload handling, not from skipping more storage blocks.

## Correctness

The cyclic-join fixture computes its expected results independently, with
sparse selectivity, nullable keys and projected values. It checks exact
values before it asserts intermediate row counts. Additional tests cover
packed nulls, ENUM ordinals, scalar formatting, projection masks, duplicate
projections and literal date-interval bounds.

## Reproduction

Build baseline and candidate release binaries on Linux. Use the schema, the
seed-42 SF1 generator and the snapshot setup in `benchmark/run-tpch.ts` to
create an isolated synthetic source and replica. Keep the API token and
connection settings in a private session file outside the repository, in the
format given at the top of `benchmark/replay-tpch.ts`. Keep the same replica
between binaries, and run one server process at a time with no competing
builds or harnesses on the measurement host.

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

For the canonical 20M eight-query comparison, run `bun run benchmark/run.ts`
from clean baseline and candidate checkouts, one after the other. Leave each
checkout untouched until the harness exits. For the complete correctness gate,
run `bun run scripts/validate.ts --profile rc`. A subset is not that gate.
