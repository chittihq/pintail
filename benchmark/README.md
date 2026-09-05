# Analytical benchmark

This is Pintail's deterministic port of Duckling's 20-million-order analytical
suite. One run creates isolated MySQL 8.4 and ClickHouse 25.8 containers on the
configured Docker host, snapshots the same MySQL tables into Pintail, verifies
row counts, and executes the same eight aggregate queries against all three
engines.

The release gate uses the sum of the eight measured query times and requires
Pintail to be at least 50 times faster than the source MySQL AND every query's
result to match MySQL exactly (canonical multiset comparison — LIMIT queries
carry deterministic tie-break keys so the comparison is well-posed).

## Methodology (fairness rebuild, issue #3 step 0)

- **All engines run on the Docker host under identical limits** (8 CPUs, 8 GB
  each). Pintail runs as a container built from the working tree — which must
  be clean, so every measurement is attributable to a commit
  (`PINTAIL_BENCHMARK_ALLOW_DIRTY=1` overrides for local iteration;
  `PINTAIL_BENCHMARK_LOCAL=1` restores the old local-process mode).
- **Two ClickHouse references**: plain MergeTree (the raw-speed ceiling) and
  ReplacingMergeTree read with `final = 1` — ClickHouse performing the same
  always-correct merge-on-read duty Pintail performs, the apples-to-apples
  target. Note: on static fully-merged data the two converge; the FINAL
  distinction matters under a live update tail.
- **Timing**: the canonical eight report medians of 5 warm runs. The four
  ad-hoc shapes report medians of 5 distinct predicate variants, each executed
  once so Pintail's exact-result memo cannot serve a repeated query. MySQL
  timings are cold and cached per variant because full-scale queries run for
  minutes. P95/min and every cold variant are recorded in `results.json`.
- **Per-query EXPLAIN ANALYZE snapshots** land in results.json so segment and
  block pruning behavior is inspectable per run.
- **Host variance caveat**: the host may carry unrelated load; ClickHouse's
  own totals swing between runs, so cross-run comparisons should use the
  same-run pintail/ClickHouse ratio, not absolute times. A single before/after
  pair across two runs cannot attribute a change: interleave the arms within
  one run and compare per-arm minimums.
- **What this dataset cannot show.** `seed.sql` dates every order by
  `DATE_ADD('2020-01-01', INTERVAL MOD(generated_id * 7, 1825) DAY)`. Seven and
  1825 share no factor, so consecutive rows step a week through a five-year
  range and wrap, and every block holds very nearly all 1825 distinct dates.
  Block minimum and maximum therefore span the whole range everywhere: a
  one-year filter prunes nothing, measured as 0 of 8,736 blocks pruned on Q5.
  Range-based late materialization fares no better, since the fifth of rows a
  year selects are spread evenly through every block rather than gathered.

  Both techniques are central to a columnar engine and both score zero here,
  so this benchmark reads as raw scan and decode throughput. That is a fair
  contest - ClickHouse reads the same rows - but it means clustering, zone
  maps and ordered layouts cannot register, while real order data, whose
  dates track insertion, would reward all three (e09: "pruning value is
  entirely a function of data clustering"). Do not conclude from a flat
  result here that such work is worthless; conclude that this dataset cannot
  measure it.

## Run

```sh
cd benchmark
bun install --frozen-lockfile
bun run benchmark
```

The default scale is exactly 20,000,000 orders, 100,000 users, and 10,000
products. A smaller run validates orchestration and query compatibility but
does not claim or enforce the release gate:

```sh
BENCHMARK_SCALE=0.001 bun run smoke
```

Set `PINTAIL_BENCHMARK_BINARY` to reuse an existing release binary. Otherwise
the harness builds `pintail` in release mode. Pintail runs with an explicit
256 MiB per-query memory ceiling. Results are written to
`results.md` and `results.json`; non-full runs use `results-smoke.*`.
The full artifacts are checked in as release evidence and include exact row
counts, per-query timings, aggregate speedup, and the gate outcome.

## Workload

| Query | Shape |
|---|---|
| Q1 | Full table `COUNT(*)` |
| Q2 | Filtered count |
| Q3 | Status count and average |
| Q4 | Region × status count and sum |
| Q5 | Monthly 2023 revenue |
| Q6 | Top ten users by spend |
| Q7 | Regional multi-metric analytics with distinct users |
| Q8 | Users/orders join and regional aggregation |

Every container, network, temporary Pintail directory, and anonymous test
artifact has a unique run identifier and is removed even when a query fails.
The script resolves published ports through the active Docker context, so it
works with the repository's remote-Docker setup as well as a local daemon.

## Independent auditor kit

On a clean Linux or macOS machine with Git, Bun and access to a Linux Docker
Engine, clone this repository and run from its root:

```sh
bun run scripts/audit-benchmark.ts --output /absolute/path/to/new-audit
```

The Docker daemon needs at least 8 CPUs, 24 GiB RAM and enough free disk for
three copies of the 20M-row workload plus build layers (allow 100 GiB). Use an
otherwise idle dedicated host for publishable numbers. The cold MySQL work
can take hours. Keep the machine awake until `AUDITOR-DONE` or failure.
`--check` checks prerequisites without starting containers. `--smoke` runs
20,000 orders with fewer samples to verify wiring; it cannot reproduce or
support published performance claims. `--ref <commit-or-tag>` pins a revision
that includes the auditor kit; the default is the checkout's HEAD.

The command makes an isolated clean checkout, installs locked benchmark
packages and runs the same queries, equal CPU/memory limits, correctness
comparisons and timing methodology described above. It always measures fresh
MySQL timings and creates a unique seed volume and Pintail image. Ordinary
completion and errors remove those resources and the temporary checkout.
Shared dependency images and Docker build cache remain reusable. A forced
machine shutdown may require removing resources with that run's unique
`pintail-m9-bench-...` prefix; never prune unrelated Docker resources.

The new output directory contains raw samples and reports plus
`provenance.json` with the source commit, machine capacity, architecture,
kernel, tool versions and full/smoke status. The Dockerfile at that commit
pins the Rust toolchain. `private-run.log` is troubleshooting output and can
contain local infrastructure details: inspect it before sharing. Publish the
JSON/Markdown reports and provenance; do not publish the private log blindly.
A failed run retains available fresh artifacts with `status: FAIL`, never
copies the checkout's historical reports as new evidence, and exits nonzero.
