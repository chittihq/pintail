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
- **Timing**: Pintail and both ClickHouse variants report median of 5 warm
  runs (p95/min recorded in results.json); MySQL is a single cold run — it is
  the baseline being escaped, and its full-scale queries run for minutes.
- **Per-query EXPLAIN ANALYZE snapshots** land in results.json so segment and
  block pruning behavior is inspectable per run.
- **Host variance caveat**: the host may carry unrelated load; ClickHouse's
  own totals swing between runs, so cross-run comparisons should use the
  same-run pintail/ClickHouse ratio, not absolute times.

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
