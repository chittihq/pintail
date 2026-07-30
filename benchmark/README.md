# Analytical benchmark

This is Pintail's deterministic port of Duckling's 20-million-order analytical
suite. One run creates isolated MySQL 8.4 and ClickHouse 25.8 containers on the
configured Docker host, snapshots the same MySQL tables into Pintail, verifies
row counts, and executes the same eight aggregate queries against all three
engines.

The release gate uses the sum of the eight measured query times and requires
Pintail to be at least 50 times faster than the source MySQL. ClickHouse is
reported as an aspirational reference and never affects the gate.

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
