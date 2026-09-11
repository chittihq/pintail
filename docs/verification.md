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
(cd tests/browser && \
  bun install --frozen-lockfile && \
  bunx playwright install chromium && \
  bun run gate)
docker compose config --quiet
PINTAIL_HTTP_PORT=0 PINTAIL_WIRE_PORT=0 \
  docker compose --project-name pintail-release up --build --detach --wait
docker compose --project-name pintail-release exec --no-TTY pintail \
  sh -c 'id && curl --fail --silent http://127.0.0.1:8080/health'
docker compose --project-name pintail-release down --volumes
```

The oracle starts a uniquely named MySQL container and compares 1,897 generated
and hand-written queries over equivalent MySQL and Pintail data. MySQL 8.4 is
the default; `PINTAIL_ORACLE_MYSQL_IMAGE=mysql:8.0` runs the same corpus against
the older supported major. The fixed corpus has six layers:

1. **Parametric loops** (~557 cases) — small AST templates with a varying
   scalar; good regression bulk, low template entropy.
2. **Hand-written edges** (~265 cases) — windows, decimals, JSON, set ops,
   repaired review findings.
3. **Typed diversify cases** (40 cases) — multi-table `orders` seed with
   `DECIMAL` / `DATETIME` / `JSON` columns, joins against `users`, and
   column-native aggregates, windows, JSON extract, and temporal grains.
4. **Collation differential matrix** (12 cases) — Unicode equality, ordering,
   grouping, DISTINCT, joins, `IN`, extrema, and character-counted `LIKE`
. **Boundary fixture** (341 cases) — a `bounds` table at the limits of
   signed and unsigned integers, `DECIMAL(38)`, floats, padded and binary
   strings, and temporal types including negative `TIME` and fractional
   seconds, exercised through conversion, comparison and grouping contexts.
6. **Reviewed and minimized regressions** (22 cases) — seven reviewed
   candidate queries and fifteen seed-minimized mismatches, stored as JSON
   in `tests/sqllogic/tests/support/`.

Results compare as typed values: NULL never equals text, bytes stay bytes,
and floats get a tolerance only when both sides are floats. A case that
diverges through a documented limitation is listed in
`tests/sqllogic/tests/support/oracle_known_failures.json` with the
limitation it quotes. It warns while it fails; the run fails once it passes,
or if its quotation is no longer in `docs/limitations.md`.

The ignored differential fuzzer adds a generated layer on top. One grammar emits
sixteen balanced families—scalar, filters, grouping, two/three-table joins,
DECIMAL, temporal, JSON, ENUM, windows, conditionals/NULLs, strings, correlated
subqueries, derived set operations, numeric functions, and hashes/encodings.
It batches bounded groups through one real MySQL instance, treats every MySQL
rejection as a generator failure (never a skipped compatibility case), and
compares every ordered answer byte-for-byte. Multiple comma-separated seeds
and optional SQL export support large reproducible sweeps:

```sh
PINTAIL_ORACLE_MYSQL_IMAGE=mysql:8.4 \
PINTAIL_FUZZ_CASES=5000 \
PINTAIL_FUZZ_SEEDS=0xDEADBEEF,0xC0FFEE,0xA11CE,0x5EED,0xBAD5EED,0x123456789,0xFACEFEED,0x31415926,0x27182818,0xFEEDBEEF \
PINTAIL_FUZZ_CORPUS_PATH=/tmp/pintail-fuzz.sql \
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test \
  -p pintail-sqllogic --test mysql_oracle \
  fuzzes_against_configured_mysql -- --ignored --nocapture
