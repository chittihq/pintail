# Pintail

Pintail is a columnar analytical database for MySQL, built from scratch in
Rust and distributed as one binary.

The project is under active development. Milestones M0 through M8 provide the
single-process skeleton, Pintail's durable PTSEG columnar storage core, and
the in-process MySQL-dialect query engine, plus real MySQL/MariaDB source
probing, resumable consistent snapshots, native row-binlog CDC, live DDL
tracking, binlog-disabled polling with delete reconciliation, the authenticated
HTTP control plane and dashboard, a read-only MySQL wire endpoint,
independently supervised databases, Prometheus metrics, safe DLQ retry, and
native full/incremental S3-compatible backup and side-by-side restore.

## Quick start with Docker

```sh
docker compose up --build --detach
docker compose logs pintail
```

Open <http://127.0.0.1:8080>, create the first admin, then choose **Add
database**. Supply a MySQL DSN whose host is reachable from the container,
test and probe it, select the recommended CDC or polling mode, and start the
snapshot. The database becomes queryable when its state changes to
**Streaming** or **Polling**.

The first container boot prints the generated JWT and DSN-encryption secrets
once; preserve the `pintail-data` volume and restrict access to its logs. The
dashboard/API is published on port 8080 and the read-only MySQL endpoint on
3306. Override them with `PINTAIL_HTTP_PORT` and `PINTAIL_WIRE_PORT`.

## Run from source

```sh
cd packages/dashboard
bun install --frozen-lockfile
bun run generate
cd ../..
cargo run --release -- --data-dir ./data
```

Open <http://127.0.0.1:8080>. The JWT secret lives in the SQLite `settings`
table; the DSN key is saved in `./data/secrets.toml` with owner-only
permissions on Unix.

Configuration precedence is CLI arguments, `PINTAIL_*` environment variables,
`pintail.toml`, then defaults. See `pintail.example.toml`.
`--query-memory-limit-bytes`, `PINTAIL_QUERY_MEMORY_LIMIT_BYTES`, and
`[query].memory_limit_bytes` set the shared hard per-query memory ceiling.

The MySQL wire endpoint listens on `127.0.0.1:3306` by default. In the
dashboard, create a database API key with the `query` scope, then open
**Connect** for complete client snippets. The database name is both the MySQL
username and selected database; the API key is the password. For example:

```sh
MYSQL_PWD='pk_your_key' mysql \
  --protocol=tcp \
  --host=127.0.0.1 \
  --port=3306 \
  --user=analytics \
  --database=analytics
```

Use a MySQL 8.4 or compatible MariaDB CLI. Oracle's MySQL 9.x CLI no longer
ships the `mysql_native_password` client plugin used by Pintail's hash-only
challenge authentication. mysql2, PyMySQL, DBeaver, and Metabase remain
compatible.

## Develop the dashboard

The dashboard uses Bun:

```sh
cd packages/dashboard
bun install
bun run dev
```

`cargo build` runs `bun run generate` when dashboard inputs change, then embeds
the resulting static assets in the Rust binary. Set
`PINTAIL_DASHBOARD_PREBUILT=1` only when the output was generated immediately
before the Cargo invocation, as the container and CI builds do.

## Verify the current milestone

```sh
(cd packages/dashboard && bun install --frozen-lockfile)
(cd packages/dashboard && bun run typecheck)
(cd packages/dashboard && bun run generate)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p pintail-sqllogic --test plan_quality
cargo test -p pintail-sqllogic --test mysql_oracle -- --ignored --nocapture
cargo test -p pintail-snapshot --test mysql_snapshot \
  m3_snapshot_basic_resume_type_fidelity_and_pk_matrix -- --ignored --nocapture
cargo test -p pintail-snapshot --test mysql_snapshot \
  snapshot_compatibility_matrix_covers_file_position_mariadb_and_polling_sources \
  -- --ignored --nocapture
cargo test -p pintail-cdc --test mysql_cdc \
  -- --ignored --nocapture --test-threads=1
cargo test -p pintail-cdc --test mysql_cdc \
  ddl_evolution_add_drop_rename_create_truncate_and_orphan \
  -- --ignored --nocapture
cargo test -p pintail-cdc --test mysql_cdc \
  cdc_cascade_negative_control_and_scheduled_repair \
  -- --ignored --nocapture
cargo test -p pintail-poll --test mysql_poll \
  -- --ignored --nocapture
PINTAIL_EXTERNAL_WIRE_CLIENTS=1 \
PINTAIL_MYSQL_CLI=/opt/homebrew/opt/mysql-client@8.4/bin/mysql \
cargo test -p pintail-wire --test wire_compat -- --nocapture
cargo test -p pintail-backup --test minio_restore -- --ignored --nocapture
cargo test -p pintail-api --test mysql_api \
  three_database_supervisor_contains_one_source_failure \
  -- --ignored --exact --nocapture
```

The oracle starts a uniquely named MySQL 8.4 container and compares 600
generated and hand-written queries over equivalent nullable MySQL and Pintail
data. The M3 and M4 gates additionally run MySQL 8.4, MySQL 5.7, MariaDB 11,
and a binlog-disabled source. They snapshot one million rows, SIGKILL real
snapshot and CDC worker processes, verify restart replay, exercise GTID and
file/position CRUD plus MyISAM boundaries, quarantine a decode failure, and
purge the captured log before automatic resnapshot recovery. The M5 gates
add live ADD/DROP/RENAME/CREATE/TRUNCATE/DROP tracking, binlog-disabled CRUD,
same-token delete/insert repair, composite-key reconciliation, secondary
UNIQUE reuse, CDC-invisible cascades, and idle-cycle storage invariance.
The M7 wire gate additionally covers native challenge authentication, metadata
discovery, prepared statements, BI-style aggregates, read-only errors, and
typed binary results with a Rust client, MySQL CLI, mysql2 under Bun, and
PyMySQL. The M8 gates exercise a full/incremental/checksum-verified restore
against MinIO and three independently supervised MySQL sources in mixed CDC
and polling modes while one source fails.
Current compatibility boundaries are recorded in
[`docs/limitations.md`](docs/limitations.md).
