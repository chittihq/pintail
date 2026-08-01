# Release verification

The full gate sequence run before each milestone release.

## Gate sequence

```sh
(cd packages/dashboard && \
  bun install --frozen-lockfile && \
  bun run typecheck && \
  bun run generate)
cargo fmt --all -- --check
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo clippy --workspace --all-targets --all-features -- -D warnings
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test --workspace --all-targets --all-features --locked
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-sqllogic --test plan_quality
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-sqllogic --test mysql_oracle -- --ignored --nocapture
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-snapshot --test mysql_snapshot \
  m3_snapshot_basic_resume_type_fidelity_and_pk_matrix -- --ignored --nocapture
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-snapshot --test mysql_snapshot \
  snapshot_compatibility_matrix_covers_file_position_mariadb_and_polling_sources \
  -- --ignored --nocapture
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-cdc --test mysql_cdc \
  -- --ignored --nocapture --test-threads=1
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-poll --test mysql_poll \
  -- --ignored --nocapture
(cd tests/integration/wire-clients && bun install --frozen-lockfile)
PINTAIL_DASHBOARD_PREBUILT=1 \
PINTAIL_EXTERNAL_WIRE_CLIENTS=1 \
PINTAIL_MYSQL_CLI=/opt/homebrew/opt/mysql-client@8.4/bin/mysql \
  cargo test -p pintail-wire --test wire_compat -- --nocapture
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-backup --test minio_restore -- --ignored --nocapture
PINTAIL_DASHBOARD_PREBUILT=1 \
  cargo test -p pintail-api --test mysql_api \
  -- --ignored --nocapture --test-threads=1
docker compose config --quiet
PINTAIL_HTTP_PORT=0 PINTAIL_WIRE_PORT=0 \
  docker compose --project-name pintail-release up --build --detach --wait
docker compose --project-name pintail-release exec --no-TTY pintail \
  sh -c 'id && curl --fail --silent http://127.0.0.1:8080/health'
docker compose --project-name pintail-release down --volumes
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
and polling modes while one source fails. The M9 release matrix ran this
complete sequence twice consecutively, including all three ignored API gates,
production Compose build/health checks, and a real-browser
wizard→snapshot→streaming→typed-query flow at desktop and 390-pixel widths.
See the [`M9 release report`](docs/milestones/M9.md) for the recorded outcome.
Current compatibility boundaries are recorded in
[`docs/limitations.md`](docs/limitations.md).