```

The dockerless metamorphic pack reuses the same grammar for tautology,
double-negation, HAVING, join-commutation, derived-table, empty-UNION, and
zero-offset LIMIT equivalences. The latest high-volume evidence is recorded in
[`tests/sqllogic/fuzz-results.md`](../tests/sqllogic/fuzz-results.md).

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

Queries under live replication have a generated layer in the unit stage:
`crates/pintail-exec/tests/live_replication_queries.rs` runs a seeded
sequence of change batches (inserts past the end and into holes left in the
key space, updates that move a collated group's first-seen spelling,
deletes, dimension-only changes), insert-only batches after a flush (the
aggregate memo's delta path), stale replays of strictly older versions for
flushed keys, batches hugging direct slice boundaries, flushes and
compactions over a fact and a dimension table. After every step it checks
aggregates, a collated `GROUP BY` whose representative must be the lowest
key's spelling, full rows for live, deleted and absent keys, a key range,
a filter-first predicate with a third column projected, a nullable column
under an ordered limit, a limit pushed into the scan, extremes, a join on
a non-key column and one on the storage key, and the scan's key order
against an in-memory model; every fifth step compares every row and pins a
reader across a change, a flush and a compaction. Each scenario the
generator claims is counted and required. Three fixtures run: below the
streaming threshold, above it, and across several direct slices. The
settled memo stays on, as in production; failures name the fixture, seed,
step and the last operations.

The store's recovery has two generated layers of its own, both in the unit
gate. The crash fuzz (`crates/pintail-store/tests/crash_fuzz.rs`) kills a
fixed write loop at a random moment and checks the reopened tables against
the acknowledged-commit oracle. The recovery sequences
(`crates/pintail-store/tests/recovery_sequences.rs`) generate the shape of
the run as well as the moment: random interleavings of versioned writes and
tombstones, flushes, compactions, reclaims, checkpoints, ADD COLUMN,
at-least-once replays and one process abort, checked against an in-memory
model after the crash, after replaying the tail into the restarted table,
after the rest of the sequence and after a clean reopen. A failing sequence
is shrunk to the shortest one that still fails and printed one op per line;
`PINTAIL_RECOVERY_SEQUENCE_FILE` replays such a file, and
`PINTAIL_RECOVERY_SEQUENCE_SEED` moves the seed range. A `failpoints` build
adds the same sequences with the abort inside a WAL write.

The end-to-end differential gate (`tests/e2e`) boots a real MySQL source
(image selectable via `PINTAIL_E2E_MYSQL_IMAGE`; 8.4 primary, 8.0 as a
second leg) under `binlog_row_metadata=MINIMAL` — MySQL's default — and the
release binary, registers the database through the HTTP API, and drives 21
workload phases (transactional CRUD with rollbacks, type edges, live DDL
including a mid-stream CREATE TABLE, schema drift under both metadata modes,
seeded churn with live queries, a contention storm racing reads against
DML, a SIGKILL restart with writes while the process is down, a
control-plane pass over the operator API routes, table/database drop
lifecycles, and documented-gap DDL). After every phase it re-verifies each
base table over the wire protocol plus the differential corpus in
`queries.ts` — 164 unique query shapes covering joins up to five tables,
windows, aggregates, subqueries, CTEs, set operations, JSON, temporal
grains, regex, SET/geometry contracts, and 21 BI-tool compilation shapes —
and an errno/SQLSTATE rejection matrix. A pinned Sequelize, Prisma, and
Drizzle matrix additionally compares generated read queries, decoded
results, and schema-introspection artifacts against MySQL. The headline
count in the banked ledger is checks across phases, not independent
behaviors: the same corpus replays after every settled phase. Documented
gaps report WARN. `E2E_PHASES` selects a subset while
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

Locally, `bun run scripts/validate.ts` drives the sequence as one
detached process — preflighting the shared Docker host (reachability,
free disk, leftover harness containers), running stages strictly in
order, retrying once on transient container-init races, aborting on
host-level failures like a full disk, and capturing crashed-container
logs before harness cleanup.

Which stages run is a **profile**, and a profile is the policy a run
claims to satisfy:

| profile | stages | claims |
|---|---|---|
| `development` | fmt, typecheck, unit | it compiles, lints, typechecks, and passes its unit tests |
| `rc` | + oracle, e2e, e2e-mysql80, browser | rc correctness gates passed, on both MySQL majors the release covers |
| `stable` | + recovery, bench, accept | the same, with the measured evidence regenerated |

`--profile rc` is the release-candidate gate; the bare command is
`--profile stable`. `--stages=…` still runs any subset, but a subset
reports itself as one: its verdict is `PASS (SUBSET)` and its report
names every stage that did not run, so a green `--stages=oracle` can
never be mistaken for a gate.

Two stages sit outside every profile. `freshness`
(`benchmark/check-evidence-freshness.ts`) asks whether the *banked*
evidence describes HEAD, so it can only pass after a run's artifacts are
committed — `scripts/release-chain.sh` runs it in the closing
`--stages=fmt,freshness,accept` pass, on the banked tree, and that pass
is the stable release's evidence gate. `soak` and `memsoak` are opt-in
by cost.

The stable profile runs `recovery` after `e2e-mysql80` and before `browser`.
It uses a separate feature-enabled binary and banks `tests/e2e/results-recovery.md`;
rc omits this additional fault matrix. For a development check, run
`bun run scripts/validate.ts --stages=fmt,typecheck,unit,recovery`; its verdict
is a subset, not a release gate.

Each run writes its own directory under `validate-out/runs/`, recording
HEAD, the toolchain versions, the requested stages and the ones that did
not run. `validate-out/latest` points at the newest run,
`validate-out/latest-complete` at the newest one that finished a whole
profile, and `validate-out/validate-report.md` is a stub naming both.
Progress streams to the run's `status.log` and to
`validate-out/validate-status.log`.

CI runs these gates automatically on GitHub-hosted runners with no external
infrastructure: `.github/workflows/e2e.yml` gives every push and pull
request a three-phase e2e smoke plus the browser dashboard walkthrough and
runs the full eight-phase gate nightly,
and `.github/workflows/compat.yml` runs the Docker-gated compatibility
suites (CDC against MySQL 8.4 GTID/file-position/MINIMAL-metadata and
MariaDB 11, snapshot and polling sources, the control-plane API suite,
MinIO backup restore, and the wire-protocol client matrix) every night.
The e2e workflow also runs the browser smoke suite (`tests/browser`) on every
`dev`/`main` push and pull request, plus its nightly schedule and manual
dispatch. Headless Chromium walks the embedded dashboard through
first-boot operator setup, the add-database wizard against a live MySQL
source, replication reaching streaming, the SQL console returning typed
results, and a 390-pixel login render, capturing screenshots on failure.
