# Recovery suite — 2026-09-25T18:04:32.443Z

Verdict: **PASS**

HEAD: a6d5810257493274c5eb2549a9b16da42188a8ab; rustc 1.97.0 (2d8144b78 2026-07-07); Bun 1.3.14.
Working tree: clean. Binary: built from checkout (recovery profile); SHA-256: ef96843681f80f95dbc6226aa097695cab3115c286eabe266de3fb070c910e65.
Source: MySQL 8.4.11; ROW/FULL images; MINIMAL metadata; GTID. Seed: 953.
Checks: 692 PASS, 3 WARN, 0 FAIL.
Scenarios: 38/38 requested; 38 registered. Duration: 3.7 minutes.

Failure policy: stop after the first scenario failure.

| scenario | check | status | detail |
|---|---|---|---|
| baseline | contract | PASS | docs/design/recovery-suite.md §0 |
| baseline | converged:baseline | PASS |  |
| baseline | dlq:baseline | PASS | [] |
| baseline | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=13; rolled_back=1 |
| baseline | converged:after-recovery | PASS |  |
| baseline | dlq:after-recovery | PASS | [] |
| baseline | converged:after-live-writes | PASS |  |
| baseline | dlq:after-live-writes | PASS | [] |
| baseline | converged:after-second-restart | PASS |  |
| baseline | dlq:after-second-restart | PASS | [] |
| baseline | automatic:no-manual-repair-event | PASS |  |
| mode-cdc-poll-cdc | contract | PASS | crates/pintail-api/src/supervisor.rs: polling handoff |
| mode-cdc-poll-cdc | converged:baseline | PASS |  |
| mode-cdc-poll-cdc | dlq:baseline | PASS | [] |
| mode-cdc-poll-cdc | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_da0f8df3de687bee5b69e716dd780b71","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:01:00.486157954+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-cdc-poll-cdc | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-cdc-poll-cdc | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_da0f8df3de687bee5b69e716dd780b71","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-76","binlog_file":"mysql-bin.000003","binlog_pos":60014,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:00.843863085+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-cdc-poll-cdc | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-cdc-poll-cdc | handoff:automatic-event | PASS |  |
| mode-cdc-poll-cdc | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=40; rolled_back=4 |
| mode-cdc-poll-cdc | converged:after-recovery | PASS |  |
| mode-cdc-poll-cdc | dlq:after-recovery | PASS | [] |
| mode-cdc-poll-cdc | converged:after-live-writes | PASS |  |
| mode-cdc-poll-cdc | dlq:after-live-writes | PASS | [] |
| mode-cdc-poll-cdc | converged:after-second-restart | PASS |  |
| mode-cdc-poll-cdc | dlq:after-second-restart | PASS | [] |
| mode-cdc-poll-cdc | automatic:no-manual-repair-event | PASS |  |
| mode-handoff-abort | contract | PASS | crates/pintail-api/src/supervisor.rs: interrupted handoff |
| mode-handoff-abort | converged:baseline | PASS |  |
| mode-handoff-abort | dlq:baseline | PASS | [] |
| mode-handoff-abort | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_01b87c753adf37aa193abd4b99d1ed58","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:01:02.610412243+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-abort | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-handoff-abort | interrupts at supervisor.handoff.after_begin | PASS | failpoint supervisor.handoff.after_begin hit 1: aborting |
| mode-handoff-abort | durable-before-restart:fault-supervisor.handoff.after_begin | PASS | {"checkpoints":[{"db_id":"db_01b87c753adf37aa193abd4b99d1ed58","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:01:02.697804906+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-abort | churn:during-crash:supervisor.handoff.after_begin | PASS | commits=71→73 |
| mode-handoff-abort | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_01b87c753adf37aa193abd4b99d1ed58","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-168","binlog_file":"mysql-bin.000003","binlog_pos":153237,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:04.228638787+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-handoff-abort | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-handoff-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=81; rolled_back=8 |
| mode-handoff-abort | converged:after-recovery | PASS |  |
| mode-handoff-abort | dlq:after-recovery | PASS | [] |
| mode-handoff-abort | converged:after-live-writes | PASS |  |
| mode-handoff-abort | dlq:after-live-writes | PASS | [] |
| mode-handoff-abort | converged:after-second-restart | PASS |  |
| mode-handoff-abort | dlq:after-second-restart | PASS | [] |
| mode-handoff-abort | automatic:no-manual-repair-event | PASS |  |
| mode-handoff-snapshot-abort | contract | PASS | crates/pintail-api/src/supervisor.rs: interrupted snapshot recovery |
| mode-handoff-snapshot-abort | converged:baseline | PASS |  |
| mode-handoff-snapshot-abort | dlq:baseline | PASS | [] |
| mode-handoff-snapshot-abort | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_ef75ac63afdf6c8055d5bff6b85e6267","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:01:06.042450196+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-snapshot-abort | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-handoff-snapshot-abort | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| mode-handoff-snapshot-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_ef75ac63afdf6c8055d5bff6b85e6267","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-223","binlog_file":"mysql-bin.000003","binlog_pos":207119,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:06.391494885+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| mode-handoff-snapshot-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=71→74 |
| mode-handoff-snapshot-abort | durable-before-restart:mode-handoff-copy | PASS | {"checkpoints":[{"db_id":"db_ef75ac63afdf6c8055d5bff6b85e6267","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-223","binlog_file":"mysql-bin.000003","binlog_pos":207119,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:06.391494885+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| mode-handoff-snapshot-abort | partial-copy:not-healthy:mode-handoff-copy | PASS |  |
| mode-handoff-snapshot-abort | partial-database:not-healthy:mode-handoff-copy | PASS |  |
| mode-handoff-snapshot-abort | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_ef75ac63afdf6c8055d5bff6b85e6267","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-262","binlog_file":"mysql-bin.000003","binlog_pos":252766,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:07.654195958+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-handoff-snapshot-abort | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-handoff-snapshot-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=81; rolled_back=8 |
| mode-handoff-snapshot-abort | converged:after-recovery | PASS |  |
| mode-handoff-snapshot-abort | dlq:after-recovery | PASS | [] |
| mode-handoff-snapshot-abort | converged:after-live-writes | PASS |  |
| mode-handoff-snapshot-abort | dlq:after-live-writes | PASS | [] |
| mode-handoff-snapshot-abort | converged:after-second-restart | PASS |  |
| mode-handoff-snapshot-abort | dlq:after-second-restart | PASS | [] |
| mode-handoff-snapshot-abort | automatic:no-manual-repair-event | PASS |  |
| mode-poll-during-cdc-lag | contract | PASS | crates/pintail-api/src/supervisor.rs: fresh handoff preserves polling-era writes |
| mode-poll-during-cdc-lag | converged:baseline | PASS |  |
| mode-poll-during-cdc-lag | dlq:baseline | PASS | [] |
| mode-poll-during-cdc-lag | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_a90a8bdec2fb8a23ebbe97ae66a0b470","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:01:09.834010510+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-poll-during-cdc-lag | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-poll-during-cdc-lag | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_a90a8bdec2fb8a23ebbe97ae66a0b470","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-330","binlog_file":"mysql-bin.000003","binlog_pos":321516,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:10.220152436+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-poll-during-cdc-lag | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-poll-during-cdc-lag | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=5 |
| mode-poll-during-cdc-lag | converged:after-recovery | PASS |  |
| mode-poll-during-cdc-lag | dlq:after-recovery | PASS | [] |
| mode-poll-during-cdc-lag | converged:after-live-writes | PASS |  |
| mode-poll-during-cdc-lag | dlq:after-live-writes | PASS | [] |
| mode-poll-during-cdc-lag | converged:after-second-restart | PASS |  |
| mode-poll-during-cdc-lag | dlq:after-second-restart | PASS | [] |
| mode-poll-during-cdc-lag | automatic:no-manual-repair-event | PASS |  |
| cdc-after-ingest | contract | PASS | crates/pintail-cdc/src/lib.rs: crate durability contract |
| cdc-after-ingest | converged:baseline | PASS |  |
| cdc-after-ingest | dlq:baseline | PASS | [] |
| cdc-after-ingest | converged:before-fault | PASS |  |
| cdc-after-ingest | dlq:before-fault | PASS | [] |
| cdc-after-ingest | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_33e7f2770132837f32da945e9f3cbbf9","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-357","binlog_file":"mysql-bin.000003","binlog_pos":343265,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:11.743706830+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | fixture:single-witness-transaction | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:358 |
| cdc-after-ingest | interrupts at cdc.after_ingest | PASS | failpoint cdc.after_ingest hit 1: aborting |
| cdc-after-ingest | durable-before-restart:fault-cdc.after_ingest | PASS | {"checkpoints":[{"db_id":"db_33e7f2770132837f32da945e9f3cbbf9","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-357","binlog_file":"mysql-bin.000003","binlog_pos":343265,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:11.743706830+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | churn:during-crash:cdc.after_ingest | PASS | commits=52→55 |
| cdc-after-ingest | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_33e7f2770132837f32da945e9f3cbbf9","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-357","binlog_file":"mysql-bin.000003","binlog_pos":343265,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:11.743706830+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:358 |
| cdc-after-ingest | checkpoint:previous-transaction-retained | PASS | {"before":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-357","after":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-357"} |
| cdc-after-ingest | checkpoint:belongs-to-source-history | PASS |  |
| cdc-after-ingest | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=59; rolled_back=6 |
| cdc-after-ingest | converged:after-recovery | PASS |  |
| cdc-after-ingest | dlq:after-recovery | PASS | [] |
| cdc-after-ingest | converged:after-live-writes | PASS |  |
| cdc-after-ingest | dlq:after-live-writes | PASS | [] |
| cdc-after-ingest | converged:after-second-restart | PASS |  |
| cdc-after-ingest | dlq:after-second-restart | PASS | [] |
| cdc-after-ingest | automatic:no-manual-repair-event | PASS |  |
| cdc-after-first-table-sync | contract | PASS | crates/pintail-cdc/src/lib.rs: crate durability contract |
| cdc-after-first-table-sync | converged:baseline | PASS |  |
| cdc-after-first-table-sync | dlq:baseline | PASS | [] |
| cdc-after-first-table-sync | converged:before-fault | PASS |  |
| cdc-after-first-table-sync | dlq:before-fault | PASS | [] |
| cdc-after-first-table-sync | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_43c161e2df1b8c15f765d87221471bdd","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-428","binlog_file":"mysql-bin.000003","binlog_pos":413710,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:14.669390690+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | fixture:single-witness-transaction | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:429 |
| cdc-after-first-table-sync | interrupts at cdc.after_first_table_sync | PASS | failpoint cdc.after_first_table_sync hit 1: aborting |
| cdc-after-first-table-sync | durable-before-restart:fault-cdc.after_first_table_sync | PASS | {"checkpoints":[{"db_id":"db_43c161e2df1b8c15f765d87221471bdd","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-428","binlog_file":"mysql-bin.000003","binlog_pos":413710,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:14.669390690+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | churn:during-crash:cdc.after_first_table_sync | PASS | commits=52→55 |
| cdc-after-first-table-sync | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_43c161e2df1b8c15f765d87221471bdd","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-428","binlog_file":"mysql-bin.000003","binlog_pos":413710,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:14.669390690+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:429 |
| cdc-after-first-table-sync | checkpoint:previous-transaction-retained | PASS | {"before":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-428","after":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-428"} |
| cdc-after-first-table-sync | checkpoint:belongs-to-source-history | PASS |  |
| cdc-after-first-table-sync | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=58; rolled_back=6 |
| cdc-after-first-table-sync | converged:after-recovery | PASS |  |
| cdc-after-first-table-sync | dlq:after-recovery | PASS | [] |
| cdc-after-first-table-sync | converged:after-live-writes | PASS |  |
| cdc-after-first-table-sync | dlq:after-live-writes | PASS | [] |
| cdc-after-first-table-sync | converged:after-second-restart | PASS |  |
| cdc-after-first-table-sync | dlq:after-second-restart | PASS | [] |
| cdc-after-first-table-sync | automatic:no-manual-repair-event | PASS |  |
| cdc-before-checkpoint-commit | contract | PASS | crates/pintail-cdc/src/lib.rs: crate durability contract |
| cdc-before-checkpoint-commit | converged:baseline | PASS |  |
| cdc-before-checkpoint-commit | dlq:baseline | PASS | [] |
| cdc-before-checkpoint-commit | converged:before-fault | PASS |  |
| cdc-before-checkpoint-commit | dlq:before-fault | PASS | [] |
| cdc-before-checkpoint-commit | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_66cc233a908e836026b1d9a87021ff50","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-498","binlog_file":"mysql-bin.000003","binlog_pos":486315,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:17.567329378+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | fixture:single-witness-transaction | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:499 |
| cdc-before-checkpoint-commit | interrupts at cdc.before_checkpoint_commit | PASS | failpoint cdc.before_checkpoint_commit hit 1: aborting |
| cdc-before-checkpoint-commit | durable-before-restart:fault-cdc.before_checkpoint_commit | PASS | {"checkpoints":[{"db_id":"db_66cc233a908e836026b1d9a87021ff50","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-498","binlog_file":"mysql-bin.000003","binlog_pos":486315,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:17.567329378+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | churn:during-crash:cdc.before_checkpoint_commit | PASS | commits=52→55 |
| cdc-before-checkpoint-commit | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_66cc233a908e836026b1d9a87021ff50","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-498","binlog_file":"mysql-bin.000003","binlog_pos":486315,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:17.567329378+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:499 |
| cdc-before-checkpoint-commit | checkpoint:previous-transaction-retained | PASS | {"before":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-498","after":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-498"} |
| cdc-before-checkpoint-commit | checkpoint:belongs-to-source-history | PASS |  |
| cdc-before-checkpoint-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=58; rolled_back=6 |
| cdc-before-checkpoint-commit | converged:after-recovery | PASS |  |
| cdc-before-checkpoint-commit | dlq:after-recovery | PASS | [] |
| cdc-before-checkpoint-commit | converged:after-live-writes | PASS |  |
| cdc-before-checkpoint-commit | dlq:after-live-writes | PASS | [] |
| cdc-before-checkpoint-commit | converged:after-second-restart | PASS |  |
| cdc-before-checkpoint-commit | dlq:after-second-restart | PASS | [] |
| cdc-before-checkpoint-commit | automatic:no-manual-repair-event | PASS |  |
| cdc-after-checkpoint-commit | contract | PASS | crates/pintail-cdc/src/lib.rs: crate durability contract |
| cdc-after-checkpoint-commit | converged:baseline | PASS |  |
| cdc-after-checkpoint-commit | dlq:baseline | PASS | [] |
| cdc-after-checkpoint-commit | converged:before-fault | PASS |  |
| cdc-after-checkpoint-commit | dlq:before-fault | PASS | [] |
| cdc-after-checkpoint-commit | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_a483ef30f28a6853e378580643c49e2d","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-568","binlog_file":"mysql-bin.000003","binlog_pos":559479,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:20.454928757+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | fixture:single-witness-transaction | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:569 |
| cdc-after-checkpoint-commit | interrupts at cdc.after_checkpoint_commit | PASS | failpoint cdc.after_checkpoint_commit hit 1: aborting |
| cdc-after-checkpoint-commit | durable-before-restart:fault-cdc.after_checkpoint_commit | PASS | {"checkpoints":[{"db_id":"db_a483ef30f28a6853e378580643c49e2d","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-569","binlog_file":"mysql-bin.000003","binlog_pos":560644,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:20.615709810+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | churn:during-crash:cdc.after_checkpoint_commit | PASS | commits=52→55 |
| cdc-after-checkpoint-commit | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_a483ef30f28a6853e378580643c49e2d","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-569","binlog_file":"mysql-bin.000003","binlog_pos":560644,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:20.615709810+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:569 |
| cdc-after-checkpoint-commit | checkpoint:advances-after-commit | PASS | {"before":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-568","after":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-569"} |
| cdc-after-checkpoint-commit | checkpoint:belongs-to-source-history | PASS |  |
| cdc-after-checkpoint-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=58; rolled_back=6 |
| cdc-after-checkpoint-commit | converged:after-recovery | PASS |  |
| cdc-after-checkpoint-commit | dlq:after-recovery | PASS | [] |
| cdc-after-checkpoint-commit | converged:after-live-writes | PASS |  |
| cdc-after-checkpoint-commit | dlq:after-live-writes | PASS | [] |
| cdc-after-checkpoint-commit | converged:after-second-restart | PASS |  |
| cdc-after-checkpoint-commit | dlq:after-second-restart | PASS | [] |
| cdc-after-checkpoint-commit | automatic:no-manual-repair-event | PASS |  |
| cdc-wal-before-sync | contract | PASS | crates/pintail-cdc/src/lib.rs: crate durability contract |
| cdc-wal-before-sync | converged:baseline | PASS |  |
| cdc-wal-before-sync | dlq:baseline | PASS | [] |
| cdc-wal-before-sync | converged:before-fault | PASS |  |
| cdc-wal-before-sync | dlq:before-fault | PASS | [] |
| cdc-wal-before-sync | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_af96426a0dfdc45c0e5f12718dc84f44","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631794,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:23.353673858+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | fixture:single-witness-transaction | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:639 |
| cdc-wal-before-sync | interrupts at store.wal.before_sync | PASS | failpoint store.wal.before_sync hit 1: aborting |
| cdc-wal-before-sync | durable-before-restart:fault-store.wal.before_sync | PASS | {"checkpoints":[{"db_id":"db_af96426a0dfdc45c0e5f12718dc84f44","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631794,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:23.353673858+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | churn:during-crash:store.wal.before_sync | PASS | commits=52→55 |
| cdc-wal-before-sync | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_af96426a0dfdc45c0e5f12718dc84f44","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631794,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:23.353673858+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:639 |
| cdc-wal-before-sync | checkpoint:previous-transaction-retained | PASS | {"before":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-638","after":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-638"} |
| cdc-wal-before-sync | checkpoint:belongs-to-source-history | PASS |  |
| cdc-wal-before-sync | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=58; rolled_back=6 |
| cdc-wal-before-sync | converged:after-recovery | PASS |  |
| cdc-wal-before-sync | dlq:after-recovery | PASS | [] |
| cdc-wal-before-sync | converged:after-live-writes | PASS |  |
| cdc-wal-before-sync | dlq:after-live-writes | PASS | [] |
| cdc-wal-before-sync | converged:after-second-restart | PASS |  |
| cdc-wal-before-sync | dlq:after-second-restart | PASS | [] |
| cdc-wal-before-sync | automatic:no-manual-repair-event | PASS |  |
| cdc-meta-commit-error | contract | PASS | crates/pintail-cdc/src/lib.rs: checkpoint commit after WAL synchronization |
| cdc-meta-commit-error | converged:baseline | PASS |  |
| cdc-meta-commit-error | dlq:baseline | PASS | [] |
| cdc-meta-commit-error | interrupts at meta.before_commit | PASS | failpoint meta.before_commit hit 1: error |
| cdc-meta-commit-error | metadata:error-visible | PASS |  |
| cdc-meta-commit-error | metadata:retries-without-restart | PASS |  |
| cdc-meta-commit-error | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=20; rolled_back=2 |
| cdc-meta-commit-error | converged:after-recovery | PASS |  |
| cdc-meta-commit-error | dlq:after-recovery | PASS | [] |
| cdc-meta-commit-error | converged:after-live-writes | PASS |  |
| cdc-meta-commit-error | dlq:after-live-writes | PASS | [] |
| cdc-meta-commit-error | converged:after-second-restart | PASS |  |
| cdc-meta-commit-error | dlq:after-second-restart | PASS | [] |
| cdc-meta-commit-error | automatic:no-manual-repair-event | PASS |  |
| purge-auto-resnapshot | contract | PASS | docs/limitations.md: automatic purge recovery |
| purge-auto-resnapshot | converged:baseline | PASS |  |
| purge-auto-resnapshot | dlq:baseline | PASS | [] |
| purge-auto-resnapshot | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_f43740d411a5cf31ef2ab8183cd01603","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-740","binlog_file":"mysql-bin.000003","binlog_pos":729274,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:27.625315144+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-auto-resnapshot | purge:required-file-is-gone | PASS |  |
| purge-auto-resnapshot | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_f43740d411a5cf31ef2ab8183cd01603 rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-auto-resnapshot | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=27; rolled_back=2 |
| purge-auto-resnapshot | converged:after-recovery | PASS |  |
| purge-auto-resnapshot | dlq:after-recovery | PASS | [] |
| purge-auto-resnapshot | converged:after-live-writes | PASS |  |
| purge-auto-resnapshot | dlq:after-live-writes | PASS | [] |
| purge-auto-resnapshot | converged:after-second-restart | PASS |  |
| purge-auto-resnapshot | dlq:after-second-restart | PASS | [] |
| purge-auto-resnapshot | automatic:no-manual-repair-event | PASS |  |
| purge-resnapshot-abort-once | contract | PASS | crates/pintail-api/src/supervisor.rs: interrupted copy recovery |
| purge-resnapshot-abort-once | converged:baseline | PASS |  |
| purge-resnapshot-abort-once | dlq:baseline | PASS | [] |
| purge-resnapshot-abort-once | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_944cb3bfbab82a892a6dd0a35afba26a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-779","binlog_file":"mysql-bin.000005","binlog_pos":20141,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:29.307280270+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-abort-once | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-abort-once | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_944cb3bfbab82a892a6dd0a35afba26a rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-abort-once | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| purge-resnapshot-abort-once | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_944cb3bfbab82a892a6dd0a35afba26a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-791","binlog_file":"mysql-bin.000007","binlog_pos":1363,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:29.694919822+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-once | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=58→61 |
| purge-resnapshot-abort-once | durable-before-restart:after-snapshot.chunk.after_ingest-2 | PASS | {"checkpoints":[{"db_id":"db_944cb3bfbab82a892a6dd0a35afba26a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-791","binlog_file":"mysql-bin.000007","binlog_pos":1363,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:29.694919822+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-once | partial-copy:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-once | partial-database:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-once | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=64; rolled_back=7 |
| purge-resnapshot-abort-once | converged:after-recovery | PASS |  |
| purge-resnapshot-abort-once | dlq:after-recovery | PASS | [] |
| purge-resnapshot-abort-once | converged:after-live-writes | PASS |  |
| purge-resnapshot-abort-once | dlq:after-live-writes | PASS | [] |
| purge-resnapshot-abort-once | converged:after-second-restart | PASS |  |
| purge-resnapshot-abort-once | dlq:after-second-restart | PASS | [] |
| purge-resnapshot-abort-once | automatic:no-manual-repair-event | PASS |  |
| purge-resnapshot-abort-twice | contract | PASS | docs/limitations.md: purge recovery once per runner invocation |
| purge-resnapshot-abort-twice | converged:baseline | PASS |  |
| purge-resnapshot-abort-twice | dlq:baseline | PASS | [] |
| purge-resnapshot-abort-twice | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_301b054cf63becb612d61b16aac0f59a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-855","binlog_file":"mysql-bin.000007","binlog_pos":67253,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:32.199015047+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-abort-twice | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-abort-twice | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_301b054cf63becb612d61b16aac0f59a rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-abort-twice | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| purge-resnapshot-abort-twice | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_301b054cf63becb612d61b16aac0f59a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-870","binlog_file":"mysql-bin.000009","binlog_pos":1369,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:32.696249742+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=61→63 |
| purge-resnapshot-abort-twice | durable-before-restart:after-snapshot.chunk.after_ingest-2 | PASS | {"checkpoints":[{"db_id":"db_301b054cf63becb612d61b16aac0f59a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-870","binlog_file":"mysql-bin.000009","binlog_pos":1369,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:32.696249742+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | partial-copy:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-twice | partial-database:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-twice | interrupts at snapshot.table.before_complete | PASS | failpoint snapshot.table.before_complete hit 1: aborting |
| purge-resnapshot-abort-twice | durable-before-restart:fault-snapshot.table.before_complete | PASS | {"checkpoints":[{"db_id":"db_301b054cf63becb612d61b16aac0f59a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-910","binlog_file":"mysql-bin.000009","binlog_pos":48304,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:33.993037900+00:00"}],"tables":[{"name":"accounts","state":"snapshotting"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | churn:during-crash:snapshot.table.before_complete | PASS | commits=101→105 |
| purge-resnapshot-abort-twice | durable-before-restart:after-snapshot.table.before_complete | PASS | {"checkpoints":[{"db_id":"db_301b054cf63becb612d61b16aac0f59a","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-910","binlog_file":"mysql-bin.000009","binlog_pos":48304,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:33.993037900+00:00"}],"tables":[{"name":"accounts","state":"snapshotting"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | partial-copy:not-healthy:after-snapshot.table.before_complete | PASS |  |
| purge-resnapshot-abort-twice | partial-database:not-healthy:after-snapshot.table.before_complete | PASS |  |
| purge-resnapshot-abort-twice | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=108; rolled_back=11 |
| purge-resnapshot-abort-twice | converged:after-recovery | PASS |  |
| purge-resnapshot-abort-twice | dlq:after-recovery | PASS | [] |
| purge-resnapshot-abort-twice | converged:after-live-writes | PASS |  |
| purge-resnapshot-abort-twice | dlq:after-live-writes | PASS | [] |
| purge-resnapshot-abort-twice | converged:after-second-restart | PASS |  |
| purge-resnapshot-abort-twice | dlq:after-second-restart | PASS | [] |
| purge-resnapshot-abort-twice | automatic:no-manual-repair-event | PASS |  |
| purge-resnapshot-position-abort | contract | PASS | crates/pintail-cdc/src/lib.rs: durable resnapshot handoff |
| purge-resnapshot-position-abort | converged:baseline | PASS |  |
| purge-resnapshot-position-abort | dlq:baseline | PASS | [] |
| purge-resnapshot-position-abort | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_21c97a5420261dd5995b48df67034eeb","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-975","binlog_file":"mysql-bin.000009","binlog_pos":115892,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:36.481133243+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-position-abort | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_21c97a5420261dd5995b48df67034eeb rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-position-abort | interrupts at cdc.resnapshot.after_targets | PASS | failpoint cdc.resnapshot.after_targets hit 1: aborting |
| purge-resnapshot-position-abort | durable-before-restart:fault-cdc.resnapshot.after_targets | PASS | {"checkpoints":[{"db_id":"db_21c97a5420261dd5995b48df67034eeb","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-990","binlog_file":"mysql-bin.000011","binlog_pos":1387,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:36.975639733+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | churn:during-crash:cdc.resnapshot.after_targets | PASS | commits=61→63 |
| purge-resnapshot-position-abort | durable-before-restart:after-cdc.resnapshot.after_targets | PASS | {"checkpoints":[{"db_id":"db_21c97a5420261dd5995b48df67034eeb","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-990","binlog_file":"mysql-bin.000011","binlog_pos":1387,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:36.975639733+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | partial-copy:not-healthy:after-cdc.resnapshot.after_targets | PASS |  |
| purge-resnapshot-position-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=67; rolled_back=7 |
| purge-resnapshot-position-abort | converged:after-recovery | PASS |  |
| purge-resnapshot-position-abort | dlq:after-recovery | PASS | [] |
| purge-resnapshot-position-abort | converged:after-live-writes | PASS |  |
| purge-resnapshot-position-abort | dlq:after-live-writes | PASS | [] |
| purge-resnapshot-position-abort | converged:after-second-restart | PASS |  |
| purge-resnapshot-position-abort | dlq:after-second-restart | PASS | [] |
| purge-resnapshot-position-abort | automatic:no-manual-repair-event | PASS |  |
| repair-alter-add-column | contract | PASS | docs/limitations.md: ADD COLUMN evolves schema |
| repair-alter-add-column | converged:baseline | PASS |  |
| repair-alter-add-column | dlq:baseline | PASS | [] |
| repair-alter-add-column | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_9245850a3a9206c6a899e07c36286b09","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1052","binlog_file":"mysql-bin.000011","binlog_pos":5487881,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:41.255213193+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-alter-add-column | purge:required-file-is-gone | PASS |  |
| repair-alter-add-column | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-alter-add-column | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_9245850a3a9206c6a899e07c36286b09","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1071","binlog_file":"mysql-bin.000013","binlog_pos":1339,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:41.875530701+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-alter-add-column | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=63→67 |
| repair-alter-add-column | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_9245850a3a9206c6a899e07c36286b09","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1071","binlog_file":"mysql-bin.000013","binlog_pos":1339,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:41.875530701+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-alter-add-column | partial-copy:not-healthy:schema-window | PASS |  |
| repair-alter-add-column | partial-database:not-healthy:schema-window | PASS |  |
| repair-alter-add-column | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=71; rolled_back=7 |
| repair-alter-add-column | converged:after-recovery | PASS |  |
| repair-alter-add-column | dlq:after-recovery | PASS | [] |
| repair-alter-add-column | converged:after-live-writes | PASS |  |
| repair-alter-add-column | dlq:after-live-writes | PASS | [] |
| repair-alter-add-column | converged:after-second-restart | PASS |  |
| repair-alter-add-column | dlq:after-second-restart | PASS | [] |
| repair-alter-add-column | automatic:no-manual-repair-event | PASS |  |
| repair-truncate | contract | PASS | docs/limitations.md: TRUNCATE replaces generation |
| repair-truncate | converged:baseline | PASS |  |
| repair-truncate | dlq:baseline | PASS | [] |
| repair-truncate | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_b358f6bc44e8c71f77ab76390c8e1ee0","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1146","binlog_file":"mysql-bin.000013","binlog_pos":5493510,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:49.642154207+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-truncate | purge:required-file-is-gone | PASS |  |
| repair-truncate | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-truncate | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_b358f6bc44e8c71f77ab76390c8e1ee0","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1161","binlog_file":"mysql-bin.000015","binlog_pos":1291,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:50.143615137+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-truncate | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=61→64 |
| repair-truncate | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_b358f6bc44e8c71f77ab76390c8e1ee0","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1161","binlog_file":"mysql-bin.000015","binlog_pos":1291,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:50.143615137+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-truncate | partial-copy:not-healthy:schema-window | PASS |  |
| repair-truncate | partial-database:not-healthy:schema-window | PASS |  |
| repair-truncate | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=68; rolled_back=7 |
| repair-truncate | converged:after-recovery | PASS |  |
| repair-truncate | dlq:after-recovery | PASS | [] |
| repair-truncate | converged:after-live-writes | PASS |  |
| repair-truncate | dlq:after-live-writes | PASS | [] |
| repair-truncate | converged:after-second-restart | PASS |  |
| repair-truncate | dlq:after-second-restart | PASS | [] |
| repair-truncate | automatic:no-manual-repair-event | PASS |  |
| repair-drop-recreate | contract | PASS | docs/limitations.md: DROP and recreated table identity |
| repair-drop-recreate | converged:baseline | PASS |  |
| repair-drop-recreate | dlq:baseline | PASS | [] |
| repair-drop-recreate | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_c0bc8d7ea5f2231ba549b15414c260df","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1235","binlog_file":"mysql-bin.000015","binlog_pos":5492209,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:54.563943861+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-drop-recreate | purge:required-file-is-gone | PASS |  |
| repair-drop-recreate | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-drop-recreate | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_c0bc8d7ea5f2231ba549b15414c260df","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1251","binlog_file":"mysql-bin.000017","binlog_pos":1321,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:55.071297835+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-drop-recreate | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=62→65 |
| repair-drop-recreate | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_c0bc8d7ea5f2231ba549b15414c260df","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1251","binlog_file":"mysql-bin.000017","binlog_pos":1321,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:55.071297835+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-drop-recreate | partial-copy:not-healthy:schema-window | PASS |  |
| repair-drop-recreate | partial-database:not-healthy:schema-window | PASS |  |
| repair-drop-recreate | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=69; rolled_back=7 |
| repair-drop-recreate | converged:after-recovery | PASS |  |
| repair-drop-recreate | dlq:after-recovery | PASS | [] |
| repair-drop-recreate | converged:after-live-writes | PASS |  |
| repair-drop-recreate | dlq:after-live-writes | PASS | [] |
| repair-drop-recreate | converged:after-second-restart | PASS |  |
| repair-drop-recreate | dlq:after-second-restart | PASS | [] |
| repair-drop-recreate | automatic:no-manual-repair-event | PASS |  |
| repair-rename | contract | PASS | docs/limitations.md: a name renamed away mid-copy is retained as a dropped table |
| repair-rename | converged:baseline | PASS |  |
| repair-rename | dlq:baseline | PASS | [] |
| repair-rename | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_701fcf2653ec92c26fb25d1c8c4a09f3","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1328","binlog_file":"mysql-bin.000017","binlog_pos":5493925,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:59.453054389+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-rename | purge:required-file-is-gone | PASS |  |
| repair-rename | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-rename | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_701fcf2653ec92c26fb25d1c8c4a09f3","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1343","binlog_file":"mysql-bin.000019","binlog_pos":1279,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:59.954366273+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-rename | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=61→64 |
| repair-rename | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_701fcf2653ec92c26fb25d1c8c4a09f3","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1343","binlog_file":"mysql-bin.000019","binlog_pos":1279,"poll_cursors_json":null,"updated_at":"2026-09-25T18:01:59.954366273+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-rename | partial-copy:not-healthy:schema-window | PASS |  |
| repair-rename | partial-database:not-healthy:schema-window | PASS |  |
| repair-rename | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=68; rolled_back=7 |
| repair-rename | converged:after-recovery | PASS |  |
| repair-rename | dlq:after-recovery | PASS | [] |
| repair-rename | documented-state-gap:after-recovery | WARN | big: excluded; docs/limitations.md: the old name of a table renamed mid-copy is retained as a dropped table, never streamed or served. All source rows and columns compared exactly. |
| repair-rename | converged:after-live-writes | PASS |  |
| repair-rename | dlq:after-live-writes | PASS | [] |
| repair-rename | documented-state-gap:after-live-writes | WARN | big: excluded; docs/limitations.md: the old name of a table renamed mid-copy is retained as a dropped table, never streamed or served. All source rows and columns compared exactly. |
| repair-rename | converged:after-second-restart | PASS |  |
| repair-rename | dlq:after-second-restart | PASS | [] |
| repair-rename | documented-state-gap:after-second-restart | WARN | big: excluded; docs/limitations.md: the old name of a table renamed mid-copy is retained as a dropped table, never streamed or served. All source rows and columns compared exactly. |
| repair-rename | durable-before-restart:documented-state-gap | PASS | {"checkpoints":[{"db_id":"db_701fcf2653ec92c26fb25d1c8c4a09f3","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-1401","binlog_file":"mysql-bin.000019","binlog_pos":58147,"poll_cursors_json":null,"updated_at":"2026-09-25T18:02:02.589114920+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"excluded"},{"name":"big2","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-rename | gap:old-name-is-not-a-complete-copy | PASS |  |
| repair-rename | automatic:no-manual-repair-event | PASS |  |
| reconcile-alter | contract | PASS | docs/limitations.md: polling re-probe after DDL |
| reconcile-alter | converged:baseline | PASS |  |
| reconcile-alter | dlq:baseline | PASS | [] |
| reconcile-alter | interrupts at poll.reconcile.before_state_commit | PASS | failpoint poll.reconcile.before_state_commit hit 3: aborting |
| reconcile-alter | durable-before-restart:fault-poll.reconcile.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_daee70afb7fab940e914ecba07fadcbc","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:14.311202797+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"big","state":"polling"},{"name":"ledger","state":"polling"}]} |
| reconcile-alter | churn:during-crash:poll.reconcile.before_state_commit | PASS | commits=261→264 |
| reconcile-alter | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=268; rolled_back=29 |
| reconcile-alter | converged:after-recovery | PASS |  |
| reconcile-alter | dlq:after-recovery | PASS | [] |
| reconcile-alter | converged:after-live-writes | PASS |  |
| reconcile-alter | dlq:after-live-writes | PASS | [] |
| reconcile-alter | converged:after-second-restart | PASS |  |
| reconcile-alter | dlq:after-second-restart | PASS | [] |
| reconcile-alter | automatic:no-manual-repair-event | PASS |  |
| poll-after-ingest | contract | PASS | crates/pintail-poll/src/lib.rs: run_poll_cycle durability |
| poll-after-ingest | converged:baseline | PASS |  |
| poll-after-ingest | dlq:baseline | PASS | [] |
| poll-after-ingest | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_046312da471f1dafee5c8ffd0cfeb2a5","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:27.611501402+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | poll:cursor-strategy | PASS |  |
| poll-after-ingest | poll:checksum-strategy | PASS |  |
| poll-after-ingest | poll:keyless-fixture | PASS |  |
| poll-after-ingest | interrupts at poll.after_ingest | PASS | failpoint poll.after_ingest hit 3: aborting |
| poll-after-ingest | durable-before-restart:fault-poll.after_ingest | PASS | {"checkpoints":[{"db_id":"db_046312da471f1dafee5c8ffd0cfeb2a5","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:27.685340002+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | churn:during-crash:poll.after_ingest | PASS | commits=48→51 |
| poll-after-ingest | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_046312da471f1dafee5c8ffd0cfeb2a5","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:27.685340002+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | poll:retains-durable-poll-state | PASS |  |
| poll-after-ingest | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-after-ingest | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=6 |
| poll-after-ingest | converged:after-recovery | PASS |  |
| poll-after-ingest | dlq:after-recovery | PASS | [] |
| poll-after-ingest | converged:after-live-writes | PASS |  |
| poll-after-ingest | dlq:after-live-writes | PASS | [] |
| poll-after-ingest | converged:after-second-restart | PASS |  |
| poll-after-ingest | dlq:after-second-restart | PASS | [] |
| poll-after-ingest | automatic:no-manual-repair-event | PASS |  |
| poll-before-state-commit | contract | PASS | crates/pintail-poll/src/lib.rs: run_poll_cycle durability |
| poll-before-state-commit | converged:baseline | PASS |  |
| poll-before-state-commit | dlq:baseline | PASS | [] |
| poll-before-state-commit | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_8b06cc1473b680a13d0d2b7220b681a0","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:33.235830643+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | poll:cursor-strategy | PASS |  |
| poll-before-state-commit | poll:checksum-strategy | PASS |  |
| poll-before-state-commit | poll:keyless-fixture | PASS |  |
| poll-before-state-commit | interrupts at poll.before_state_commit | PASS | failpoint poll.before_state_commit hit 3: aborting |
| poll-before-state-commit | durable-before-restart:fault-poll.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_8b06cc1473b680a13d0d2b7220b681a0","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:33.299898716+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | churn:during-crash:poll.before_state_commit | PASS | commits=48→51 |
| poll-before-state-commit | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_8b06cc1473b680a13d0d2b7220b681a0","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:33.299898716+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | poll:retains-durable-poll-state | PASS |  |
| poll-before-state-commit | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-before-state-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=6 |
| poll-before-state-commit | converged:after-recovery | PASS |  |
| poll-before-state-commit | dlq:after-recovery | PASS | [] |
| poll-before-state-commit | converged:after-live-writes | PASS |  |
| poll-before-state-commit | dlq:after-live-writes | PASS | [] |
| poll-before-state-commit | converged:after-second-restart | PASS |  |
| poll-before-state-commit | dlq:after-second-restart | PASS | [] |
| poll-before-state-commit | automatic:no-manual-repair-event | PASS |  |
| poll-append-after-reset | contract | PASS | crates/pintail-poll/src/lib.rs: run_poll_cycle durability |
| poll-append-after-reset | converged:baseline | PASS |  |
| poll-append-after-reset | dlq:baseline | PASS | [] |
| poll-append-after-reset | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_422df0183c37ed6023e7bbb705563f19","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:38.839164387+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | poll:cursor-strategy | PASS |  |
| poll-append-after-reset | poll:checksum-strategy | PASS |  |
| poll-append-after-reset | poll:keyless-fixture | PASS |  |
| poll-append-after-reset | interrupts at poll.append.after_reset | PASS | failpoint poll.append.after_reset hit 1: aborting |
| poll-append-after-reset | durable-before-restart:fault-poll.append.after_reset | PASS | {"checkpoints":[{"db_id":"db_422df0183c37ed6023e7bbb705563f19","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:38.902956189+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | churn:during-crash:poll.append.after_reset | PASS | commits=48→51 |
| poll-append-after-reset | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_422df0183c37ed6023e7bbb705563f19","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:38.902956189+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | poll:retains-durable-poll-state | PASS |  |
| poll-append-after-reset | poll:interrupted-table-state-is-old | PASS | audit |
| poll-append-after-reset | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=6 |
| poll-append-after-reset | converged:after-recovery | PASS |  |
| poll-append-after-reset | dlq:after-recovery | PASS | [] |
| poll-append-after-reset | converged:after-live-writes | PASS |  |
| poll-append-after-reset | dlq:after-live-writes | PASS | [] |
| poll-append-after-reset | converged:after-second-restart | PASS |  |
| poll-append-after-reset | dlq:after-second-restart | PASS | [] |
| poll-append-after-reset | automatic:no-manual-repair-event | PASS |  |
| poll-checksum-before-chunk-commit | contract | PASS | crates/pintail-poll/src/lib.rs: run_poll_cycle durability |
| poll-checksum-before-chunk-commit | converged:baseline | PASS |  |
| poll-checksum-before-chunk-commit | dlq:baseline | PASS | [] |
| poll-checksum-before-chunk-commit | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_f0387cfdbdde32d8c37272013afb7803","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:44.441272785+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | poll:cursor-strategy | PASS |  |
| poll-checksum-before-chunk-commit | poll:checksum-strategy | PASS |  |
| poll-checksum-before-chunk-commit | poll:keyless-fixture | PASS |  |
| poll-checksum-before-chunk-commit | interrupts at poll.checksum.before_chunk_commit | PASS | failpoint poll.checksum.before_chunk_commit hit 1: aborting |
| poll-checksum-before-chunk-commit | durable-before-restart:fault-poll.checksum.before_chunk_commit | PASS | {"checkpoints":[{"db_id":"db_f0387cfdbdde32d8c37272013afb7803","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:44.524365921+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | churn:during-crash:poll.checksum.before_chunk_commit | PASS | commits=48→51 |
| poll-checksum-before-chunk-commit | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_f0387cfdbdde32d8c37272013afb7803","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:02:44.524365921+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | poll:retains-durable-poll-state | PASS |  |
| poll-checksum-before-chunk-commit | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-checksum-before-chunk-commit | poll:chunk-journal-is-old | PASS |  |
| poll-checksum-before-chunk-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=6 |
| poll-checksum-before-chunk-commit | converged:after-recovery | PASS |  |
| poll-checksum-before-chunk-commit | dlq:after-recovery | PASS | [] |
| poll-checksum-before-chunk-commit | converged:after-live-writes | PASS |  |
| poll-checksum-before-chunk-commit | dlq:after-live-writes | PASS | [] |
| poll-checksum-before-chunk-commit | converged:after-second-restart | PASS |  |
| poll-checksum-before-chunk-commit | dlq:after-second-restart | PASS | [] |
| poll-checksum-before-chunk-commit | automatic:no-manual-repair-event | PASS |  |
| poll-meta-commit-error | contract | PASS | crates/pintail-poll/src/lib.rs: atomic poll state |
| poll-meta-commit-error | converged:baseline | PASS |  |
| poll-meta-commit-error | dlq:baseline | PASS | [] |
| poll-meta-commit-error | interrupts at meta.before_commit | PASS | failpoint meta.before_commit hit 1: error |
| poll-meta-commit-error | metadata:error-visible | PASS |  |
| poll-meta-commit-error | metadata:retries-without-restart | PASS |  |
| poll-meta-commit-error | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=20; rolled_back=2 |
| poll-meta-commit-error | converged:after-recovery | PASS |  |
| poll-meta-commit-error | dlq:after-recovery | PASS | [] |
| poll-meta-commit-error | converged:after-live-writes | PASS |  |
| poll-meta-commit-error | dlq:after-live-writes | PASS | [] |
| poll-meta-commit-error | converged:after-second-restart | PASS |  |
| poll-meta-commit-error | dlq:after-second-restart | PASS | [] |
| poll-meta-commit-error | automatic:no-manual-repair-event | PASS |  |
| poll-timestamp-ties | contract | PASS | GOAL.md §9; docs/limitations.md DDL and polling |
| poll-timestamp-ties | converged:baseline | PASS |  |
| poll-timestamp-ties | dlq:baseline | PASS | [] |
| poll-timestamp-ties | ties:observed-cycle-0 | PASS |  |
| poll-timestamp-ties | ties:observed-cycle-1 | PASS |  |
| poll-timestamp-ties | ties:observed-cycle-2 | PASS |  |
| poll-timestamp-ties | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
| poll-timestamp-ties | converged:after-recovery | PASS |  |
| poll-timestamp-ties | dlq:after-recovery | PASS | [] |
| poll-timestamp-ties | converged:after-live-writes | PASS |  |
| poll-timestamp-ties | dlq:after-live-writes | PASS | [] |
| poll-timestamp-ties | converged:after-second-restart | PASS |  |
| poll-timestamp-ties | dlq:after-second-restart | PASS | [] |
| poll-timestamp-ties | automatic:no-manual-repair-event | PASS |  |
| poll-update-no-timestamp | contract | PASS | GOAL.md §9; docs/limitations.md DDL and polling |
| poll-update-no-timestamp | converged:baseline | PASS |  |
| poll-update-no-timestamp | dlq:baseline | PASS | [] |
| poll-update-no-timestamp | converged:before-unchanged-cursor | PASS |  |
| poll-update-no-timestamp | dlq:before-unchanged-cursor | PASS | [] |
| poll-update-no-timestamp | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
| poll-update-no-timestamp | converged:after-recovery | PASS |  |
| poll-update-no-timestamp | dlq:after-recovery | PASS | [] |
| poll-update-no-timestamp | converged:after-live-writes | PASS |  |
| poll-update-no-timestamp | dlq:after-live-writes | PASS | [] |
| poll-update-no-timestamp | converged:after-second-restart | PASS |  |
| poll-update-no-timestamp | dlq:after-second-restart | PASS | [] |
| poll-update-no-timestamp | automatic:no-manual-repair-event | PASS |  |
| poll-backdated-update | contract | PASS | GOAL.md §9; docs/limitations.md DDL and polling |
| poll-backdated-update | converged:baseline | PASS |  |
| poll-backdated-update | dlq:baseline | PASS | [] |
| poll-backdated-update | converged:before-backdated-cursor | PASS |  |
| poll-backdated-update | dlq:before-backdated-cursor | PASS | [] |
| poll-backdated-update | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
| poll-backdated-update | converged:after-recovery | PASS |  |
| poll-backdated-update | dlq:after-recovery | PASS | [] |
| poll-backdated-update | converged:after-live-writes | PASS |  |
| poll-backdated-update | dlq:after-live-writes | PASS | [] |
| poll-backdated-update | converged:after-second-restart | PASS |  |
| poll-backdated-update | dlq:after-second-restart | PASS | [] |
| poll-backdated-update | automatic:no-manual-repair-event | PASS |  |
| poll-delete-insert-neutral | contract | PASS | GOAL.md §9; docs/limitations.md DDL and polling |
| poll-delete-insert-neutral | converged:baseline | PASS |  |
| poll-delete-insert-neutral | dlq:baseline | PASS | [] |
| poll-delete-insert-neutral | converged:before-neutral-mutation | PASS |  |
| poll-delete-insert-neutral | dlq:before-neutral-mutation | PASS | [] |
| poll-delete-insert-neutral | neutral:mutation-during-pagination | PASS | SELECT `id`,`owner`,`balance`,`updated_at` FROM `rec_poll_delete_insert_neutral_c15b78`.`accounts` WHERE `updated_at` >= ? ORDER BY `updated_at`,`id` LIMIT 10000 OFFSET 10000 |
| poll-delete-insert-neutral | fixture:count-max-token-unchanged | PASS |  |
| poll-delete-insert-neutral | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
| poll-delete-insert-neutral | converged:after-recovery | PASS |  |
| poll-delete-insert-neutral | dlq:after-recovery | PASS | [] |
| poll-delete-insert-neutral | converged:after-live-writes | PASS |  |
| poll-delete-insert-neutral | dlq:after-live-writes | PASS | [] |
| poll-delete-insert-neutral | converged:after-second-restart | PASS |  |
| poll-delete-insert-neutral | dlq:after-second-restart | PASS | [] |
| poll-delete-insert-neutral | automatic:no-manual-repair-event | PASS |  |
| poll-keyless-dup-churn | contract | PASS | GOAL.md §9; docs/limitations.md DDL and polling |
| poll-keyless-dup-churn | converged:baseline | PASS |  |
| poll-keyless-dup-churn | dlq:baseline | PASS | [] |
| poll-keyless-dup-churn | converged:before-duplicate-delete | PASS |  |
| poll-keyless-dup-churn | dlq:before-duplicate-delete | PASS | [] |
| poll-keyless-dup-churn | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
| poll-keyless-dup-churn | converged:after-recovery | PASS |  |
| poll-keyless-dup-churn | dlq:after-recovery | PASS | [] |
| poll-keyless-dup-churn | converged:after-live-writes | PASS |  |
| poll-keyless-dup-churn | dlq:after-live-writes | PASS | [] |
| poll-keyless-dup-churn | converged:after-second-restart | PASS |  |
| poll-keyless-dup-churn | dlq:after-second-restart | PASS | [] |
| poll-keyless-dup-churn | automatic:no-manual-repair-event | PASS |  |
| outage-during-cdc | contract | PASS | crates/pintail-api/src/supervisor.rs: per-database failure containment |
| outage-during-cdc | converged:baseline | PASS |  |
| outage-during-cdc | dlq:baseline | PASS | [] |
| outage-during-cdc | bystander:live-through-outage-2 | PASS | source commits continued; exact prefix through 12 |
| outage-during-cdc | outage:0:error-visible | PASS |  |
| outage-during-cdc | bystander:live-through-outage-3 | PASS | source commits continued; exact prefix through 23 |
| outage-during-cdc | outage:0:source-writes-continue | PASS |  |
| outage-during-cdc | converged:outage-0-restored | PASS |  |
| outage-during-cdc | dlq:outage-0-restored | PASS | [] |
| outage-during-cdc | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=36; rolled_back=3 |
| outage-during-cdc | converged:after-recovery | PASS |  |
| outage-during-cdc | dlq:after-recovery | PASS | [] |
| outage-during-cdc | converged:after-live-writes | PASS |  |
| outage-during-cdc | dlq:after-live-writes | PASS | [] |
| outage-during-cdc | converged:after-second-restart | PASS |  |
| outage-during-cdc | dlq:after-second-restart | PASS | [] |
| outage-during-cdc | automatic:no-manual-repair-event | PASS |  |
| outage-repeated | contract | PASS | docs/design/recovery-suite.md §8: repeated failures and eventual catch-up |
| outage-repeated | converged:baseline | PASS |  |
| outage-repeated | dlq:baseline | PASS | [] |
| outage-repeated | bystander:live-through-outage-2 | PASS | source commits continued; exact prefix through 12 |
| outage-repeated | outage:0:error-visible | PASS |  |
| outage-repeated | bystander:live-through-outage-3 | PASS | source commits continued; exact prefix through 21 |
| outage-repeated | outage:0:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-0-restored | PASS |  |
| outage-repeated | dlq:outage-0-restored | PASS | [] |
| outage-repeated | outage:1:error-visible | PASS |  |
| outage-repeated | bystander:live-through-outage-4 | PASS | source commits continued; exact prefix through 37 |
| outage-repeated | outage:1:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-1-restored | PASS |  |
| outage-repeated | dlq:outage-1-restored | PASS | [] |
| outage-repeated | outage:2:error-visible | PASS |  |
| outage-repeated | bystander:live-through-outage-5 | PASS | source commits continued; exact prefix through 50 |
| outage-repeated | outage:2:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-2-restored | PASS |  |
| outage-repeated | dlq:outage-2-restored | PASS | [] |
| outage-repeated | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=69; rolled_back=7 |
| outage-repeated | converged:after-recovery | PASS |  |
| outage-repeated | dlq:after-recovery | PASS | [] |
| outage-repeated | converged:after-live-writes | PASS |  |
| outage-repeated | dlq:after-live-writes | PASS | [] |
| outage-repeated | converged:after-second-restart | PASS |  |
| outage-repeated | dlq:after-second-restart | PASS | [] |
| outage-repeated | automatic:no-manual-repair-event | PASS |  |
| outage-during-snapshot | contract | PASS | crates/pintail-api/src/supervisor.rs: interrupted snapshot recovery |
| outage-during-snapshot | converged:baseline | PASS |  |
| outage-during-snapshot | dlq:baseline | PASS | [] |
| outage-during-snapshot | bystander:live-through-outage-2 | PASS | source commits continued; exact prefix through 12 |
| outage-during-snapshot | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_856d1c7df478336641e7bce3c3cd4aa4","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-2379","binlog_file":"mysql-bin.000019","binlog_pos":12663474,"poll_cursors_json":null,"updated_at":"2026-09-25T18:03:53.941047010+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| outage-during-snapshot | purge:required-file-is-gone | PASS |  |
| outage-during-snapshot | durable-before-restart:before-snapshot-outage | PASS | {"checkpoints":[{"db_id":"db_856d1c7df478336641e7bce3c3cd4aa4","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-2379","binlog_file":"mysql-bin.000019","binlog_pos":12663474,"poll_cursors_json":null,"updated_at":"2026-09-25T18:03:53.941047010+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| outage-during-snapshot | outage:snapshot-query-witness | PASS | SELECT `id`, `value` FROM `rec_outage_during_snapshot_2bba61`.`big` ORDER BY `id` LIMIT ? |
| outage-during-snapshot | outage:interrupted-snapshot-source-error | PASS |  |
| outage-during-snapshot | bystander:live-through-outage-3 | PASS | source commits continued; exact prefix through 27 |
| outage-during-snapshot | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=41; rolled_back=4 |
| outage-during-snapshot | converged:after-recovery | PASS |  |
| outage-during-snapshot | dlq:after-recovery | PASS | [] |
| outage-during-snapshot | converged:after-live-writes | PASS |  |
| outage-during-snapshot | dlq:after-live-writes | PASS | [] |
| outage-during-snapshot | converged:after-second-restart | PASS |  |
| outage-during-snapshot | dlq:after-second-restart | PASS | [] |
| outage-during-snapshot | automatic:no-manual-repair-event | PASS |  |
| outage-during-reconcile | contract | PASS | crates/pintail-poll/src/lib.rs: failed reconciliation does not commit state |
| outage-during-reconcile | converged:baseline | PASS |  |
| outage-during-reconcile | dlq:baseline | PASS | [] |
| outage-during-reconcile | bystander:live-through-outage-2 | PASS | source commits continued; exact prefix through 12 |
| outage-during-reconcile | outage:query-witness | PASS | SELECT `id`,`owner`,`balance`,`updated_at` FROM `rec_outage_during_reconcile_d544db`.`accounts` ORDER BY `updated_at`,`id` LIMIT 10000 OFFSET 0 |
| outage-during-reconcile | outage:0:error-visible | PASS |  |
| outage-during-reconcile | bystander:live-through-outage-3 | PASS | source commits continued; exact prefix through 111 |
| outage-during-reconcile | outage:0:source-writes-continue | PASS |  |
| outage-during-reconcile | outage:scheduled-reconciliation-completed | PASS |  |
| outage-during-reconcile | converged:outage-0-restored | PASS |  |
| outage-during-reconcile | dlq:outage-0-restored | PASS | [] |
| outage-during-reconcile | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=188; rolled_back=20 |
| outage-during-reconcile | converged:after-recovery | PASS |  |
| outage-during-reconcile | dlq:after-recovery | PASS | [] |
| outage-during-reconcile | converged:after-live-writes | PASS |  |
| outage-during-reconcile | dlq:after-live-writes | PASS | [] |
| outage-during-reconcile | converged:after-second-restart | PASS |  |
| outage-during-reconcile | dlq:after-second-restart | PASS | [] |
| outage-during-reconcile | automatic:no-manual-repair-event | PASS |  |
| operator-poll-keyless-schema-quarantine | contract | PASS | crates/pintail-api/src/supervisor.rs: quarantine contains schema drift to one table |
| operator-poll-keyless-schema-quarantine | converged:baseline | PASS |  |
| operator-poll-keyless-schema-quarantine | dlq:baseline | PASS | [] |
| operator-poll-keyless-schema-quarantine | quarantine:healthy-table-continues | PASS |  |
| operator-poll-keyless-schema-quarantine | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 1: aborting |
| operator-poll-keyless-schema-quarantine | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_5789b34341fe36a145c9838e94fd40d9","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:04:15.853363280+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"polling"}]} |
| operator-poll-keyless-schema-quarantine | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=215→217 |
| operator-poll-keyless-schema-quarantine | durable-before-restart:operator-poll-keyless-copy | PASS | {"checkpoints":[{"db_id":"db_5789b34341fe36a145c9838e94fd40d9","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:04:15.853363280+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"polling"}]} |
| operator-poll-keyless-schema-quarantine | partial-copy:not-healthy:operator-poll-keyless-copy | PASS |  |
| operator-poll-keyless-schema-quarantine | operator:interrupted-copy-resumes-without-repost | PASS |  |
| operator-poll-keyless-schema-quarantine | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=221; rolled_back=24 |
| operator-poll-keyless-schema-quarantine | converged:after-recovery | PASS |  |
| operator-poll-keyless-schema-quarantine | dlq:after-recovery | PASS | [] |
| operator-poll-keyless-schema-quarantine | converged:after-live-writes | PASS |  |
| operator-poll-keyless-schema-quarantine | dlq:after-live-writes | PASS | [] |
| operator-poll-keyless-schema-quarantine | converged:after-second-restart | PASS |  |
| operator-poll-keyless-schema-quarantine | dlq:after-second-restart | PASS | [] |
| operator-poll-keyless-schema-quarantine | automatic:no-manual-repair-event | PASS |  |
| operator-resync-table-abort | contract | PASS | docs/limitations.md: quarantined keyless table requires generation rebuild |
| operator-resync-table-abort | converged:baseline | PASS |  |
| operator-resync-table-abort | dlq:baseline | PASS | [] |
| operator-resync-table-abort | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 1: aborting |
| operator-resync-table-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_6fada22f3561c0b89861d8cf04970cf7","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-3012","binlog_file":"mysql-bin.000021","binlog_pos":590802,"poll_cursors_json":null,"updated_at":"2026-09-25T18:04:21.697028499+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"streaming"}]} |
| operator-resync-table-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=55→58 |
| operator-resync-table-abort | durable-before-restart:operator-table-copy | PASS | {"checkpoints":[{"db_id":"db_6fada22f3561c0b89861d8cf04970cf7","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-3012","binlog_file":"mysql-bin.000021","binlog_pos":590802,"poll_cursors_json":null,"updated_at":"2026-09-25T18:04:21.697028499+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"streaming"}]} |
| operator-resync-table-abort | partial-copy:not-healthy:operator-table-copy | PASS |  |
| operator-resync-table-abort | operator:interrupted-copy-resumes-without-repost | PASS |  |
| operator-resync-table-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=62; rolled_back=6 |
| operator-resync-table-abort | converged:after-recovery | PASS |  |
| operator-resync-table-abort | dlq:after-recovery | PASS | [] |
| operator-resync-table-abort | converged:after-live-writes | PASS |  |
| operator-resync-table-abort | dlq:after-live-writes | PASS | [] |
| operator-resync-table-abort | converged:after-second-restart | PASS |  |
| operator-resync-table-abort | dlq:after-second-restart | PASS | [] |
| operator-resync-table-abort | automatic:no-manual-repair-event | PASS |  |
| operator-reset-abort | contract | PASS | crates/pintail-api/src/snapshot.rs: reset and interrupted snapshot recovery |
| operator-reset-abort | converged:baseline | PASS |  |
| operator-reset-abort | dlq:baseline | PASS | [] |
| operator-reset-abort | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| operator-reset-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_28383f6293cd0fa58c3c7374009a110d","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-3082","binlog_file":"mysql-bin.000021","binlog_pos":663068,"poll_cursors_json":null,"updated_at":"2026-09-25T18:04:24.394081745+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| operator-reset-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=48→52 |
| operator-reset-abort | durable-before-restart:operator-reset | PASS | {"checkpoints":[{"db_id":"db_28383f6293cd0fa58c3c7374009a110d","kind":"gtid","gtid_set":"0cb2d7d5-b90b-11f1-b2a4-72f13a52df08:1-3082","binlog_file":"mysql-bin.000021","binlog_pos":663068,"poll_cursors_json":null,"updated_at":"2026-09-25T18:04:24.394081745+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| operator-reset-abort | partial-copy:not-healthy:operator-reset | PASS |  |
| operator-reset-abort | partial-database:not-healthy:operator-reset | PASS |  |
| operator-reset-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=55; rolled_back=6 |
| operator-reset-abort | converged:after-recovery | PASS |  |
| operator-reset-abort | dlq:after-recovery | PASS | [] |
| operator-reset-abort | converged:after-live-writes | PASS |  |
| operator-reset-abort | dlq:after-live-writes | PASS | [] |
| operator-reset-abort | converged:after-second-restart | PASS |  |
| operator-reset-abort | dlq:after-second-restart | PASS | [] |
| operator-reset-abort | automatic:no-manual-repair-event | PASS |  |
| operator-reconcile-abort | contract | PASS | crates/pintail-poll/src/lib.rs: reconciliation checkpoint follows WAL |
| operator-reconcile-abort | converged:baseline | PASS |  |
| operator-reconcile-abort | dlq:baseline | PASS | [] |
| operator-reconcile-abort | interrupts at poll.reconcile.before_state_commit | PASS | failpoint poll.reconcile.before_state_commit hit 1: aborting |
| operator-reconcile-abort | durable-before-restart:fault-poll.reconcile.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_4b81ee7b1beeb3e575b89b315b388604","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:04:26.901810109+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| operator-reconcile-abort | churn:during-crash:poll.reconcile.before_state_commit | PASS | commits=49→52 |
| operator-reconcile-abort | durable-before-restart:operator-reconcile | PASS | {"checkpoints":[{"db_id":"db_4b81ee7b1beeb3e575b89b315b388604","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-25T18:04:26.901810109+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| operator-reconcile-abort | operator:reconcile-was-running | PASS |  |
| operator-reconcile-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=55; rolled_back=6 |
| operator-reconcile-abort | converged:after-recovery | PASS |  |
| operator-reconcile-abort | dlq:after-recovery | PASS | [] |
| operator-reconcile-abort | converged:after-live-writes | PASS |  |
| operator-reconcile-abort | dlq:after-live-writes | PASS | [] |
| operator-reconcile-abort | converged:after-second-restart | PASS |  |
| operator-reconcile-abort | dlq:after-second-restart | PASS | [] |
| operator-reconcile-abort | automatic:no-manual-repair-event | PASS |  |
