# Integration suites

Container-driven replication, durability, wire, API, and backup suites live
here as their corresponding milestones are implemented. Every suite uses real
MySQL or MariaDB instances on the configured remote Docker host.

The M3 snapshot matrix lives beside its crate at
`crates/pintail-snapshot/tests/mysql_snapshot.rs` so it can exercise public
probe, metadata, store, and snapshot APIs directly. It covers MySQL 5.7/8.4,
MariaDB 11, GTID, file/position, binlog-disabled polling, one million rows,
SIGKILL resume, type fidelity, and the full snapshot key matrix.

The M4 CDC matrix likewise lives at
`crates/pintail-cdc/tests/mysql_cdc.rs`. Run its ignored cases serially so the
remote Docker host has predictable memory and port pressure:

```sh
cargo test -p pintail-cdc --test mysql_cdc \
  -- --ignored --nocapture --test-threads=1
```

It covers MySQL 8.4 GTID and file/position, MySQL 5.7, MariaDB 11 fallback,
transactional CRUD, MyISAM, GIPK, append replay, type fidelity, DLQ
continuation, real CDC-worker SIGKILL during paced writes, and
purge-to-automatic-resnapshot recovery.

M5 adds one DDL case to that file and a polling matrix at
`crates/pintail-poll/tests/mysql_poll.rs`:

```sh
cargo test -p pintail-cdc --test mysql_cdc \
  ddl_evolution_add_drop_rename_create_truncate_and_orphan \
  -- --ignored --nocapture
cargo test -p pintail-cdc --test mysql_cdc \
  cdc_cascade_negative_control_and_scheduled_repair \
  -- --ignored --nocapture
cargo test -p pintail-poll --test mysql_poll \
  -- --ignored --nocapture
```

The DDL gate tracks live ADD/DROP columns across restart, table-local rename
quarantine, TRUNCATE, matching CREATE auto-snapshot, and retained DROP orphans.
The CDC cascade gate first proves that InnoDB emits neither child delete nor
update row events, then repairs both through the scheduled full-row
reconciliation path without changing the CDC checkpoint. The binlog-disabled
polling gate covers cursor and cursor-less CRUD, composite keys, count-neutral
same-token changes, full delete repair, unique-value reuse, soft deletes,
append generations, and ten idle cycles with no row-storage growth. Later
end-to-end supervisor/API suites will compose these gates from this directory.

M7's wire compatibility gate lives at
`crates/pintail-wire/tests/wire_compat.rs`. Its default run uses a real Rust
MySQL client; set the external-client flag to add the `mysql` CLI, mysql2 under
Bun, PyMySQL, and Go's `database/sql` with go-sql-driver/mysql:

```sh
PINTAIL_EXTERNAL_WIRE_CLIENTS=1 \
PINTAIL_MYSQL_CLI=/opt/homebrew/opt/mysql-client@8.4/bin/mysql \
cargo test -p pintail-wire --test wire_compat -- --nocapture
```

`PINTAIL_MYSQL_CLI` is optional and defaults to `mysql`. MySQL 9.x clients no
longer ship the `mysql_native_password` client plugin, so use a MySQL 8.4 or
compatible MariaDB client binary for Pintail's hash-only native challenge
gate. JavaScript dependencies in `tests/integration/wire-clients` are locked
and installed with Bun. All three external clients replay
`tests/integration/wire-clients/metadata.sql`, a checked-in discovery corpus
covering the `SHOW`, index, column, view, alias, join, and aggregate shapes
used by MySQL CLI, DBeaver/Metabase-style inspectors, and ORMs. The GUI clients
themselves are not automated by this gate; their SQL shapes are replayed by
the deterministic protocol clients above.

The end-to-end differential gate also runs pinned read-only ORM clients against
the MySQL source and its converged Pintail replica. Sequelize exercises schema
discovery, and Drizzle runs `drizzle-kit pull`; both generate point, filtered,
relation, grouped, ordered, and paginated reads. The gate compares both
ORM-decoded values and normalized SQL; it never invokes synchronization,
migration, or mutation APIs.
