# Integration suites

Container-driven replication, durability, wire, API, and backup suites live
here as their corresponding milestones are implemented. Every suite uses real
MySQL or MariaDB instances on the configured remote Docker host.

The M3 snapshot matrix currently lives beside its crate at
`crates/pintail-snapshot/tests/mysql_snapshot.rs` so it can exercise public
probe, metadata, store, and snapshot APIs directly. It covers MySQL 5.7/8.4,
MariaDB 11, GTID, file/position, binlog-disabled polling, one million rows,
SIGKILL resume, type fidelity, and the full snapshot key matrix. Later
end-to-end supervisor/API suites will compose these gates from this directory.
