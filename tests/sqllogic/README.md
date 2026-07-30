# SQL logic corpus

The ignored `mysql_oracle` integration test generates 600 deterministic
queries across scalar expressions, dates, constant subqueries, common table
expressions, table-reading subqueries, scans, sorting, aggregation, joins, and
`UNION ALL`. It executes the same statements through MySQL 8.4 and Pintail
over pinned storage snapshots, then compares normalized ordered rows.

The harness starts a uniquely named MySQL container, batches the queries
through one client process, and removes the container even when a comparison
fails.

Run the explicit Docker-backed gate with:

```sh
cargo test -p pintail-sqllogic --test mysql_oracle -- --ignored --nocapture
```

Run the physical pruning gate with:

```sh
cargo test -p pintail-sqllogic --test plan_quality
```
