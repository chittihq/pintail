# Pintail

Pintail is a columnar analytical database for MySQL, built from scratch in
Rust and distributed as one binary.

The project is under active development. Milestones M0 through M4 provide the
single-process skeleton, Pintail's durable PTSEG columnar storage core, and
the in-process MySQL-dialect query engine, plus real MySQL/MariaDB source
probing, resumable consistent snapshots, and native row-binlog CDC. Polling,
the external query APIs, and the MySQL wire endpoint arrive in later
milestones.

## Run locally

```sh
(cd packages/dashboard && bun install)
cargo run --release -- --data-dir ./data
```

Open <http://127.0.0.1:8080>. On the first boot, Pintail prints the generated
JWT and DSN-encryption secrets once. The JWT secret lives in the SQLite
`settings` table; the DSN key is saved in `./data/secrets.toml` with owner-only
permissions on Unix.

Configuration precedence is CLI arguments, `PINTAIL_*` environment variables,
`pintail.toml`, then defaults. See `pintail.example.toml`.

## Run with Docker Compose

```sh
docker compose up --build
```

The dashboard and health endpoint are then available on
<http://127.0.0.1:8080>, with durable state in the `pintail-data` volume.

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
```

The oracle starts a uniquely named MySQL 8.4 container and compares 600
generated and hand-written queries over equivalent nullable MySQL and Pintail
data. The M3 and M4 gates additionally run MySQL 8.4, MySQL 5.7, MariaDB 11,
and a binlog-disabled source. They snapshot one million rows, SIGKILL real
snapshot and CDC worker processes, verify restart replay, exercise GTID and
file/position CRUD plus MyISAM boundaries, quarantine a decode failure, and
purge the captured log before automatic resnapshot recovery. Current
compatibility boundaries are recorded in
[`docs/limitations.md`](docs/limitations.md).
