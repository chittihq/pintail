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
(cd tests/e2e && bun install --frozen-lockfile && bun run e2e)
docker compose config --quiet
PINTAIL_HTTP_PORT=0 PINTAIL_WIRE_PORT=0 \
  docker compose --project-name pintail-release up --build --detach --wait
docker compose --project-name pintail-release exec --no-TTY pintail \
  sh -c 'id && curl --fail --silent http://127.0.0.1:8080/health'
docker compose --project-name pintail-release down --volumes
```

The oracle starts a uniquely named MySQL 8.4 container and compares 862
generated and hand-written queries over equivalent MySQL and Pintail data.
The corpus has three layers:

1. **Parametric loops** (~557 cases) — small AST templates with a varying
   scalar; good regression bulk, low template entropy.
2. **Hand-written edges** (~265 cases) — windows, decimals, JSON, set ops,
   repaired review findings.
3. **Typed diversify cases** (40 cases) — multi-table `orders` seed with
   `DECIMAL` / `DATETIME` / `JSON` columns, joins against `users`, and
   column-native aggregates, windows, JSON extract, and temporal grains.

A separate non-Docker unit test (`documented_rejects_stay_explicit`) pins
twelve limitation shapes so they fail closed with an explicit error rather
than a plausible wrong answer. Inventory and function-gap ranking:

```sh
bun run scripts/oracle-coverage.ts
bun run scripts/function-surface.ts tests/corpus/bi-shapes.sql
```

Prefer unique SQL templates and typed-column coverage over raw case count
when judging diversity. Optional production BI capture and dual-engine
replay is documented under `tests/corpus/bi-captured/README.md` (not a
release requirement).

The end-to-end differential gate (`tests/e2e`) boots a real MySQL 8.4
source and the release binary, registers the database through the HTTP API,
and drives eight workload phases (transactional CRUD with rollbacks, type
edges, live DDL including a mid-stream CREATE TABLE, 400 seeded churn
operations with live queries every 100 operations, a SIGKILL restart with
writes while the process is down, a control-plane pass that exercises the
operator API routes — status, metrics, activity, mode switching, resync,
API-key lifecycle, and database create/update/delete — and documented-gap
DDL). After every phase it re-verifies each base table over the wire protocol
plus 47 differential query shapes covering joins, windows, aggregates,
subqueries, CTEs, set operations, JSON extract, and decimal averages. A pinned Sequelize, Prisma, and Drizzle
matrix additionally compares generated read queries, decoded results, and
schema-introspection artifacts against MySQL. The complete gate records 506
passing checks; that headline is checks across phases, not independent
behaviors. Documented gaps report WARN. `E2E_PHASES` selects a subset while
iterating, and `PINTAIL_E2E_BINARY` skips the release build. The M3 and M4 gates
additionally run MySQL 8.4, MySQL 5.7, MariaDB 11,
and a binlog-disabled source. They snapshot one million rows, SIGKILL real
snapshot and CDC worker processes, verify restart replay, exercise GTID and
file/position CRUD plus MyISAM boundaries, quarantine a decode failure, and
purge the captured log before automatic resnapshot recovery. The M5 gates
add live ADD/DROP/RENAME/CREATE/TRUNCATE/DROP tracking, binlog-disabled CRUD,
same-token delete/insert repair, composite-key reconciliation, secondary
UNIQUE reuse, CDC-invisible cascades, and idle-cycle storage invariance.
The M7 wire gate additionally covers native challenge authentication, metadata
discovery, prepared statements, BI-style aggregates, read-only errors, and
typed binary results with a Rust client, MySQL CLI, mysql2 under Bun, PyMySQL,
and Go `database/sql` with go-sql-driver/mysql parameter interpolation. The M8
gates exercise a full/incremental/checksum-verified restore
against MinIO and three independently supervised MySQL sources in mixed CDC
and polling modes while one source fails. The M9 release matrix ran this
complete sequence twice consecutively, including all three ignored API gates,
production Compose build/health checks, and a real-browser
wizard→snapshot→streaming→typed-query flow at desktop and 390-pixel widths.
See the [`M9 release report`](docs/milestones/M9.md) for the recorded outcome.
Current compatibility boundaries are recorded in
[`docs/limitations.md`](docs/limitations.md).

Locally, `bun run scripts/validate.ts` drives the full sequence as one
detached process — preflighting the shared Docker host (reachability,
free disk, leftover harness containers), running stages strictly in
order (fmt+clippy, unit, oracle, e2e, benchmark, acceptance), retrying
once on transient container-init races, aborting on host-level failures
like a full disk, and capturing crashed-container logs before harness
cleanup. Progress streams to `validate-out/validate-status.log`; the
verdict lands in `validate-out/validate-report.md`. Use
`--stages=fmt,unit,oracle` for the fast loop.

CI runs these gates automatically on GitHub-hosted runners with no external
infrastructure: `.github/workflows/e2e.yml` gives every push and pull
request a three-phase e2e smoke and runs the full eight-phase gate nightly,
and `.github/workflows/compat.yml` runs the Docker-gated compatibility
suites (CDC against MySQL 8.4 GTID/file-position/MINIMAL-metadata and
MariaDB 11, snapshot and polling sources, the control-plane API suite,
MinIO backup restore, and the wire-protocol client matrix) every night.
The nightly e2e workflow also runs the browser smoke suite
(`tests/browser`): headless Chromium walks the embedded dashboard through
first-boot operator setup, the add-database wizard against a live MySQL
source, replication reaching streaming, the SQL console returning typed
results, and a 390-pixel login render, capturing screenshots on failure.
