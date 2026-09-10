# Recovery suite — 2026-09-10T07:17:07.742Z

Verdict: **PASS**

HEAD: 4ad883cf9f0908e5ddaa45c2073d1eb9b4a0bc2f; rustc 1.97.0 (2d8144b78 2026-07-07); Bun 1.3.14.
Working tree: clean. Binary: built from checkout (recovery profile); SHA-256: c538016a954316c4c22b7cd21207f05462c1486cff0d2eac237323a1109e29a1.
Source: MySQL 8.4.11; ROW/FULL images; MINIMAL metadata; GTID. Seed: 953.
Checks: 692 PASS, 3 WARN, 0 FAIL.
Scenarios: 38/38 requested; 38 registered. Duration: 4.6 minutes.

Failure policy: stop after the first scenario failure.

| scenario | check | status | detail |
|---|---|---|---|
| baseline | contract | PASS | docs/design/recovery-suite.md §0 |
| baseline | converged:baseline | PASS |  |
| baseline | dlq:baseline | PASS | [] |
| baseline | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=14; rolled_back=1 |
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
| mode-cdc-poll-cdc | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_95550e05abd684fe43c8bbcb94801a35","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:12:54.547782112+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-cdc-poll-cdc | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-cdc-poll-cdc | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_95550e05abd684fe43c8bbcb94801a35","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-77","binlog_file":"mysql-bin.000003","binlog_pos":61065,"poll_cursors_json":null,"updated_at":"2026-09-10T07:12:54.907853574+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
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
| mode-handoff-abort | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_24612c11c02bdda413cfafd65cb4c66a","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:12:56.907417519+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-abort | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-handoff-abort | interrupts at supervisor.handoff.after_begin | PASS | failpoint supervisor.handoff.after_begin hit 1: aborting |
| mode-handoff-abort | durable-before-restart:fault-supervisor.handoff.after_begin | PASS | {"checkpoints":[{"db_id":"db_24612c11c02bdda413cfafd65cb4c66a","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:12:56.991979545+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-abort | churn:during-crash:supervisor.handoff.after_begin | PASS | commits=71→74 |
| mode-handoff-abort | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_24612c11c02bdda413cfafd65cb4c66a","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-169","binlog_file":"mysql-bin.000003","binlog_pos":154288,"poll_cursors_json":null,"updated_at":"2026-09-10T07:12:58.510999016+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
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
| mode-handoff-snapshot-abort | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_54f098f16fb5183c7c40c7d45b4be073","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:13:00.644782222+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-handoff-snapshot-abort | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-handoff-snapshot-abort | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| mode-handoff-snapshot-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_54f098f16fb5183c7c40c7d45b4be073","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-224","binlog_file":"mysql-bin.000003","binlog_pos":208170,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:01.000615912+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| mode-handoff-snapshot-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=72→74 |
| mode-handoff-snapshot-abort | durable-before-restart:mode-handoff-copy | PASS | {"checkpoints":[{"db_id":"db_54f098f16fb5183c7c40c7d45b4be073","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-224","binlog_file":"mysql-bin.000003","binlog_pos":208170,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:01.000615912+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| mode-handoff-snapshot-abort | partial-copy:not-healthy:mode-handoff-copy | PASS |  |
| mode-handoff-snapshot-abort | partial-database:not-healthy:mode-handoff-copy | PASS |  |
| mode-handoff-snapshot-abort | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_54f098f16fb5183c7c40c7d45b4be073","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-264","binlog_file":"mysql-bin.000003","binlog_pos":254987,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:02.269156066+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-handoff-snapshot-abort | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-handoff-snapshot-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=81; rolled_back=9 |
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
| mode-poll-during-cdc-lag | durable-before-restart:polling-handoff | PASS | {"checkpoints":[{"db_id":"db_e8bfedcc11cb6853df715c25bfffccb2","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:13:04.771968876+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| mode-poll-during-cdc-lag | handoff:starts-with-polling-checkpoint | PASS |  |
| mode-poll-during-cdc-lag | durable-before-restart:cdc-handoff | PASS | {"checkpoints":[{"db_id":"db_e8bfedcc11cb6853df715c25bfffccb2","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-333","binlog_file":"mysql-bin.000003","binlog_pos":324870,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:05.150014268+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| mode-poll-during-cdc-lag | handoff:has-new-gtid-checkpoint | PASS |  |
| mode-poll-during-cdc-lag | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=55; rolled_back=6 |
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
| cdc-after-ingest | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_1461c93bc50917e2bb26b425f95ec8a1","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-359","binlog_file":"mysql-bin.000003","binlog_pos":345532,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:06.993099487+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | fixture:single-witness-transaction | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:360 |
| cdc-after-ingest | interrupts at cdc.after_ingest | PASS | failpoint cdc.after_ingest hit 1: aborting |
| cdc-after-ingest | durable-before-restart:fault-cdc.after_ingest | PASS | {"checkpoints":[{"db_id":"db_1461c93bc50917e2bb26b425f95ec8a1","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-359","binlog_file":"mysql-bin.000003","binlog_pos":345532,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:06.993099487+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | churn:during-crash:cdc.after_ingest | PASS | commits=52→54 |
| cdc-after-ingest | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_1461c93bc50917e2bb26b425f95ec8a1","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-359","binlog_file":"mysql-bin.000003","binlog_pos":345532,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:06.993099487+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-ingest | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:360 |
| cdc-after-ingest | checkpoint:previous-transaction-retained | PASS | {"before":"06a77b05-ace7-11f1-950a-767fab706f74:1-359","after":"06a77b05-ace7-11f1-950a-767fab706f74:1-359"} |
| cdc-after-ingest | checkpoint:belongs-to-source-history | PASS |  |
| cdc-after-ingest | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=58; rolled_back=6 |
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
| cdc-after-first-table-sync | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_edda3444c7adab2dec6782eae6d768c6","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-429","binlog_file":"mysql-bin.000003","binlog_pos":414873,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:10.225385739+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | fixture:single-witness-transaction | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:430 |
| cdc-after-first-table-sync | interrupts at cdc.after_first_table_sync | PASS | failpoint cdc.after_first_table_sync hit 1: aborting |
| cdc-after-first-table-sync | durable-before-restart:fault-cdc.after_first_table_sync | PASS | {"checkpoints":[{"db_id":"db_edda3444c7adab2dec6782eae6d768c6","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-429","binlog_file":"mysql-bin.000003","binlog_pos":414873,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:10.225385739+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | churn:during-crash:cdc.after_first_table_sync | PASS | commits=52→55 |
| cdc-after-first-table-sync | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_edda3444c7adab2dec6782eae6d768c6","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-429","binlog_file":"mysql-bin.000003","binlog_pos":414873,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:10.225385739+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-first-table-sync | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:430 |
| cdc-after-first-table-sync | checkpoint:previous-transaction-retained | PASS | {"before":"06a77b05-ace7-11f1-950a-767fab706f74:1-429","after":"06a77b05-ace7-11f1-950a-767fab706f74:1-429"} |
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
| cdc-before-checkpoint-commit | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_4e292e7117a0b1045132b81117af4c1e","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-499","binlog_file":"mysql-bin.000003","binlog_pos":487478,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:13.461166973+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | fixture:single-witness-transaction | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:500 |
| cdc-before-checkpoint-commit | interrupts at cdc.before_checkpoint_commit | PASS | failpoint cdc.before_checkpoint_commit hit 1: aborting |
| cdc-before-checkpoint-commit | durable-before-restart:fault-cdc.before_checkpoint_commit | PASS | {"checkpoints":[{"db_id":"db_4e292e7117a0b1045132b81117af4c1e","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-499","binlog_file":"mysql-bin.000003","binlog_pos":487478,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:13.461166973+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | churn:during-crash:cdc.before_checkpoint_commit | PASS | commits=52→55 |
| cdc-before-checkpoint-commit | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_4e292e7117a0b1045132b81117af4c1e","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-499","binlog_file":"mysql-bin.000003","binlog_pos":487478,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:13.461166973+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-before-checkpoint-commit | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:500 |
| cdc-before-checkpoint-commit | checkpoint:previous-transaction-retained | PASS | {"before":"06a77b05-ace7-11f1-950a-767fab706f74:1-499","after":"06a77b05-ace7-11f1-950a-767fab706f74:1-499"} |
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
| cdc-after-checkpoint-commit | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_13e1c9587c125661c191d9ce68259519","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-569","binlog_file":"mysql-bin.000003","binlog_pos":560642,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:16.673802440+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | fixture:single-witness-transaction | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:570 |
| cdc-after-checkpoint-commit | interrupts at cdc.after_checkpoint_commit | PASS | failpoint cdc.after_checkpoint_commit hit 1: aborting |
| cdc-after-checkpoint-commit | durable-before-restart:fault-cdc.after_checkpoint_commit | PASS | {"checkpoints":[{"db_id":"db_13e1c9587c125661c191d9ce68259519","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-570","binlog_file":"mysql-bin.000003","binlog_pos":561807,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:16.821465852+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | churn:during-crash:cdc.after_checkpoint_commit | PASS | commits=51→54 |
| cdc-after-checkpoint-commit | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_13e1c9587c125661c191d9ce68259519","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-570","binlog_file":"mysql-bin.000003","binlog_pos":561807,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:16.821465852+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-after-checkpoint-commit | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:570 |
| cdc-after-checkpoint-commit | checkpoint:advances-after-commit | PASS | {"before":"06a77b05-ace7-11f1-950a-767fab706f74:1-569","after":"06a77b05-ace7-11f1-950a-767fab706f74:1-570"} |
| cdc-after-checkpoint-commit | checkpoint:belongs-to-source-history | PASS |  |
| cdc-after-checkpoint-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=57; rolled_back=6 |
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
| cdc-wal-before-sync | durable-before-restart:before-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_ac61db4e10a14774c1e931d933e57447","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631787,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:19.869948554+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | fixture:single-witness-transaction | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:639 |
| cdc-wal-before-sync | interrupts at store.wal.before_sync | PASS | failpoint store.wal.before_sync hit 1: aborting |
| cdc-wal-before-sync | durable-before-restart:fault-store.wal.before_sync | PASS | {"checkpoints":[{"db_id":"db_ac61db4e10a14774c1e931d933e57447","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631787,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:19.869948554+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | churn:during-crash:store.wal.before_sync | PASS | commits=51→54 |
| cdc-wal-before-sync | durable-before-restart:after-cdc-fault | PASS | {"checkpoints":[{"db_id":"db_ac61db4e10a14774c1e931d933e57447","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-638","binlog_file":"mysql-bin.000003","binlog_pos":631787,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:19.869948554+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| cdc-wal-before-sync | checkpoint:acknowledges-exact-witness-only-after-commit | PASS | 06a77b05-ace7-11f1-950a-767fab706f74:639 |
| cdc-wal-before-sync | checkpoint:previous-transaction-retained | PASS | {"before":"06a77b05-ace7-11f1-950a-767fab706f74:1-638","after":"06a77b05-ace7-11f1-950a-767fab706f74:1-638"} |
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
| purge-auto-resnapshot | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_ed282034f022daf28efbc5dba10fa808","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-739","binlog_file":"mysql-bin.000003","binlog_pos":728145,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:24.574646325+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-auto-resnapshot | purge:required-file-is-gone | PASS |  |
| purge-auto-resnapshot | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_ed282034f022daf28efbc5dba10fa808 rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
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
| purge-resnapshot-abort-once | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_25c0da88f3d2c8d91a0642c42de1d493","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-778","binlog_file":"mysql-bin.000005","binlog_pos":20141,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:26.488426524+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-abort-once | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-abort-once | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_25c0da88f3d2c8d91a0642c42de1d493 rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-abort-once | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| purge-resnapshot-abort-once | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_25c0da88f3d2c8d91a0642c42de1d493","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-790","binlog_file":"mysql-bin.000007","binlog_pos":1363,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:26.872104961+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-once | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=58→61 |
| purge-resnapshot-abort-once | durable-before-restart:after-snapshot.chunk.after_ingest-2 | PASS | {"checkpoints":[{"db_id":"db_25c0da88f3d2c8d91a0642c42de1d493","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-790","binlog_file":"mysql-bin.000007","binlog_pos":1363,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:26.872104961+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
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
| purge-resnapshot-abort-twice | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_e04eba8968865f25674d93d93be804dd","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-854","binlog_file":"mysql-bin.000007","binlog_pos":67253,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:29.579576063+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-abort-twice | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-abort-twice | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_e04eba8968865f25674d93d93be804dd rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-abort-twice | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 2: aborting |
| purge-resnapshot-abort-twice | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_e04eba8968865f25674d93d93be804dd","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-866","binlog_file":"mysql-bin.000009","binlog_pos":1369,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:29.970146840+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=58→61 |
| purge-resnapshot-abort-twice | durable-before-restart:after-snapshot.chunk.after_ingest-2 | PASS | {"checkpoints":[{"db_id":"db_e04eba8968865f25674d93d93be804dd","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-866","binlog_file":"mysql-bin.000009","binlog_pos":1369,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:29.970146840+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | partial-copy:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-twice | partial-database:not-healthy:after-snapshot.chunk.after_ingest-2 | PASS |  |
| purge-resnapshot-abort-twice | interrupts at snapshot.table.before_complete | PASS | failpoint snapshot.table.before_complete hit 1: aborting |
| purge-resnapshot-abort-twice | durable-before-restart:fault-snapshot.table.before_complete | PASS | {"checkpoints":[{"db_id":"db_e04eba8968865f25674d93d93be804dd","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-907","binlog_file":"mysql-bin.000009","binlog_pos":49433,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:31.260530109+00:00"}],"tables":[{"name":"accounts","state":"snapshotting"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | churn:during-crash:snapshot.table.before_complete | PASS | commits=99→101 |
| purge-resnapshot-abort-twice | durable-before-restart:after-snapshot.table.before_complete | PASS | {"checkpoints":[{"db_id":"db_e04eba8968865f25674d93d93be804dd","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-907","binlog_file":"mysql-bin.000009","binlog_pos":49433,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:31.260530109+00:00"}],"tables":[{"name":"accounts","state":"snapshotting"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| purge-resnapshot-abort-twice | partial-copy:not-healthy:after-snapshot.table.before_complete | PASS |  |
| purge-resnapshot-abort-twice | partial-database:not-healthy:after-snapshot.table.before_complete | PASS |  |
| purge-resnapshot-abort-twice | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=105; rolled_back=11 |
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
| purge-resnapshot-position-abort | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_ec16456d6c4e5e4e8bc8718fea328d96","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-971","binlog_file":"mysql-bin.000009","binlog_pos":115941,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:34.076069300+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | purge:required-file-is-gone | PASS |  |
| purge-resnapshot-position-abort | diagnostic:cdc\.resnapshot .*unavailable source position | PASS | pintail cdc.resnapshot db=db_ec16456d6c4e5e4e8bc8718fea328d96 rebuilding after unavailable source position: CDC position requires resnapshot: Server error: `ERROR 1236 (HY000): Could not find first log file name in binary log index file' |
| purge-resnapshot-position-abort | interrupts at cdc.resnapshot.after_targets | PASS | failpoint cdc.resnapshot.after_targets hit 1: aborting |
| purge-resnapshot-position-abort | durable-before-restart:fault-cdc.resnapshot.after_targets | PASS | {"checkpoints":[{"db_id":"db_ec16456d6c4e5e4e8bc8718fea328d96","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-983","binlog_file":"mysql-bin.000011","binlog_pos":1387,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:34.467653595+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | churn:during-crash:cdc.resnapshot.after_targets | PASS | commits=57→61 |
| purge-resnapshot-position-abort | durable-before-restart:after-cdc.resnapshot.after_targets | PASS | {"checkpoints":[{"db_id":"db_ec16456d6c4e5e4e8bc8718fea328d96","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-983","binlog_file":"mysql-bin.000011","binlog_pos":1387,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:34.467653595+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| purge-resnapshot-position-abort | partial-copy:not-healthy:after-cdc.resnapshot.after_targets | PASS |  |
| purge-resnapshot-position-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=63; rolled_back=7 |
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
| repair-alter-add-column | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_ed45899df9a00c7b431b6977b80c8317","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1047","binlog_file":"mysql-bin.000011","binlog_pos":5489766,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:41.218122788+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-alter-add-column | purge:required-file-is-gone | PASS |  |
| repair-alter-add-column | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-alter-add-column | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_ed45899df9a00c7b431b6977b80c8317","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1061","binlog_file":"mysql-bin.000013","binlog_pos":1339,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:41.671118321+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-alter-add-column | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=58→61 |
| repair-alter-add-column | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_ed45899df9a00c7b431b6977b80c8317","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1061","binlog_file":"mysql-bin.000013","binlog_pos":1339,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:41.671118321+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-alter-add-column | partial-copy:not-healthy:schema-window | PASS |  |
| repair-alter-add-column | partial-database:not-healthy:schema-window | PASS |  |
| repair-alter-add-column | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=64; rolled_back=7 |
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
| repair-truncate | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_54da12bfb91a115e61da7c2ee56cff71","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1135","binlog_file":"mysql-bin.000013","binlog_pos":5491825,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:59.272898038+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-truncate | purge:required-file-is-gone | PASS |  |
| repair-truncate | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-truncate | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_54da12bfb91a115e61da7c2ee56cff71","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1147","binlog_file":"mysql-bin.000015","binlog_pos":1291,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:59.691562338+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-truncate | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=61→64 |
| repair-truncate | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_54da12bfb91a115e61da7c2ee56cff71","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1147","binlog_file":"mysql-bin.000015","binlog_pos":1291,"poll_cursors_json":null,"updated_at":"2026-09-10T07:13:59.691562338+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
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
| repair-drop-recreate | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_e19bb380310c8903e53f5abf394e2381","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1219","binlog_file":"mysql-bin.000015","binlog_pos":5490339,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:06.417998526+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-drop-recreate | purge:required-file-is-gone | PASS |  |
| repair-drop-recreate | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-drop-recreate | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_e19bb380310c8903e53f5abf394e2381","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1237","binlog_file":"mysql-bin.000017","binlog_pos":1321,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:07.012772601+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-drop-recreate | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=61→64 |
| repair-drop-recreate | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_e19bb380310c8903e53f5abf394e2381","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1237","binlog_file":"mysql-bin.000017","binlog_pos":1321,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:07.012772601+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-drop-recreate | partial-copy:not-healthy:schema-window | PASS |  |
| repair-drop-recreate | partial-database:not-healthy:schema-window | PASS |  |
| repair-drop-recreate | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=68; rolled_back=7 |
| repair-drop-recreate | converged:after-recovery | PASS |  |
| repair-drop-recreate | dlq:after-recovery | PASS | [] |
| repair-drop-recreate | converged:after-live-writes | PASS |  |
| repair-drop-recreate | dlq:after-live-writes | PASS | [] |
| repair-drop-recreate | converged:after-second-restart | PASS |  |
| repair-drop-recreate | dlq:after-second-restart | PASS | [] |
| repair-drop-recreate | automatic:no-manual-repair-event | PASS |  |
| repair-rename | contract | PASS | docs/limitations.md: rename during interrupted forced resnapshot leaves stale progress |
| repair-rename | converged:baseline | PASS |  |
| repair-rename | dlq:baseline | PASS | [] |
| repair-rename | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_37bd88e7b7791f2ad5084d96841136fa","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1316","binlog_file":"mysql-bin.000017","binlog_pos":5495647,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:13.860451011+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-rename | purge:required-file-is-gone | PASS |  |
| repair-rename | interrupts at snapshot.chunk.after_ingest | PASS | failpoint snapshot.chunk.after_ingest hit 3: aborting |
| repair-rename | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_37bd88e7b7791f2ad5084d96841136fa","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1329","binlog_file":"mysql-bin.000019","binlog_pos":2360,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:14.268992525+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-rename | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=58→62 |
| repair-rename | durable-before-restart:schema-window | PASS | {"checkpoints":[{"db_id":"db_37bd88e7b7791f2ad5084d96841136fa","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1329","binlog_file":"mysql-bin.000019","binlog_pos":2360,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:14.268992525+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"pending"},{"name":"big","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| repair-rename | partial-copy:not-healthy:schema-window | PASS |  |
| repair-rename | partial-database:not-healthy:schema-window | PASS |  |
| repair-rename | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=64; rolled_back=7 |
| repair-rename | converged:after-recovery | PASS |  |
| repair-rename | dlq:after-recovery | PASS | [] |
| repair-rename | documented-state-gap:after-recovery | WARN | big: snapshotting; docs/limitations.md: stale old-name progress after interrupted resnapshot rename. All source rows and columns compared exactly. |
| repair-rename | converged:after-live-writes | PASS |  |
| repair-rename | dlq:after-live-writes | PASS | [] |
| repair-rename | documented-state-gap:after-live-writes | WARN | big: snapshotting; docs/limitations.md: stale old-name progress after interrupted resnapshot rename. All source rows and columns compared exactly. |
| repair-rename | converged:after-second-restart | PASS |  |
| repair-rename | dlq:after-second-restart | PASS | [] |
| repair-rename | documented-state-gap:after-second-restart | WARN | big: snapshotting; docs/limitations.md: stale old-name progress after interrupted resnapshot rename. All source rows and columns compared exactly. |
| repair-rename | durable-before-restart:documented-state-gap | PASS | {"checkpoints":[{"db_id":"db_37bd88e7b7791f2ad5084d96841136fa","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-1383","binlog_file":"mysql-bin.000019","binlog_pos":54889,"poll_cursors_json":null,"updated_at":"2026-09-10T07:14:22.322018662+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"snapshotting"},{"name":"big2","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| repair-rename | gap:old-name-is-not-a-complete-copy | PASS |  |
| repair-rename | automatic:no-manual-repair-event | PASS |  |
| reconcile-alter | contract | PASS | docs/limitations.md: polling re-probe after DDL |
| reconcile-alter | converged:baseline | PASS |  |
| reconcile-alter | dlq:baseline | PASS | [] |
| reconcile-alter | interrupts at poll.reconcile.before_state_commit | PASS | failpoint poll.reconcile.before_state_commit hit 3: aborting |
| reconcile-alter | durable-before-restart:fault-poll.reconcile.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_e045351ba05dd7a361c2168b531eab32","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:35.511759451+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"big","state":"polling"},{"name":"ledger","state":"polling"}]} |
| reconcile-alter | churn:during-crash:poll.reconcile.before_state_commit | PASS | commits=270→272 |
| reconcile-alter | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=276; rolled_back=30 |
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
| poll-after-ingest | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_21f63a9a1d9ef14ce4d3b9017c4af53c","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:53.523096422+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | poll:cursor-strategy | PASS |  |
| poll-after-ingest | poll:checksum-strategy | PASS |  |
| poll-after-ingest | poll:keyless-fixture | PASS |  |
| poll-after-ingest | interrupts at poll.after_ingest | PASS | failpoint poll.after_ingest hit 3: aborting |
| poll-after-ingest | durable-before-restart:fault-poll.after_ingest | PASS | {"checkpoints":[{"db_id":"db_21f63a9a1d9ef14ce4d3b9017c4af53c","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:53.592865432+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | churn:during-crash:poll.after_ingest | PASS | commits=47→51 |
| poll-after-ingest | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_21f63a9a1d9ef14ce4d3b9017c4af53c","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:53.592865432+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-after-ingest | poll:retains-durable-poll-state | PASS |  |
| poll-after-ingest | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-after-ingest | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=5 |
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
| poll-before-state-commit | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_cc0192db385aa4b65cc5f127fcf42c0f","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:59.129816542+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | poll:cursor-strategy | PASS |  |
| poll-before-state-commit | poll:checksum-strategy | PASS |  |
| poll-before-state-commit | poll:keyless-fixture | PASS |  |
| poll-before-state-commit | interrupts at poll.before_state_commit | PASS | failpoint poll.before_state_commit hit 3: aborting |
| poll-before-state-commit | durable-before-restart:fault-poll.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_cc0192db385aa4b65cc5f127fcf42c0f","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:59.193926607+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | churn:during-crash:poll.before_state_commit | PASS | commits=47→50 |
| poll-before-state-commit | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_cc0192db385aa4b65cc5f127fcf42c0f","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:14:59.193926607+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-before-state-commit | poll:retains-durable-poll-state | PASS |  |
| poll-before-state-commit | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-before-state-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=5 |
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
| poll-append-after-reset | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_cadae80c3466858284e03df7691e8fca","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:04.731975028+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | poll:cursor-strategy | PASS |  |
| poll-append-after-reset | poll:checksum-strategy | PASS |  |
| poll-append-after-reset | poll:keyless-fixture | PASS |  |
| poll-append-after-reset | interrupts at poll.append.after_reset | PASS | failpoint poll.append.after_reset hit 1: aborting |
| poll-append-after-reset | durable-before-restart:fault-poll.append.after_reset | PASS | {"checkpoints":[{"db_id":"db_cadae80c3466858284e03df7691e8fca","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:04.800664487+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | churn:during-crash:poll.append.after_reset | PASS | commits=47→51 |
| poll-append-after-reset | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_cadae80c3466858284e03df7691e8fca","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:04.800664487+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-append-after-reset | poll:retains-durable-poll-state | PASS |  |
| poll-append-after-reset | poll:interrupted-table-state-is-old | PASS | audit |
| poll-append-after-reset | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=5 |
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
| poll-checksum-before-chunk-commit | durable-before-restart:poll-strategies | PASS | {"checkpoints":[{"db_id":"db_d92a8088ea69a81161ce36142c97bd93","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:10.365367352+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | poll:cursor-strategy | PASS |  |
| poll-checksum-before-chunk-commit | poll:checksum-strategy | PASS |  |
| poll-checksum-before-chunk-commit | poll:keyless-fixture | PASS |  |
| poll-checksum-before-chunk-commit | interrupts at poll.checksum.before_chunk_commit | PASS | failpoint poll.checksum.before_chunk_commit hit 1: aborting |
| poll-checksum-before-chunk-commit | durable-before-restart:fault-poll.checksum.before_chunk_commit | PASS | {"checkpoints":[{"db_id":"db_d92a8088ea69a81161ce36142c97bd93","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:10.431149246+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | churn:during-crash:poll.checksum.before_chunk_commit | PASS | commits=47→51 |
| poll-checksum-before-chunk-commit | durable-before-restart:poll-interrupted | PASS | {"checkpoints":[{"db_id":"db_d92a8088ea69a81161ce36142c97bd93","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:15:10.431149246+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| poll-checksum-before-chunk-commit | poll:retains-durable-poll-state | PASS |  |
| poll-checksum-before-chunk-commit | poll:interrupted-table-state-is-old | PASS | ledger |
| poll-checksum-before-chunk-commit | poll:chunk-journal-is-old | PASS |  |
| poll-checksum-before-chunk-commit | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=54; rolled_back=5 |
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
| poll-delete-insert-neutral | neutral:mutation-during-pagination | PASS | SELECT `id`,`owner`,`balance`,`updated_at` FROM `rec_poll_delete_insert_neutral_9f01ed`.`accounts` WHERE `updated_at` >= ? ORDER BY `updated_at`,`id` LIMIT 10000 OFFSET 10000 |
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
| outage-during-cdc | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=34; rolled_back=3 |
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
| outage-repeated | bystander:live-through-outage-3 | PASS | source commits continued; exact prefix through 23 |
| outage-repeated | outage:0:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-0-restored | PASS |  |
| outage-repeated | dlq:outage-0-restored | PASS | [] |
| outage-repeated | outage:1:error-visible | PASS |  |
| outage-repeated | bystander:live-through-outage-4 | PASS | source commits continued; exact prefix through 37 |
| outage-repeated | outage:1:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-1-restored | PASS |  |
| outage-repeated | dlq:outage-1-restored | PASS | [] |
| outage-repeated | outage:2:error-visible | PASS |  |
| outage-repeated | bystander:live-through-outage-5 | PASS | source commits continued; exact prefix through 52 |
| outage-repeated | outage:2:source-writes-continue | PASS |  |
| outage-repeated | converged:outage-2-restored | PASS |  |
| outage-repeated | dlq:outage-2-restored | PASS | [] |
| outage-repeated | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=62; rolled_back=6 |
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
| outage-during-snapshot | durable-before-restart:before-purge | PASS | {"checkpoints":[{"db_id":"db_dc5240e18503b98a3f55190aa235a826","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-2355","binlog_file":"mysql-bin.000019","binlog_pos":12654986,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:22.470990883+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| outage-during-snapshot | purge:required-file-is-gone | PASS |  |
| outage-during-snapshot | durable-before-restart:before-snapshot-outage | PASS | {"checkpoints":[{"db_id":"db_dc5240e18503b98a3f55190aa235a826","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-2355","binlog_file":"mysql-bin.000019","binlog_pos":12654986,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:22.470990883+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"streaming"},{"name":"big","state":"streaming"},{"name":"ledger","state":"streaming"}]} |
| outage-during-snapshot | outage:snapshot-query-witness | PASS | SELECT `id`, `value` FROM `rec_outage_during_snapshot_81e873`.`big` ORDER BY `id` LIMIT ? |
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
| outage-during-reconcile | outage:query-witness | PASS | SELECT `id`,`owner`,`balance`,`updated_at` FROM `rec_outage_during_reconcile_de9cdb`.`accounts` ORDER BY `updated_at`,`id` LIMIT 10000 OFFSET 0 |
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
| operator-poll-keyless-schema-quarantine | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_9b99b40b4bc7cbf9834d9d35d4f0b27c","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:16:50.657463791+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"polling"}]} |
| operator-poll-keyless-schema-quarantine | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=215→218 |
| operator-poll-keyless-schema-quarantine | durable-before-restart:operator-poll-keyless-copy | PASS | {"checkpoints":[{"db_id":"db_9b99b40b4bc7cbf9834d9d35d4f0b27c","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:16:50.657463791+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"polling"}]} |
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
| operator-resync-table-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_786df09cc77be4ea32f8ef9a1cad8894","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-2993","binlog_file":"mysql-bin.000021","binlog_pos":589667,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:56.496244014+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"streaming"}]} |
| operator-resync-table-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=55→58 |
| operator-resync-table-abort | durable-before-restart:operator-table-copy | PASS | {"checkpoints":[{"db_id":"db_786df09cc77be4ea32f8ef9a1cad8894","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-2993","binlog_file":"mysql-bin.000021","binlog_pos":589667,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:56.496244014+00:00"}],"tables":[{"name":"accounts","state":"streaming"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"streaming"}]} |
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
| operator-reset-abort | durable-before-restart:fault-snapshot.chunk.after_ingest | PASS | {"checkpoints":[{"db_id":"db_b666a92bba1aeb6231af006ad37b7c6c","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-3063","binlog_file":"mysql-bin.000021","binlog_pos":661933,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:59.409211852+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
| operator-reset-abort | churn:during-crash:snapshot.chunk.after_ingest | PASS | commits=49→52 |
| operator-reset-abort | durable-before-restart:operator-reset | PASS | {"checkpoints":[{"db_id":"db_b666a92bba1aeb6231af006ad37b7c6c","kind":"gtid","gtid_set":"06a77b05-ace7-11f1-950a-767fab706f74:1-3063","binlog_file":"mysql-bin.000021","binlog_pos":661933,"poll_cursors_json":null,"updated_at":"2026-09-10T07:16:59.409211852+00:00"}],"tables":[{"name":"accounts","state":"pending"},{"name":"audit","state":"snapshotting"},{"name":"ledger","state":"snapshotting"}]} |
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
| operator-reconcile-abort | durable-before-restart:fault-poll.reconcile.before_state_commit | PASS | {"checkpoints":[{"db_id":"db_c7d9c4941ac0929644fd1b34af04f0e8","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:17:02.223606896+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| operator-reconcile-abort | churn:during-crash:poll.reconcile.before_state_commit | PASS | commits=49→52 |
| operator-reconcile-abort | durable-before-restart:operator-reconcile | PASS | {"checkpoints":[{"db_id":"db_c7d9c4941ac0929644fd1b34af04f0e8","kind":"polling","gtid_set":null,"binlog_file":null,"binlog_pos":null,"poll_cursors_json":"{}","updated_at":"2026-09-10T07:17:02.223606896+00:00"}],"tables":[{"name":"accounts","state":"polling"},{"name":"audit","state":"polling"},{"name":"ledger","state":"polling"}]} |
| operator-reconcile-abort | operator:reconcile-was-running | PASS |  |
| operator-reconcile-abort | churn:commits-and-rollbacks-through-injection | PASS | seed=953; committed=55; rolled_back=6 |
| operator-reconcile-abort | converged:after-recovery | PASS |  |
| operator-reconcile-abort | dlq:after-recovery | PASS | [] |
| operator-reconcile-abort | converged:after-live-writes | PASS |  |
| operator-reconcile-abort | dlq:after-live-writes | PASS | [] |
| operator-reconcile-abort | converged:after-second-restart | PASS |  |
| operator-reconcile-abort | dlq:after-second-restart | PASS | [] |
| operator-reconcile-abort | automatic:no-manual-repair-event | PASS |  |
