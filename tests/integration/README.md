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
purge-to-automatic-resnapshot recovery. Later end-to-end supervisor/API suites
will compose these gates from this directory.
