# SQL logic corpus

The ignored `mysql_oracle` integration test runs 1,293 deterministic queries
against MySQL 8.4 and Pintail over pinned storage snapshots. It compares
normalized rows in query order when ordering is specified, and as multisets
otherwise. Exact values compare byte-for-byte; floating-point results use the
harness's tolerance.

Coverage layers:

1. **Parametric loops** — scalar templates with varying inputs.
2. **Hand-written edges** — windows, decimals, JSON, set operations, SQL modes,
   collations, and optimizer regressions.
3. **Typed tables and interactions** — decimal, datetime, enum, and JSON
   columns, plus joins against nullable text fixtures.

The latest expansion adds 64 distinct query shapes across eight families:
NULL truth tables, outer joins with NULLs, empty/all-NULL aggregates,
conditional decimal aggregates, nullable window frames, aggregate subqueries,
calendar boundaries, and collation-sensitive expressions. Outer-join cases
exercise predicate placement in both `ON` and `WHERE`; window cases include
empty frames and NULL values beside out-of-partition defaults. Every multirow
case has a deterministic ordering or uses multiset comparison.

This corpus tests planner/executor semantics over snapshots. It does not by
itself prove snapshot ingestion, CDC, schema-change, or wire-protocol parity;
those paths need the separate end-to-end gates. The case count is a regression
inventory, not a claim of complete MySQL compatibility.

A non-Docker unit test (`documented_rejects_stay_explicit`) pins limitation
shapes that must fail closed. Inventory:

```sh
bun run scripts/oracle-coverage.ts
```

The harness starts a uniquely named MySQL container, batches the queries
through one client process, and removes the container even when a comparison
fails.

Run these commands on the configured build host. Run the fixed-corpus
Docker-backed gate with:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test -p pintail-sqllogic --test mysql_oracle matches_configured_mysql_for_fixed_corpus -- --ignored --nocapture
```

Run inventory / reject unit tests without Docker:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test -p pintail-sqllogic --test mysql_oracle
```

Run the physical pruning gate with:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test -p pintail-sqllogic --test plan_quality
```
