# Analytical benchmark

This is Pintail's deterministic port of Duckling's 20-million-order analytical
suite. One run creates isolated MySQL 8.4 and ClickHouse 26.8 containers on the
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
  target. FINAL is charged here WITHOUT a live update tail: the snapshot is
  loaded once and fully merged before the timed queries, so the RMT+FINAL
  column is a lower bound on ClickHouse's merge-on-read cost and the ratio
  against it is the most favourable one ClickHouse can get (on static,
  fully-merged data the two ClickHouse columns converge). A phase that keeps
  updates and deletes flowing through MySQL during the timed queries, so
  both engines carry versions, is issue #31.
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
4 GiB per-query memory ceiling inside its 8 GiB container (the same figure
the production harness uses, and the one that produced the banked
evidence). Results are written to
`results.md` and `results.json`; non-full runs use `results-smoke.*`.
The full artifacts are checked in as release evidence and include exact row
counts, per-query timings, aggregate speedup, and the gate outcome.

## Cache study: both result caches, and freshness after ingestion

The canonical table times a Pintail settled-memo hit against a ClickHouse
execution with its query cache off: one engine's cache against the other's
execution. `BENCHMARK_CACHE_STUDY=1` adds three comparisons that keep the
two apart (`cache-study.ts`, issue #36). The memo-off execution track is
unchanged and still the engine-speed comparison.

- **Static, both caches warm.** Pintail's settled memo and ClickHouse's query
  cache on RMT+FINAL (`use_query_cache = 1`, `query_cache_ttl = 3600`,
  `query_cache_min_query_runs = 0`, `query_cache_min_query_duration = 0`;
  the server's `query_cache_*` limits are recorded in the artifact). Each
  query is warmed, then timed. Every timed answer is compared with MySQL and
  proved a hit: Pintail by `pintail_settled_memo_hits_total` on `/metrics`
  read around each answer, ClickHouse by `QueryCacheHits` in
  `system.query_log` per query id. A shape a cache does not serve (Pintail
  answers a bare `COUNT(*)` from table statistics, not the memo) is marked
  `not cached`. The same statement executed with the cache off stays beside
  it as the uncached reference.
- **Freshness after ingestion.** ClickHouse gets a versioned copy,
  `ReplacingMergeTree(_version, _is_deleted)` read with `final = 1`, the
  shape a CDC pipeline keeps. With both caches warm, one deterministic
  change to an invented key band of `orders` commits to MySQL: an insert of
  new keys, an update that moves rows to a new status, or a delete. The
  harness writes the same change into ClickHouse the moment MySQL commits,
  so ClickHouse's delay is a floor for any real pipeline; Pintail gets it
  through its own CDC. Both engines are then polled (every
  `BENCHMARK_CACHE_POLL_MS`, default 100 ms, for up to
  `BENCHMARK_CACHE_TIMEOUT_S`, default TTL + 60 s) with a count-and-total
  query, a group-by-status query and a control on the unchanged `users`
  table. Every answer is classified against MySQL's answer before and after
  the change, so a stale answer counts as stale, never as a fast correct
  one, and a query that never turns correct is a timeout. A row-level read
  of the changed keys, which no result cache serves, marks when each
  engine's data shows the change: an old answer before that point is
  replication delay, after it cache staleness. The first correct answer
  (the recomputation) and the answers after it are reported separately.
- **ClickHouse's two freshness modes.** Native TTL expiry
  (`BENCHMARK_CACHE_TTL_S`, default 60, ClickHouse's own default), and an
  explicit `SYSTEM DROP QUERY CACHE` issued once a cache-off read of the
  changed keys matches MySQL; the probes and the drop are charged as
  coordination cost. Each mode sees one insert, one update and one delete.

Before each change Pintail is polled until its memo serves the warm
queries again (`BENCHMARK_CACHE_SETTLE_S`, default 60): the memo only
answers over a settled table, so this waits for the previous change to
flush. The phase writes to the source, so it runs after every other track,
on a Pintail restarted with the memo on. Results land in `results.json`
under `cacheStudy` and in `results.md`; `render-readme-table.ts` adds the
README form when the artifact carries it. `PINTAIL_BENCHMARK_IMAGE=<tag>`
reuses an image already built from the commit instead of building one;
`BENCHMARK_CONCURRENCY=` (empty) skips the concurrency sweep.

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
