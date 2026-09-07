# Extended release correctness

A supplementary workflow runs on pushed `v*` tags, including release candidates,
and on manual dispatch. Branch pushes and pull requests do not trigger it.
Creating a tag only on a developer's machine does not trigger GitHub Actions.
The workflow must be present in the tagged commit. Existing CI, validation,
release publication and development commands are unchanged. Publication does
not wait for this workflow; its final status is separate release evidence.

All jobs use standard GitHub-hosted Linux runners and their own local Docker.
There are no deployment credentials, private datasets, external Docker hosts,
repository writes, or automatic commits. Matrix failures do not cancel other
shards. Artifacts are retained for 14 days; download evidence that must survive
longer. The job timeouts bound runs, and the SQL matrix runs at most four jobs
simultaneously. Other workflows still share the account's runner concurrency
and cache capacity.

## Layers

| Layer | Release workload | Oracle |
| --- | --- | --- |
| Fixed SQL | Existing corpus once per MySQL version | MySQL 8.0 and 8.4 |
| Generated SQL | Four shards × two seeds × 2,500 cases per version | MySQL 8.0 and 8.4 |
| Equivalent SQL | Four shards × two seeds × 1,000 base queries per version leg | Existing equivalent-query transformations |
| Execution/storage | SQL regression and spill packs; feature-enabled storage/metadata fault tests | Assertions and expected results |
| Query reuse lab | Current standalone prototype correctness tests | Independent execution and invalidation assertions |
| Real server | Existing full E2E harness for both MySQL versions | Replication convergence and differential queries |

That is 20,000 generated SQL cases per MySQL version, 40,000 comparisons
across versions. Generated SQL may repeat: the harness reports unique SQL and
family counts; these are not 40,000 distinct hand-written cases. Metamorphic
runs have 8,000 base queries per version leg and a variable number of equivalent
variants; this layer does not use MySQL and intentionally repeats on both legs.
The existing metamorphic harness may skip erroring base queries, with a hard
20% ceiling; logs and reports expose those counts. A generated differential
query error fails its stage.
Each shard combines a stable seed with a commit-derived seed. Odd seeds remain
distinct after the existing generator's seed normalization. Release reruns are
reproducible; new commits explore another generated sequence.

The workflow extends execution of existing harnesses instead of duplicating
SQL semantics in another oracle. Fixed/generated comparisons use the harness's
existing typed normalization, including its explicit floating-point tolerance;
this is not universal byte-exact comparison. SQL fixtures and generation are
synthetic. `generated.sql` is exported before execution, so a failure retains
the input corpus. Logs preserve the harness's seed/case diagnostics. Automatic
failure shrinking is not implemented.

The runner fails for nonzero exit codes, timeouts, and successful cargo commands
that ran zero tests. It continues subsequent layers and records each outcome.
Reports include the source commit, dirty status, seed derivation, commands,
case budgets, environment overrides, elapsed times and logs. The summary counts
Rust tests separately from SQL comparisons. Hard job cancellation may leave an
incomplete report, which is never reported as a completed PASS.

## Reproduction

Run builds on the authorized build machine. The SQL layer requires Docker;
the core layer does not. From the repository root:

```sh
python3 -m unittest discover -s tests/extended -p 'test_*.py'
python3 tests/extended/run.py --layer sql --mysql 8.4 --shard 0
python3 tests/extended/run.py --layer core
```

Use `--seed-commit <recorded-sha>` to replay the generated sequence while testing
a fix on another commit. Use `--cases 20 --meta-cases 10` for harness validation;
this is a reduced run, not release-scale evidence. `--output <directory>` selects
a fresh artifact destination. Full reproduction also requires the recorded
source, lockfiles, toolchain and MySQL version; the `mysql:8.x` tags are floating
patch-version channels, not pinned image digests.

## Scope of confidence

This adds independent differential, equivalent-query and injected-failure
layers. It does not reproduce an entire mature database testing program, nor
does it prove absence of defects. The reuse mechanisms remain prototypes: the
live E2E layer exercises the current engine, not unimplemented production cache
integration. Dedicated publication-race model checking, generated CDC histories,
mutation testing of invalidation safeguards, and shadow comparison remain work
for that integration. The ordinary RC/stable release gates remain required.

## Initial validation

The workflow passed `actionlint`; five Python runner tests passed, including
nonzero-exit and zero-tests failure propagation. On a clean archived checkout,
the core runner passed 22 SQL/spill tests, 180 storage/fault tests and seven
reuse prototype tests. The SQL runner passed the fixed MySQL 8.4 corpus,
40 generated comparisons (40 unique SQL, zero skips), and 94 equivalence
comparisons over 20 generated bases (zero skips). Core evidence names
`3d31c63`; the subsequent SQL smoke names `7ddae3f`.

These are local harness-validation results on the authorized build machine,
not a completed GitHub Actions release matrix. The 8.0 leg, full generated
budget and hosted-runner E2E jobs await the first workflow execution. The
workflow is committed locally and becomes available after it is pushed.
