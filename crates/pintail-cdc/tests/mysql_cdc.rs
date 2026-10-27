use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mysql_async::{Opts, Pool, prelude::Queryable};
use pintail_cdc::{CdcCheckpoint, CdcError, CdcOptions, CdcTarget, run_cdc};
use pintail_meta::{CdcApplyIntent, MetaStore};
use pintail_poll::{CdcReconcileScope, PollTarget, run_cdc_reconciliation};
use pintail_probe::{ProbeReport, RecommendedMode, probe};
use pintail_snapshot::{SnapshotOptions, SnapshotTarget, run_snapshot};
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, Value};
use rusqlite::Connection;

const DATABASE_ID: &str = "m4-source";
const RESTART_WRITER_GATE: &str = "pintail-cdc-restart-writer";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn spilled_unique_value_swap_is_never_torn_by_readers() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE unique_swap (\
               id BIGINT UNSIGNED PRIMARY KEY,\
               slot VARCHAR(16) NOT NULL UNIQUE\
             );\
             INSERT INTO unique_swap VALUES (1,'left'),(2,'right');",
        )
        .expect("unique-swap source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("unique-swap DSN"));
    let report = probe(&pool, "app").await.expect("probe unique-swap source");
    let workspace = tempfile::tempdir().expect("unique-swap workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("unique-swap metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register unique-swap source");
    let source = report.tables.first().expect("unique-swap table").clone();
    let schema = source.table_schema().expect("unique-swap schema");
    let table_directory = workspace.path().join("unique_swap");
    let store = TableStore::open(&table_directory, schema.clone(), StoreOptions::default())
        .expect("unique-swap store");
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![SnapshotTarget::new(source.clone(), store).expect("unique-swap snapshot target")],
        SnapshotOptions::default(),
    )
    .await
    .expect("unique-swap baseline snapshot");

    mysql
        .query_batch(
            "START TRANSACTION;\
               UPDATE unique_swap SET slot='moving' WHERE id=1;\
               UPDATE unique_swap SET slot='left' WHERE id=2;\
               UPDATE unique_swap SET slot='right' WHERE id=1;\
             COMMIT;",
        )
        .expect("commit unique-value swap");

    let stop = Arc::new(AtomicBool::new(false));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let reader_stop = Arc::clone(&stop);
    let reader_observations = Arc::clone(&observations);
    let reader = thread::spawn(move || {
        while !reader_stop.load(Ordering::Acquire) {
            let rows = TableSnapshot::open(&table_directory, schema.clone())
                .expect("open concurrent unique-swap reader")
                .scan()
                .expect("scan concurrent unique-swap reader");
            let slots = rows
                .iter()
                .map(|row| row.values()[1].clone())
                .collect::<Vec<_>>();
            assert!(
                slots
                    == [
                        Value::Utf8("left".to_owned()),
                        Value::Utf8("right".to_owned())
                    ]
                    || slots
                        == [
                            Value::Utf8("right".to_owned()),
                            Value::Utf8("left".to_owned())
                        ],
                "reader observed a torn source transaction: {slots:?}"
            );
            reader_observations
                .lock()
                .expect("record unique-swap observation")
                .push(slots);
        }
    });
    thread::sleep(Duration::from_millis(25));

    let target = snapshot
        .targets
        .into_iter()
        .next()
        .expect("unique-swap snapshot target");
    let result = run_cdc(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![CdcTarget::new(source, target.into_store()).expect("unique-swap CDC target")],
        CdcOptions {
            blocking: false,
            max_transaction_bytes: 1,
            ..CdcOptions::default()
        },
    )
    .await
    .expect("unique-swap finite CDC catch-up");
    assert_eq!(result.mutations, 3);
    thread::sleep(Duration::from_millis(25));
    stop.store(true, Ordering::Release);
    reader.join().expect("join unique-swap reader");

    {
        let observations = observations.lock().expect("inspect observations");
        assert!(
            observations.iter().any(|slots| {
                slots.as_slice()
                    == [
                        Value::Utf8("left".to_owned()),
                        Value::Utf8("right".to_owned()),
                    ]
            }),
            "reader never observed the pre-commit state"
        );
        assert!(
            observations.iter().any(|slots| {
                slots.as_slice()
                    == [
                        Value::Utf8("right".to_owned()),
                        Value::Utf8("left".to_owned()),
                    ]
            }),
            "reader never observed the committed state"
        );
    }
    pool.disconnect()
        .await
        .expect("disconnect unique-swap pool");
}

struct MysqlContainer {
    name: String,
    host: String,
    port: u16,
    client: String,
}

impl MysqlContainer {
    fn start() -> Result<Self, String> {
        Self::start_variant(
            "mysql84-gtid",
            "mysql:8.4",
            "mysql",
            &[
                "--server-id=184",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--gtid-mode=ON",
                "--enforce-gtid-consistency=ON",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        )
    }

    fn start_file_position() -> Result<Self, String> {
        Self::start_variant(
            "mysql84-filepos",
            "mysql:8.4",
            "mysql",
            &[
                "--server-id=185",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        )
    }

    fn start_variant(
        label: &str,
        image: &str,
        client: &str,
        server_arguments: &[&str],
    ) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let name = format!("pintail-m4-{label}-{}-{nonce}", std::process::id());
        let mut command = Command::new("docker");
        command.args([
            "run",
            "--detach",
            "--name",
            &name,
            "--publish",
            "0:3306",
            "--tmpfs",
            "/var/lib/mysql:rw,size=2g",
            "--env",
            "MYSQL_ROOT_PASSWORD=pintail-root",
            "--env",
            "MYSQL_DATABASE=app",
            image,
        ]);
        command.args(server_arguments);
        checked_output(&mut command, "start MySQL 8.4 CDC source")?;
        let host = docker_host()?;
        let port_output = checked_output(
            Command::new("docker").args(["port", &name, "3306/tcp"]),
            "inspect MySQL CDC port",
        )?;
        let port = String::from_utf8(port_output.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next())
            .and_then(|port| port.parse().ok())
            .ok_or_else(|| "Docker did not report a numeric MySQL port".to_owned())?;
        let container = Self {
            name,
            host,
            port,
            client: client.to_owned(),
        };
        // The official images run a temporary server during initialization
        // and then restart; one successful probe can land in that window and
        // the next statement hits a stopped socket. Require consecutive
        // successes so the container is only handed out once the real
        // server is up for good.
        let mut consecutive = 0;
        for _ in 0..240 {
            if container.query_batch("SELECT 1;").is_ok() {
                consecutive += 1;
                if consecutive >= 3 {
                    return Ok(container);
                }
            } else {
                consecutive = 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err("MySQL 8.4 did not become ready within 120 seconds".to_owned())
    }

    fn dsn(&self) -> String {
        format!(
            "mysql://pintail:pintail@{}:{}/app",
            dsn_host(&self.host),
            self.port
        )
    }

    fn query_batch(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "--interactive",
                &self.name,
                &self.client,
                "--user=root",
                "--password=pintail-root",
                "--database=app",
                "--default-character-set=utf8mb4",
                "--batch",
                "--raw",
                "--skip-column-names",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn MySQL client: {error}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| "MySQL stdin was not piped".to_owned())?
            .write_all(sql.as_bytes())
            .map_err(|error| format!("write MySQL batch: {error}"))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for MySQL batch: {error}"))?;
        if output.status.success() {
            String::from_utf8(output.stdout).map_err(|error| error.to_string())
        } else {
            Err(format_output_error("execute MySQL batch", &output))
        }
    }
}

impl Drop for MysqlContainer {
    fn drop(&mut self) {
        let _status = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn ddl_evolution_add_drop_rename_create_truncate_and_orphan() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE add_drop (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO add_drop VALUES (1,'one');\
             CREATE TABLE rename_me (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO rename_me VALUES (1,'rename-old');\
             CREATE TABLE truncate_me (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO truncate_me VALUES (1,'truncate-old');\
             CREATE TABLE drop_me (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO drop_me VALUES (1,'orphan-retained');",
        )
        .expect("DDL source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("DDL DSN"));
    let mut report = probe(&pool, "app").await.expect("probe DDL source");
    let workspace = tempfile::tempdir().expect("DDL workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("DDL metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register DDL source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("DDL table schema"),
                StoreOptions::default(),
            )
            .expect("DDL table store");
            SnapshotTarget::new(source.clone(), store).expect("DDL snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("DDL initial snapshot");
    let mut targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("DDL CDC target")
        })
        .collect();

    mysql
        .query_batch("ALTER TABLE add_drop ADD COLUMN note VARCHAR(64) NULL;")
        .expect("ADD COLUMN");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    let added = cdc_target(&targets, "add_drop");
    assert_eq!(
        added
            .store()
            .schema()
            .columns()
            .iter()
            .map(Column::name)
            .collect::<Vec<_>>(),
        ["id", "value", "note"]
    );
    assert_eq!(added.store().schema().version(), 2);
    assert_eq!(
        added.store().snapshot().scan().unwrap()[0].values()[2],
        Value::Null
    );

    mysql
        .query_batch("INSERT INTO add_drop VALUES (2,'two','two-note');")
        .expect("post-add insert");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    mysql
        .query_batch("ALTER TABLE add_drop DROP COLUMN value;")
        .expect("DROP COLUMN");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    mysql
        .query_batch("UPDATE add_drop SET note='one-note' WHERE id=1;")
        .expect("post-drop update");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    let dropped = cdc_target(&targets, "add_drop");
    assert_eq!(
        dropped
            .store()
            .schema()
            .columns()
            .iter()
            .map(Column::name)
            .collect::<Vec<_>>(),
        ["id", "note"]
    );
    assert_eq!(dropped.store().schema().version(), 3);
    assert_eq!(
        dropped
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        [
            vec![Value::UInt64(1), Value::Utf8("one-note".to_owned())],
            vec![Value::UInt64(2), Value::Utf8("two-note".to_owned())],
        ]
    );

    drop(targets);
    report = probe(&pool, "app")
        .await
        .expect("reprobe after schema evolution");
    targets = report
        .tables
        .iter()
        .map(|source| {
            CdcTarget::open_tracked(
                &metadata_path,
                DATABASE_ID,
                source.clone(),
                workspace.path().join(&source.name),
                StoreOptions::default(),
            )
            .expect("reopen tracked DDL target")
        })
        .collect();
    assert_eq!(
        cdc_target(&targets, "add_drop")
            .store()
            .schema()
            .columns()
            .iter()
            .map(Column::id)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    mysql
        .query_batch("UPDATE add_drop SET note='restart-safe' WHERE id=2;")
        .expect("post-restart schema update");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert!(
        cdc_target(&targets, "add_drop")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .iter()
            .any(|row| row.values()[1] == Value::Utf8("restart-safe".to_owned()))
    );

    mysql
        .query_batch("ALTER TABLE rename_me RENAME COLUMN value TO renamed;")
        .expect("RENAME COLUMN");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    let renamed_target = cdc_target(&targets, "rename_me");
    assert_eq!(
        renamed_target
            .store()
            .schema()
            .columns()
            .iter()
            .map(Column::name)
            .collect::<Vec<_>>(),
        ["id", "renamed"]
    );
    assert_eq!(
        renamed_target.store().snapshot().scan().unwrap()[0].values()[1],
        Value::Utf8("rename-old".to_owned())
    );
    assert!(
        !MetaStore::open(&metadata_path)
            .unwrap()
            .tables_needing_resync(DATABASE_ID)
            .unwrap()
            .contains("rename_me")
    );

    mysql
        .query_batch("TRUNCATE TABLE truncate_me;")
        .expect("TRUNCATE");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "truncate_me")
            .store()
            .snapshot()
            .scan()
            .unwrap(),
        [] as [pintail_types::StoredRow; 0]
    );
    mysql
        .query_batch("INSERT INTO truncate_me VALUES (2,'truncate-new');")
        .expect("post-truncate insert");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "truncate_me")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        1
    );

    mysql
        .query_batch(
            "CREATE TABLE created_after (id BIGINT UNSIGNED PRIMARY KEY, payload VARCHAR(64));",
        )
        .expect("CREATE TABLE");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "created_after")
            .store()
            .schema()
            .version(),
        1
    );
    report = probe(&pool, "app")
        .await
        .expect("refresh report after create");
    mysql
        .query_batch("INSERT INTO created_after VALUES (1,'created-live');")
        .expect("created-table insert");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "created_after")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        1
    );

    mysql
        .query_batch("DROP TABLE drop_me;")
        .expect("DROP TABLE");
    targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "drop_me")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        1
    );
    let metadata = MetaStore::open(&metadata_path).expect("inspect DDL metadata");
    assert_eq!(
        metadata
            .schema_history(DATABASE_ID, "add_drop")
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        metadata
            .schema_history(DATABASE_ID, "created_after")
            .unwrap()
            .len(),
        1
    );
    drop(metadata);
    let connection = Connection::open(&metadata_path).expect("inspect orphan state");
    let orphan: (String, Option<String>) = connection
        .query_row(
            "SELECT state, orphaned_at FROM tables \
             WHERE db_id = ?1 AND name = 'drop_me'",
            [DATABASE_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("orphan row");
    assert_eq!(orphan.0, "excluded");
    assert!(orphan.1.is_some());
    pool.disconnect().await.expect("disconnect DDL pool");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn cdc_cascade_negative_control_and_scheduled_repair() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE cascade_parent (id BIGINT UNSIGNED PRIMARY KEY);\
             CREATE TABLE cascade_child (\
               id BIGINT UNSIGNED PRIMARY KEY,\
               parent_id BIGINT UNSIGNED NOT NULL,\
               value VARCHAR(64) NOT NULL,\
               CONSTRAINT cascade_fk FOREIGN KEY (parent_id) \
                 REFERENCES cascade_parent(id) \
                 ON DELETE CASCADE ON UPDATE CASCADE\
             );\
             INSERT INTO cascade_parent VALUES (1),(2);\
             INSERT INTO cascade_child VALUES (10,1,'first'),(11,2,'second');",
        )
        .expect("cascade source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("cascade DSN"));
    let report = probe(&pool, "app").await.expect("probe cascade source");
    assert!(
        report
            .tables
            .iter()
            .find(|table| table.name == "cascade_child")
            .expect("cascade child probe")
            .requires_reconciliation
    );
    let workspace = tempfile::tempdir().expect("cascade workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("cascade metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register cascade source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("cascade schema"),
                StoreOptions::default(),
            )
            .expect("cascade store");
            SnapshotTarget::new(source.clone(), store).expect("cascade snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("cascade snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("cascade CDC target")
        })
        .collect();

    mysql
        .query_batch("UPDATE cascade_child SET value='cdc-high' WHERE id=10;")
        .expect("raise child CDC version");
    let raised = finite_catch_up(&pool, &metadata_path, &report, targets)
        .await
        .expect("child update catch-up");
    let raised_version = cdc_target(&raised.targets, "cascade_child")
        .store()
        .snapshot()
        .scan()
        .unwrap()
        .iter()
        .find(|row| row.values()[0] == Value::UInt64(10))
        .expect("updated child")
        .version();
    assert!(raised_version > 0);

    mysql
        .query_batch(
            "DELETE FROM cascade_parent WHERE id=1;\
             UPDATE cascade_parent SET id=20 WHERE id=2;",
        )
        .expect("cascade parent delete and key update");
    let negative = finite_catch_up(&pool, &metadata_path, &report, raised.targets)
        .await
        .expect("cascade CDC catch-up");
    assert_eq!(
        cdc_target(&negative.targets, "cascade_parent")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        cdc_target(&negative.targets, "cascade_child")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        2,
        "negative control: InnoDB cascade emitted no child row event"
    );
    assert_eq!(
        cdc_target(&negative.targets, "cascade_child")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .iter()
            .find(|row| row.values()[0] == Value::UInt64(11))
            .expect("cascade-updated child remains visible")
            .values()[1],
        Value::UInt64(2),
        "negative control: InnoDB cascade update emitted no child row event"
    );
    let checkpoint_before = MetaStore::open(&metadata_path)
        .unwrap()
        .snapshot_checkpoint(DATABASE_ID)
        .unwrap()
        .expect("CDC checkpoint before reconcile");
    // The scheduled repair opens every table and names the child: the
    // parent rides along, and the child's rows are verified through it.
    let cdc_targets = negative.targets;
    let targets = cdc_targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            PollTarget::new(source, target.into_store()).expect("cascade reconciliation target")
        })
        .collect::<Vec<_>>();
    let due = vec!["cascade_child".to_owned()];
    let reconciliation = run_cdc_reconciliation(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        CdcReconcileScope::Cascade(&due),
        1,
    )
    .await
    .expect("scheduled cascade reconciliation");
    assert_eq!(reconciliation.tables[0].ingested, 1);
    assert_eq!(reconciliation.tables[0].tombstones, 1);
    assert!(reconciliation.tables[0].version > raised_version);
    assert_eq!(
        reconciliation.targets[0]
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reconciliation.targets[0].store().snapshot().scan().unwrap()[0].values()[1],
        Value::UInt64(20)
    );
    let checkpoint_after = MetaStore::open(&metadata_path)
        .unwrap()
        .snapshot_checkpoint(DATABASE_ID)
        .unwrap()
        .expect("CDC checkpoint after reconcile");
    assert_eq!(checkpoint_after, checkpoint_before);
    let connection = Connection::open(&metadata_path).expect("inspect cascade mode");
    let mode: (String, String) = connection
        .query_row(
            "SELECT state, effective_mode FROM databases WHERE id = ?1",
            [DATABASE_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("cascade database mode");
    assert_eq!(mode, ("streaming".to_owned(), "cdc".to_owned()));
    pool.disconnect().await.expect("disconnect cascade pool");
}

/// The replica's rows of `table` as text, one line per row in key order,
/// the way the `MySQL` client prints them.
fn replica_text(targets: &[PollTarget], table: &str) -> String {
    let target = targets
        .iter()
        .find(|target| target.source().name == table)
        .expect("reconciled target");
    let mut lines = target
        .store()
        .snapshot()
        .scan()
        .expect("replica scan")
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Int64(value) => value.to_string(),
                    Value::UInt64(value) => value.to_string(),
                    Value::Utf8(value) => value.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>();
    lines.sort();
    lines.join("\n")
}

fn source_text(mysql: &MysqlContainer, sql: &str) -> String {
    let mut lines = mysql
        .query_batch(sql)
        .expect("source rows")
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    lines.join("\n")
}

/// Parent-key shapes a repair driven by absent parents never saw: a parent
/// deleted and inserted again with the same key, `REPLACE` on a parent, and
/// `ON UPDATE CASCADE` into a child's primary key. The stream queues the
/// parent keys it saw deleted or moved, and one pass repairs all three. A
/// cascade-affected child without a key is marked for a recopy instead, and
/// does not stop its siblings' repair.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn cdc_cascade_parent_key_reuse_replace_and_child_key_moves() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE shelf (id INT PRIMARY KEY, label VARCHAR(16) NOT NULL);\
             CREATE TABLE book (\
               id INT PRIMARY KEY, shelf_id INT NULL, title VARCHAR(16) NOT NULL,\
               FOREIGN KEY (shelf_id) REFERENCES shelf(id) ON DELETE CASCADE\
             );\
             CREATE TABLE loose_note (\
               shelf_id INT NOT NULL, body VARCHAR(16) NOT NULL,\
               FOREIGN KEY (shelf_id) REFERENCES shelf(id) ON DELETE CASCADE\
             );\
             CREATE TABLE bin (zone CHAR(2) NOT NULL, slot INT NOT NULL, PRIMARY KEY (zone, slot));\
             CREATE TABLE bin_item (\
               zone CHAR(2) NOT NULL, slot INT NOT NULL, item INT NOT NULL, qty INT NOT NULL,\
               PRIMARY KEY (zone, slot, item),\
               FOREIGN KEY (zone, slot) REFERENCES bin(zone, slot) \
                 ON DELETE CASCADE ON UPDATE CASCADE\
             );\
             INSERT INTO shelf VALUES (1,'one'),(2,'two'),(3,'three');\
             INSERT INTO book VALUES (10,1,'a'),(11,1,'b'),(20,2,'c'),(30,3,'d');\
             INSERT INTO loose_note VALUES (1,'x'),(2,'y');\
             INSERT INTO bin VALUES ('aa',1),('aa',2);\
             INSERT INTO bin_item VALUES ('aa',1,7,3),('aa',1,8,4),('aa',2,9,5);",
        )
        .expect("cascade source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("cascade DSN"));
    let report = probe(&pool, "app").await.expect("probe cascade source");
    let workspace = tempfile::tempdir().expect("cascade workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("cascade metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register cascade source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("cascade schema"),
                StoreOptions::default(),
            )
            .expect("cascade store");
            SnapshotTarget::new(source.clone(), store).expect("cascade snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("cascade snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("cascade CDC target")
        })
        .collect();
    // Every shape leaves its parent key present in the replica again, so a
    // child cannot be found by its parent's absence.
    mysql
        .query_batch(
            "DELETE FROM shelf WHERE id=1;\
             INSERT INTO shelf VALUES (1,'again');\
             INSERT INTO book VALUES (12,1,'new');\
             REPLACE INTO shelf VALUES (2,'replaced');\
             UPDATE bin SET slot=5 WHERE zone='aa' AND slot=1;\
             INSERT INTO bin VALUES ('aa',1);",
        )
        .expect("parent key reuse");
    let caught_up = finite_catch_up(&pool, &metadata_path, &report, targets)
        .await
        .expect("cascade CDC catch-up");
    let queued = MetaStore::open(&metadata_path)
        .unwrap()
        .cascade_pending(DATABASE_ID, "book")
        .unwrap();
    assert!(
        queued.keys.len() >= 2,
        "the stream queues the deleted and replaced shelf keys: {queued:?}"
    );
    let targets = caught_up
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            PollTarget::new(source, target.into_store()).expect("cascade reconciliation target")
        })
        .collect::<Vec<_>>();
    let due = vec![
        "book".to_owned(),
        "loose_note".to_owned(),
        "bin_item".to_owned(),
    ];
    let reconciliation = run_cdc_reconciliation(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        CdcReconcileScope::Cascade(&due),
        1,
    )
    .await
    .expect("scheduled cascade reconciliation");
    for table in &reconciliation.tables {
        assert!(table.error.is_none(), "{}: {:?}", table.table, table.error);
    }
    let keyless = reconciliation
        .tables
        .iter()
        .find(|table| table.table == "loose_note")
        .expect("keyless outcome");
    assert!(keyless.resync, "the keyless child is marked for a recopy");
    assert_eq!(
        replica_text(&reconciliation.targets, "book"),
        source_text(&mysql, "SELECT id, shelf_id, title FROM book;"),
        "reused and replaced parents: their old children are gone, the new one stays"
    );
    assert_eq!(
        replica_text(&reconciliation.targets, "bin_item"),
        source_text(&mysql, "SELECT zone, slot, item, qty FROM bin_item;"),
        "ON UPDATE CASCADE into the child's key: the rows are under the new key only"
    );
    let state: String = Connection::open(&metadata_path)
        .unwrap()
        .query_row(
            "SELECT state FROM tables WHERE db_id = ?1 AND name = 'loose_note'",
            [DATABASE_ID],
            |row| row.get(0),
        )
        .expect("keyless table state");
    assert_eq!(state, "needs_resync");
    // A full compare of the repaired tables finds nothing left to change,
    // and the pass consumed the keys it was queued.
    let keyed = reconciliation
        .targets
        .into_iter()
        .filter(|target| ["book", "bin_item"].contains(&target.source().name.as_str()))
        .collect::<Vec<_>>();
    let again = run_cdc_reconciliation(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        keyed,
        CdcReconcileScope::Full,
        1_000,
    )
    .await
    .expect("full compare after the pass");
    for table in again
        .tables
        .iter()
        .filter(|table| table.table == "book" || table.table == "bin_item")
    {
        assert_eq!(
            (table.ingested, table.tombstones),
            (0, 0),
            "{} was already exact",
            table.table
        );
    }
    assert_eq!(
        MetaStore::open(&metadata_path)
            .unwrap()
            .cascade_pending(DATABASE_ID, "book")
            .unwrap()
            .keys,
        [] as [(std::string::String, std::string::String); 0]
    );
    pool.disconnect().await.expect("disconnect cascade pool");
}

/// A table that exists only between two catch-up cycles - created and
/// dropped, or created and renamed - is gone from the source by the time its
/// CREATE is replayed. Neither may stop the stream, and a table renamed into
/// the schema from an untracked name must still be mirrored.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn transient_tables_between_cycles_do_not_stop_the_stream() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE anchor (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO anchor VALUES (1,'before');",
        )
        .expect("transient source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("transient DSN"));
    let report = probe(&pool, "app").await.expect("probe transient source");
    let workspace = tempfile::tempdir().expect("transient workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("transient metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register transient source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("transient table schema"),
                StoreOptions::default(),
            )
            .expect("transient table store");
            SnapshotTarget::new(source.clone(), store).expect("transient snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("transient initial snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("transient CDC target")
        })
        .collect();

    mysql
        .query_batch(
            "CREATE TABLE scratch (id BIGINT UNSIGNED PRIMARY KEY);\
             INSERT INTO scratch VALUES (1);\
             DROP TABLE scratch;\
             CREATE TABLE staged (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO staged VALUES (1,'staged');\
             RENAME TABLE staged TO swapped;\
             INSERT INTO swapped VALUES (2,'swapped');\
             INSERT INTO anchor VALUES (2,'after');",
        )
        .expect("transient DDL");
    let targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        cdc_target(&targets, "anchor")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .len(),
        2
    );
    assert!(
        targets
            .iter()
            .all(|target| target.source().name != "scratch")
    );
    let swapped = cdc_target(&targets, "swapped")
        .store()
        .snapshot()
        .scan()
        .unwrap()
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(
        swapped,
        [
            vec![Value::UInt64(1), Value::Utf8("staged".to_owned())],
            vec![Value::UInt64(2), Value::Utf8("swapped".to_owned())],
        ]
    );
    pool.disconnect().await.expect("disconnect transient pool");
}

/// A table created under the name of a table dropped earlier - in the same
/// catch-up or a later one - is a new table. Its rows must replace the dropped
/// table's retained ones, whatever its shape and key.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn a_table_recreated_under_a_dropped_name_replaces_the_retained_rows() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE same_cycle (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             INSERT INTO same_cycle VALUES (1,'old'),(2,'old');\
             CREATE TABLE later_cycle (a INT, b INT);\
             INSERT INTO later_cycle VALUES (1,1),(1,1);",
        )
        .expect("recreate source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("recreate DSN"));
    let report = probe(&pool, "app").await.expect("probe recreate source");
    let workspace = tempfile::tempdir().expect("recreate workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    let tables = workspace.path().join("auto-created");
    let metadata = MetaStore::open(&metadata_path).expect("recreate metadata");
    metadata
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register recreate source");
    drop(metadata);
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                pintail_store::table_directory(&tables, &source.name),
                source.table_schema().expect("recreate table schema"),
                StoreOptions::default(),
            )
            .expect("recreate table store");
            SnapshotTarget::new(source.clone(), store).expect("recreate snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("recreate initial snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("recreate CDC target")
        })
        .collect();

    mysql
        .query_batch(
            "DROP TABLE same_cycle;\
             CREATE TABLE same_cycle (code VARCHAR(8) PRIMARY KEY, amount INT);\
             INSERT INTO same_cycle VALUES ('new', 7);\
             DROP TABLE later_cycle;",
        )
        .expect("drop and recreate");
    let targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    let rows = |targets: &[CdcTarget], name: &str| {
        let target = targets
            .iter()
            .rev()
            .find(|target| target.source().name == name)
            .unwrap_or_else(|| panic!("missing CDC target {name}"));
        target
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(&targets, "same_cycle"),
        [vec![Value::Utf8("new".to_owned()), Value::Int64(7)]]
    );

    // A later catch-up opens only tables that are still streaming, as the
    // supervisor does, so the dropped one is no longer a target.
    let metadata = MetaStore::open(&metadata_path).expect("inspect orphans");
    let orphaned = metadata
        .tables(DATABASE_ID)
        .expect("table rows")
        .into_iter()
        .filter(|table| table.orphaned_at.is_some())
        .map(|table| table.name)
        .collect::<Vec<_>>();
    drop(metadata);
    assert_eq!(orphaned, ["later_cycle"]);
    // The superseded generation of same_cycle is still in the returned list
    // ahead of the new one; a fresh cycle would open only the new one.
    let mut latest = BTreeMap::new();
    for target in targets {
        if !orphaned.contains(&target.source().name) {
            latest.insert(target.source().name.clone(), target);
        }
    }
    let targets = latest.into_values().collect::<Vec<_>>();
    let report = probe(&pool, "app").await.expect("reprobe after the drop");
    mysql
        .query_batch(
            "CREATE TABLE later_cycle (id BIGINT AUTO_INCREMENT PRIMARY KEY, b INT);\
             INSERT INTO later_cycle VALUES (NULL, 5);",
        )
        .expect("recreate later");
    let targets = ddl_catch_up(&pool, &metadata_path, &report, targets, workspace.path()).await;
    assert_eq!(
        rows(&targets, "later_cycle"),
        [vec![Value::Int64(1), Value::Int64(5)]]
    );
    let metadata = MetaStore::open(&metadata_path).expect("inspect recreated rows");
    for name in ["same_cycle", "later_cycle"] {
        let history = metadata.schema_history(DATABASE_ID, name).expect("history");
        assert_eq!(
            history
                .iter()
                .map(|record| record.version)
                .collect::<Vec<_>>(),
            [1],
            "{name} keeps only the new generation's history"
        );
    }
    pool.disconnect().await.expect("disconnect recreate pool");
}

async fn ddl_catch_up(
    pool: &Pool,
    metadata_path: &Path,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
    workspace: &Path,
) -> Vec<CdcTarget> {
    run_cdc(
        pool,
        metadata_path,
        DATABASE_ID,
        report,
        targets,
        CdcOptions {
            blocking: false,
            new_table_root: Some(workspace.join("auto-created")),
            ..CdcOptions::default()
        },
    )
    .await
    .expect("DDL finite catch-up")
    .targets
}

fn cdc_target<'a>(targets: &'a [CdcTarget], name: &str) -> &'a CdcTarget {
    targets
        .iter()
        .find(|target| target.source().name == name)
        .unwrap_or_else(|| panic!("missing CDC target {name}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn cdc_restart_survives_sigkill_during_sustained_writes() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE restart_events (\
               id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64)\
             );",
        )
        .expect("restart source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("restart DSN"));
    let report = probe(&pool, "app").await.expect("probe restart source");
    let workspace = tempfile::tempdir().expect("restart workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("restart metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register restart source");
    let source = report.tables.first().expect("restart table");
    let store = TableStore::open(
        workspace.path().join("restart_events"),
        source.table_schema().expect("restart schema"),
        StoreOptions::default(),
    )
    .expect("restart store");
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![SnapshotTarget::new(source.clone(), store).expect("restart snapshot target")],
        SnapshotOptions::default(),
    )
    .await
    .expect("restart baseline snapshot");
    drop(snapshot);

    let mut writer_gate = pool
        .get_conn()
        .await
        .expect("open sustained-writer gate connection");
    let acquired: Option<i64> = writer_gate
        .query_first(format!("SELECT GET_LOCK('{RESTART_WRITER_GATE}', 30)"))
        .await
        .expect("acquire sustained-writer gate");
    assert_eq!(acquired, Some(1), "sustained-writer gate must be acquired");
    let mut writer = spawn_sustained_writes(&mysql, 200, 10, RESTART_WRITER_GATE);
    kill_cdc_worker(&mysql.dsn(), &metadata_path, workspace.path(), DATABASE_ID);
    assert!(
        writer
            .try_wait()
            .expect("inspect sustained writer")
            .is_none(),
        "source writer must still be active when CDC is killed"
    );
    let released: Option<i64> = writer_gate
        .query_first(format!("SELECT RELEASE_LOCK('{RESTART_WRITER_GATE}')"))
        .await
        .expect("release sustained-writer gate");
    assert_eq!(released, Some(1), "sustained-writer gate must be released");
    drop(writer_gate);
    assert!(
        writer.wait().expect("wait sustained writer").success(),
        "sustained source writes must complete"
    );

    let store = TableStore::open(
        workspace.path().join("restart_events"),
        source.table_schema().expect("recovery schema"),
        StoreOptions::default(),
    )
    .expect("reopen killed store");
    let recovered = finite_catch_up(
        &pool,
        &metadata_path,
        &report,
        vec![CdcTarget::new(source.clone(), store).expect("recovery target")],
    )
    .await
    .expect("restart catch-up");
    let rows = recovered.targets[0]
        .store()
        .snapshot()
        .scan()
        .expect("restart rows");
    assert_eq!(rows.len(), 200);
    assert_eq!(
        rows.iter()
            .map(|row| match row.values()[0] {
                Value::UInt64(id) => id,
                ref value => panic!("unexpected restart ID {value:?}"),
            })
            .sum::<u64>(),
        19_900
    );
    pool.disconnect().await.expect("disconnect restart pool");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "helper process for the cdc-restart SIGKILL gate"]
async fn cdc_crash_worker() {
    let Ok(dsn) = std::env::var("PINTAIL_CDC_CRASH_DSN") else {
        return;
    };
    let metadata_path = std::env::var("PINTAIL_CDC_CRASH_META").expect("crash metadata");
    let workspace = std::env::var("PINTAIL_CDC_CRASH_WORKSPACE").expect("crash workspace");
    let database_id = std::env::var("PINTAIL_CDC_CRASH_DATABASE").expect("crash database");
    let acknowledgement = std::env::var("PINTAIL_CDC_CRASH_ACK").expect("crash acknowledgement");
    let acknowledgement = Arc::new(Mutex::new(
        TcpStream::connect(acknowledgement).expect("connect crash acknowledgement"),
    ));
    let pool = Pool::new(Opts::from_url(&dsn).expect("crash worker DSN"));
    let report = probe(&pool, "app").await.expect("crash worker probe");
    let targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                Path::new(&workspace).join(&source.name),
                source.table_schema().expect("crash source schema"),
                StoreOptions::default(),
            )
            .expect("crash worker store");
            CdcTarget::new(source.clone(), store).expect("crash worker target")
        })
        .collect();
    let _result = pintail_cdc::run_cdc_with_progress(
        &pool,
        Path::new(&metadata_path),
        &database_id,
        &report,
        targets,
        CdcOptions::default(),
        move |_| {
            acknowledgement
                .lock()
                .expect("crash acknowledgement lock")
                .write_all(&[1])
                .expect("write crash acknowledgement");
            thread::sleep(Duration::from_millis(20));
        },
    )
    .await;
}

struct CompatibilityVariant {
    label: &'static str,
    image: &'static str,
    client: &'static str,
    arguments: &'static [&'static str],
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MySQL 5.7/8.4 and MariaDB 11 on the configured Docker host"]
async fn cdc_compatibility_matrix_covers_file_position_mariadb_and_myisam() {
    let variants = [
        CompatibilityVariant {
            label: "mysql84-filepos",
            image: "mysql:8.4",
            client: "mysql",
            arguments: &[
                "--server-id=284",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        },
        CompatibilityVariant {
            label: "mysql84-minimal-metadata",
            image: "mysql:8.4",
            client: "mysql",
            arguments: &[
                "--server-id=286",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=MINIMAL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        },
        CompatibilityVariant {
            label: "mysql57-filepos",
            image: "mysql:5.7",
            client: "mysql",
            arguments: &[
                "--server-id=257",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        },
        CompatibilityVariant {
            label: "mariadb11-gtid-fallback",
            image: "mariadb:11",
            client: "mariadb",
            arguments: &[
                "--server-id=211",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--gtid-strict-mode=ON",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
        },
    ];
    let selected = std::env::var("PINTAIL_CDC_VARIANT").ok();
    for variant in variants {
        if selected
            .as_deref()
            .is_some_and(|selected| selected != variant.label)
        {
            continue;
        }
        run_compatibility_variant(&variant).await;
    }
}

async fn run_compatibility_variant(variant: &CompatibilityVariant) {
    let mysql = MysqlContainer::start_variant(
        variant.label,
        variant.image,
        variant.client,
        variant.arguments,
    )
    .unwrap_or_else(|error| panic!("{}: {error}", variant.label));
    mysql
        .query_batch(compatibility_schema())
        .unwrap_or_else(|error| panic!("{} schema: {error}", variant.label));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("compatibility DSN"));
    let report = probe(&pool, "app")
        .await
        .unwrap_or_else(|error| panic!("{} probe: {error}", variant.label));
    // Every matrix source is CDC-capable: ROW + FULL row image + grants.
    // binlog_row_metadata (absent on 5.7/MariaDB, MINIMAL in the dedicated
    // variant) must not demote the recommendation — the decoder works from
    // the probed schema, not binlog optional metadata.
    assert!(
        matches!(report.capabilities.recommended_mode, RecommendedMode::Cdc),
        "{} expected a CDC recommendation, got {:?}: {:?}",
        variant.label,
        report.capabilities.recommended_mode,
        report.capabilities.reasons
    );
    let workspace = tempfile::tempdir().expect("compatibility workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("compatibility metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register compatibility source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("compatibility schema"),
                StoreOptions::default(),
            )
            .expect("compatibility store");
            SnapshotTarget::new(source.clone(), store).expect("compatibility snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .unwrap_or_else(|error| panic!("{} snapshot: {error}", variant.label));
    mysql
        .query_batch(compatibility_mutations())
        .unwrap_or_else(|error| panic!("{} mutations: {error}", variant.label));
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("compatibility CDC target")
        })
        .collect();
    let result = finite_catch_up(&pool, &metadata_path, &report, targets)
        .await
        .unwrap_or_else(|error| panic!("{} CDC: {error}", variant.label));
    let flagged = MetaStore::open(&metadata_path)
        .expect("compatibility metadata state")
        .tables(DATABASE_ID)
        .expect("compatibility table state")
        .into_iter()
        .filter(|table| table.state == "needs_resync")
        .map(|table| format!("{}: {}", table.name, table.last_error.unwrap_or_default()))
        .collect::<Vec<_>>();
    assert!(flagged.is_empty(), "{} flagged {flagged:?}", variant.label);
    assert!(dlq_errors(&metadata_path).is_empty(), "{}", variant.label);
    assert_eq!(result.checkpoint.kind, "filepos", "{}", variant.label);
    assert_eq!(result.mutations, 9, "{}", variant.label);
    assert_compatibility_rows(variant.label, &result.targets);
    let maintains_virtual = !variant.label.starts_with("mariadb");
    assert_virtual_column_rows(variant.label, &report, &result.targets, maintains_virtual);
    assert_virtual_column_added_mid_stream_is_recopied(
        variant.label,
        &mysql,
        &pool,
        &metadata_path,
        workspace.path(),
        result.targets,
        maintains_virtual,
    )
    .await;
    pool.disconnect()
        .await
        .expect("disconnect compatibility pool");
}

/// `MySQL` writes a `VIRTUAL` generated column into every row image, so the
/// column replicates like any other. `MariaDB` leaves its value out of UPDATE
/// after-images, so the probe skips it and records its position; the wider
/// images it does write decode by ordinal around the gap.
fn assert_virtual_column_rows(
    label: &str,
    report: &ProbeReport,
    targets: &[CdcTarget],
    maintains_virtual: bool,
) {
    let probed = report
        .tables
        .iter()
        .find(|table| table.name == "compat_generated")
        .expect("probed generated table");
    let target = cdc_target(targets, "compat_generated");
    let rows = target.store().snapshot().scan().expect("generated scan");
    assert_eq!(rows.len(), 2, "{label}");
    assert_eq!(
        probed.source_column_count, 3,
        "{label} records the source layout, skipped columns included"
    );
    if maintains_virtual {
        assert_eq!(probed.columns.len(), 3, "{label} keeps the virtual column");
        assert!(
            probed
                .warnings
                .iter()
                .all(|warning| !warning.contains("virtual generated")),
            "{label}: {:?}",
            probed.warnings
        );
        assert_eq!(
            probed
                .columns
                .iter()
                .map(|column| column.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "{label}"
        );
        // The snapshot row (id 1, then updated) and the streamed insert both
        // carry the computed value at the column's own ordinal.
        for row in &rows {
            let Value::Utf8(value) = &row.values()[1] else {
                panic!("{label}: value is text");
            };
            assert_eq!(
                row.values()[2],
                Value::Utf8(value.to_uppercase()),
                "{label} row {:?}",
                row.values()
            );
        }
    } else {
        assert_eq!(probed.columns.len(), 2, "{label} skips the virtual column");
        assert!(
            probed
                .warnings
                .iter()
                .any(|warning| warning.contains("virtual generated column value_upper")),
            "{label}: {:?}",
            probed.warnings
        );
        assert_eq!(
            probed
                .columns
                .iter()
                .map(|column| column.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "{label}"
        );
        for row in &rows {
            assert_eq!(row.values().len(), 2, "{label}");
        }
        assert_eq!(
            rows[0].values()[1],
            Value::Utf8("updated".to_owned()),
            "{label}"
        );
    }
}

/// A `VIRTUAL` column added to a live table cannot evolve in place - the
/// rows already copied would read NULL where the source computes values -
/// so the stream quarantines the table, and the recopy that follows carries
/// the values for every row. Where the server cannot keep the column current
/// the probe skips it, the wider images decode around it, and the table
/// stays live.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn assert_virtual_column_added_mid_stream_is_recopied(
    label: &str,
    mysql: &MysqlContainer,
    pool: &Pool,
    metadata_path: &Path,
    workspace: &Path,
    targets: Vec<CdcTarget>,
    maintains_virtual: bool,
) {
    mysql
        .query_batch(
            "ALTER TABLE compat_generated \
               ADD COLUMN value_len INT GENERATED ALWAYS AS (CHAR_LENGTH(value)) VIRTUAL;\
             INSERT INTO compat_generated (id, value) VALUES (4,'late');",
        )
        .unwrap_or_else(|error| panic!("{label} mid-stream ALTER: {error}"));
    let report = probe(pool, "app")
        .await
        .unwrap_or_else(|error| panic!("{label} re-probe: {error}"));
    let result = finite_catch_up(pool, metadata_path, &report, targets)
        .await
        .unwrap_or_else(|error| panic!("{label} CDC after ALTER: {error}"));
    let metadata = MetaStore::open(metadata_path).expect("metadata");
    let flagged = metadata
        .tables(DATABASE_ID)
        .expect("tables")
        .into_iter()
        .filter(|table| table.state == "needs_resync")
        .map(|table| (table.name, table.last_error.unwrap_or_default()))
        .collect::<Vec<_>>();
    if !maintains_virtual {
        assert!(flagged.is_empty(), "{label}: {flagged:?}");
        let target = cdc_target(&result.targets, "compat_generated");
        assert_eq!(target.source().source_column_count, 4, "{label}");
        let rows = target.store().snapshot().scan().expect("generated scan");
        assert_eq!(
            rows.len(),
            3,
            "{label}: the insert streamed through the wider image"
        );
        assert!(rows.iter().all(|row| row.values().len() == 2), "{label}");
        return;
    }
    assert_eq!(flagged.len(), 1, "{label}: {flagged:?}");
    assert_eq!(flagged[0].0, "compat_generated", "{label}");
    assert!(
        flagged[0].1.contains(
            "column value_len joined the schema with values the rows already copied need"
        ),
        "{label}: the table is recopied for the right reason, not a parse failure: {}",
        flagged[0].1
    );
    assert_eq!(
        cdc_target(&result.targets, "compat_generated")
            .source()
            .columns
            .len(),
        3,
        "{label}: the live schema did not adopt the column"
    );
    // The recopy the automatic resync would run: the refreshed schema, a
    // fresh store, one table's snapshot, then the stream again.
    let source = report
        .tables
        .iter()
        .find(|table| table.name == "compat_generated")
        .expect("refreshed generated table")
        .clone();
    assert_eq!(source.columns.len(), 4, "{label}");
    let directory = workspace.join("compat_generated-recopy");
    let store = TableStore::open(
        &directory,
        source.table_schema().expect("refreshed schema"),
        StoreOptions::default(),
    )
    .expect("recopy store");
    metadata
        .begin_table_resnapshot(DATABASE_ID, "compat_generated")
        .expect("resync begins");
    let snapshot = run_snapshot(
        pool,
        metadata_path,
        DATABASE_ID,
        &report,
        vec![SnapshotTarget::new(source, store).expect("recopy target")],
        SnapshotOptions::default(),
    )
    .await
    .unwrap_or_else(|error| panic!("{label} recopy: {error}"));
    metadata
        .finish_table_resnapshot(DATABASE_ID, "compat_generated", "streaming")
        .expect("resync finishes");
    let rows = snapshot.targets[0]
        .store()
        .snapshot()
        .scan()
        .expect("recopied scan");
    assert_eq!(rows.len(), 3, "{label}");
    for row in &rows {
        let Value::Utf8(value) = &row.values()[1] else {
            panic!("{label}: value is text");
        };
        assert_eq!(
            row.values()[2],
            Value::Utf8(value.to_uppercase()),
            "{label}"
        );
        assert_eq!(
            row.values()[3],
            Value::Int64(i64::try_from(value.chars().count()).expect("length")),
            "{label}: every row carries the new column's value"
        );
    }
}

fn dlq_errors(metadata_path: &Path) -> Vec<String> {
    let connection = Connection::open(metadata_path).expect("open diagnostic metadata");
    let mut statement = connection
        .prepare("SELECT error FROM dlq ORDER BY created_at, id")
        .expect("prepare DLQ diagnostics");
    statement
        .query_map([], |row| row.get(0))
        .expect("query DLQ diagnostics")
        .collect::<rusqlite::Result<_>>()
        .expect("decode DLQ diagnostics")
}

fn assert_compatibility_rows(label: &str, targets: &[CdcTarget]) {
    let targets = targets
        .iter()
        .map(|target| (target.source().name.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let events = targets["compat_events"];
    let rows = events
        .store()
        .snapshot()
        .scan()
        .expect("compatibility event scan");
    assert_eq!(rows.len(), 2, "{label}");
    assert_eq!(rows[0].values()[1], Value::Utf8("updated".to_owned()));
    let columns = events
        .source()
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    // MySQL returns the all-zero date from a SELECT, does not match it with
    // IS NULL, and counts it in COUNT(column); the replica preserves it
    // rather than inverting all three.
    assert_eq!(
        rows[1].values()[columns["date_value"]],
        Value::Utf8("0000-00-00".to_owned()),
        "{label}"
    );
    assert_eq!(
        rows[1].values()[columns["datetime_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned()),
        "{label}"
    );
    assert_eq!(
        rows[1].values()[columns["timestamp_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned()),
        "{label}"
    );
    assert_eq!(
        rows[1].values()[columns["latin_value"]],
        Value::Utf8("café".to_owned()),
        "{label}"
    );
    assert_eq!(
        rows[1].values()[columns["enum_value"]],
        Value::Utf8("βeta".to_owned()),
        "{label}"
    );
    assert_eq!(
        rows[1].values()[columns["set_value"]],
        Value::Utf8("red,blue".to_owned()),
        "{label}"
    );
    // Unsigned boundary values must decode bit-exactly through the binlog
    // regardless of binlog_row_metadata: the CDC UPDATE rewrote row 1 and the
    // CDC INSERT created row 3, so both row images crossed the decoder.
    for (column, updated, inserted) in [
        ("utiny_value", 200_u64, 255_u64),
        ("usmall_value", 65_535, 65_534),
        ("umedium_value", 16_777_215, 16_777_214),
        ("uint_value", 3_000_000_000, 4_294_967_295),
        ("ubig_value", u64::MAX, 9_223_372_036_854_775_808),
    ] {
        assert_eq!(
            rows[0].values()[columns[column]],
            Value::UInt64(updated),
            "{label} {column} update image"
        );
        assert_eq!(
            rows[1].values()[columns[column]],
            Value::UInt64(inserted),
            "{label} {column} insert image"
        );
    }
    let myisam = targets["myisam_events"]
        .store()
        .snapshot()
        .scan()
        .expect("MyISAM scan");
    assert_eq!(myisam.len(), 1, "{label}");
    assert_eq!(myisam[0].values()[1], Value::Utf8("updated".to_owned()));
}

/// A DDL this engine cannot parse quarantines the tables it names and the
/// stream moves past it. "Past it" has to reach the checkpoint: a statement
/// nothing follows would otherwise be read again by every later pass, which
/// quarantines the same tables again and holds the rows before it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn an_unreadable_ddl_with_nothing_after_it_still_advances_the_checkpoint() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE quarantine_me (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(32), \
               INDEX value_idx (value));\
             INSERT INTO quarantine_me VALUES (1,'before');",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("unreadable DSN"));
    let report = probe(&pool, "app").await.expect("probe unreadable source");
    let workspace = tempfile::tempdir().expect("unreadable workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("unreadable metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register unreadable source");
    let source = report.tables.first().expect("quarantine_me source");
    let store = TableStore::open(
        workspace.path().join("quarantine_me"),
        source.table_schema().expect("quarantine_me schema"),
        StoreOptions::default(),
    )
    .expect("quarantine_me store");
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![SnapshotTarget::new(source.clone(), store).expect("unreadable snapshot target")],
        SnapshotOptions::default(),
    )
    .await
    .expect("unreadable baseline snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("unreadable CDC target")
        })
        .collect::<Vec<_>>();

    // Valid MySQL that this engine's parser refuses, and the last event in
    // the log.
    mysql
        .query_batch("ALTER TABLE quarantine_me ALTER INDEX value_idx INVISIBLE;")
        .expect("unreadable DDL");
    let result = finite_catch_up(&pool, &metadata_path, &report, targets)
        .await
        .expect("catch up past the unreadable DDL");
    let flagged = MetaStore::open(&metadata_path)
        .expect("unreadable state")
        .tables(DATABASE_ID)
        .expect("unreadable tables")
        .into_iter()
        .filter(|table| table.state == "needs_resync")
        .map(|table| table.name)
        .collect::<Vec<_>>();
    assert_eq!(flagged, ["quarantine_me"], "the statement's table resyncs");
    let status = mysql
        .query_batch("SHOW BINARY LOG STATUS;")
        .expect("source position");
    let mut fields = status.split_whitespace();
    let file = fields.next().expect("binlog file").to_owned();
    let pos: u64 = fields
        .next()
        .expect("binlog position")
        .parse()
        .expect("binlog position is a number");
    assert_eq!(result.checkpoint.binlog_file, file);
    assert_eq!(
        result.checkpoint.binlog_pos, pos,
        "the checkpoint names the source's own position, so no later pass reads the statement again"
    );
}

/// A followed catch-up stays on the stream after the end of the source's
/// log: a transaction written once the backlog is stored is applied by the
/// same catch-up, and a stop raised while the stream is quiet ends it. The
/// backlog's large batch is stored off the stream while the events after it
/// are read ahead - row events, a transaction on another table, and a
/// schema change that has to wait for the batch - and the mirror still
/// equals the source row for row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn a_followed_catch_up_reads_ahead_of_a_large_batch_and_applies_what_arrives_after() {
    let mysql = Arc::new(MysqlContainer::start().unwrap_or_else(|error| panic!("{error}")));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE ahead_other (id BIGINT UNSIGNED PRIMARY KEY, note VARCHAR(32) NOT NULL);\
             CREATE TABLE ahead_rows (id BIGINT UNSIGNED PRIMARY KEY, bucket BIGINT NOT NULL, \
               note VARCHAR(32) NOT NULL);\
             INSERT INTO ahead_other VALUES (1,'seed');\
             INSERT INTO ahead_rows VALUES (1,0,'seed');",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("ahead DSN"));
    let report = probe(&pool, "app").await.expect("probe ahead source");
    let workspace = tempfile::tempdir().expect("ahead workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("ahead metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register ahead source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("ahead schema"),
                StoreOptions::default(),
            )
            .expect("ahead store");
            SnapshotTarget::new(source.clone(), store).expect("ahead snapshot target")
        })
        .collect::<Vec<_>>();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("ahead baseline snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("ahead CDC target")
        })
        .collect::<Vec<_>>();

    // The backlog: two transactions large enough to be stored off the
    // stream, then one on another table, a schema change, and two more.
    mysql
        .query_batch(
            "SET SESSION cte_max_recursion_depth = 100000;\
             INSERT INTO ahead_rows \
               WITH RECURSIVE seq(n) AS (SELECT 2 UNION ALL SELECT n + 1 FROM seq WHERE n < 6001) \
               SELECT n, MOD(n, 7), CONCAT('row-', n) FROM seq;\
             UPDATE ahead_rows SET bucket = bucket + 1;\
             INSERT INTO ahead_other VALUES (2,'two'),(3,'three');\
             ALTER TABLE ahead_rows ADD COLUMN extra BIGINT NULL;\
             UPDATE ahead_rows SET extra = id * 3 WHERE id <= 5000;\
             DELETE FROM ahead_rows WHERE id > 5990;",
        )
        .expect("ahead backlog");
    let status = mysql
        .query_batch("SHOW BINARY LOG STATUS;")
        .expect("source position");
    let mut fields = status.split_whitespace();
    let backlog_file = fields.next().expect("binlog file").to_owned();
    let backlog_pos: u64 = fields
        .next()
        .expect("binlog position")
        .parse()
        .expect("binlog position is a number");

    // The late write goes in only after the catch-up has stored the whole
    // backlog, so a catch-up that returned at the end of the log could not
    // have read it; the stop goes up once the late write is stored too.
    let backlog_stored = Arc::new(AtomicBool::new(false));
    let backlog_mutations = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let stop = pintail_cdc::CycleStop::new();
    let late = {
        let mysql = Arc::clone(&mysql);
        let backlog_stored = Arc::clone(&backlog_stored);
        thread::spawn(move || {
            let waited = std::time::Instant::now();
            while !backlog_stored.load(Ordering::Acquire) {
                assert!(
                    waited.elapsed() < Duration::from_secs(120),
                    "the backlog was never stored"
                );
                thread::sleep(Duration::from_millis(10));
            }
            mysql
                .query_batch("INSERT INTO ahead_other VALUES (100,'late');")
                .expect("late write");
        })
    };
    let read_ahead_before = pintail_cdc::batches_read_ahead();
    let result = {
        let backlog_stored = Arc::clone(&backlog_stored);
        let backlog_mutations = Arc::clone(&backlog_mutations);
        let stop_when_late = stop.clone();
        pintail_cdc::run_cdc_with_progress(
            &pool,
            &metadata_path,
            DATABASE_ID,
            &report,
            targets,
            CdcOptions {
                blocking: false,
                follow: Some(Duration::from_secs(120)),
                stop: Some(stop.clone()),
                ..CdcOptions::default()
            },
            move |progress| {
                let past_backlog = progress.checkpoint.binlog_file > backlog_file
                    || (progress.checkpoint.binlog_file == backlog_file
                        && progress.checkpoint.binlog_pos >= backlog_pos);
                if !past_backlog {
                    return;
                }
                if backlog_stored.swap(true, Ordering::AcqRel) {
                    if progress.mutations > backlog_mutations.load(Ordering::Acquire) {
                        stop_when_late.request();
                    }
                } else {
                    backlog_mutations.store(progress.mutations, Ordering::Release);
                }
            },
        )
        .await
        .expect("followed catch-up")
    };
    late.join().expect("late writer");

    assert!(
        pintail_cdc::batches_read_ahead() > read_ahead_before,
        "the backlog's large batch was stored off the stream"
    );
    let text = |table: &str| {
        let target = cdc_target(&result.targets, table);
        let mut lines = target
            .store()
            .snapshot()
            .scan()
            .expect("ahead scan")
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|value| match value {
                        Value::Null => "NULL".to_owned(),
                        Value::Int64(value) => value.to_string(),
                        Value::UInt64(value) => value.to_string(),
                        Value::Utf8(value) => value.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>();
        lines.sort();
        lines.join("\n")
    };
    assert_eq!(
        text("ahead_other"),
        source_text(&mysql, "SELECT id, note FROM ahead_other;"),
        "the late write was applied by the catch-up that was following"
    );
    let rows = text("ahead_rows");
    assert_eq!(rows.lines().count(), 5990);
    assert_eq!(
        rows,
        source_text(&mysql, "SELECT id, bucket, note, extra FROM ahead_rows;")
    );
}

/// A `MODIFY` whose declaration permits a rewrite - a new `NOT NULL`, a
/// narrower integer, a shorter string, a dropped `ENUM` member - is adopted
/// in place when no row the mirror holds is one the source rewrote, and
/// recopied when one is: a row copied before the stream, a row the stream
/// stored in the same pass, or any row at all once the source has changed
/// the column again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn a_narrowing_that_rewrote_nothing_is_adopted_in_place() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE null_kept (id BIGINT PRIMARY KEY, v INT NULL);\
             INSERT INTO null_kept VALUES (1,5),(2,0);\
             CREATE TABLE null_lost (id BIGINT PRIMARY KEY, v INT NULL);\
             INSERT INTO null_lost VALUES (1,5),(2,NULL);\
             CREATE TABLE int_kept (id BIGINT PRIMARY KEY, v BIGINT NULL);\
             INSERT INTO int_kept VALUES (1,32767),(2,-32768),(3,NULL);\
             CREATE TABLE int_lost (id BIGINT PRIMARY KEY, v BIGINT NULL);\
             INSERT INTO int_lost VALUES (1,32768),(2,7);\
             CREATE TABLE int_streamed (id BIGINT PRIMARY KEY, v BIGINT NULL);\
             INSERT INTO int_streamed VALUES (1,7);\
             CREATE TABLE text_kept (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL);\
             INSERT INTO text_kept VALUES (1,'abcdefgh'),(2,'ab   '),(3,'🦆🦆🦆🦆🦆🦆🦆é');\
             CREATE TABLE text_lost (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL);\
             INSERT INTO text_lost VALUES (1,'abcdefghi'),(2,'ab');\
             CREATE TABLE text_moved (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL);\
             INSERT INTO text_moved VALUES (1,'ab');\
             CREATE TABLE enum_kept (id BIGINT PRIMARY KEY, v ENUM('alpha','beta','gamma') NULL);\
             INSERT INTO enum_kept VALUES (1,'alpha'),(2,'gamma'),(3,NULL);\
             CREATE TABLE enum_lost (id BIGINT PRIMARY KEY, v ENUM('alpha','beta','gamma') NULL);\
             INSERT INTO enum_lost VALUES (1,'alpha'),(2,'beta');",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("narrowing DSN"));
    let report = probe(&pool, "app").await.expect("probe narrowing source");
    let workspace = tempfile::tempdir().expect("narrowing workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("narrowing metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register narrowing source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("narrowing table schema"),
                StoreOptions::default(),
            )
            .expect("narrowing table store");
            SnapshotTarget::new(source.clone(), store).expect("narrowing snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("narrowing baseline snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("narrowing CDC target")
        })
        .collect::<Vec<_>>();

    // The permissive mode lets the source clip instead of refusing, which
    // is the case a mirror has to notice on its own.
    mysql
        .query_batch(
            "SET sql_mode = 'NO_ENGINE_SUBSTITUTION';\
             ALTER TABLE null_kept MODIFY v INT NOT NULL;\
             ALTER TABLE null_lost MODIFY v INT NOT NULL;\
             INSERT INTO int_kept VALUES (4,-1);\
             ALTER TABLE int_kept MODIFY v SMALLINT NULL;\
             ALTER TABLE int_lost MODIFY v SMALLINT NULL;\
             INSERT INTO int_streamed VALUES (2,99999);\
             ALTER TABLE int_streamed MODIFY v SMALLINT NULL;\
             ALTER TABLE text_kept MODIFY v VARCHAR(8) NULL;\
             ALTER TABLE text_lost MODIFY v VARCHAR(8) NULL;\
             ALTER TABLE text_moved MODIFY v VARCHAR(8) NULL;\
             ALTER TABLE text_moved MODIFY v VARCHAR(64) NULL;\
             ALTER TABLE enum_kept MODIFY v ENUM('alpha','gamma') NULL;\
             ALTER TABLE enum_lost MODIFY v ENUM('alpha','gamma') NULL;\
             INSERT INTO null_kept VALUES (3,9);\
             INSERT INTO int_kept VALUES (5,-32768);\
             INSERT INTO text_kept VALUES (4,'newvalue');\
             INSERT INTO enum_kept VALUES (4,'gamma');",
        )
        .expect("narrowing statements");
    let result = finite_catch_up(&pool, &metadata_path, &report, targets)
        .await
        .expect("catch up past the narrowings");
    let flagged = MetaStore::open(&metadata_path)
        .expect("narrowing state")
        .tables(DATABASE_ID)
        .expect("narrowing tables")
        .into_iter()
        .filter(|table| table.state == "needs_resync")
        .map(|table| table.name)
        .collect::<Vec<_>>();
    assert_eq!(
        flagged,
        [
            "enum_lost",
            "int_lost",
            "int_streamed",
            "null_lost",
            "text_lost",
            "text_moved"
        ],
        "exactly the tables holding a rewritten value, or changed again, are recopied"
    );
    let values = |name: &str| {
        cdc_target(&result.targets, name)
            .store()
            .snapshot()
            .scan()
            .expect("narrowed rows")
            .iter()
            .map(|row| row.values()[1].clone())
            .collect::<Vec<_>>()
    };
    for name in ["null_kept", "int_kept", "text_kept", "enum_kept"] {
        assert_eq!(
            cdc_target(&result.targets, name).store().schema().version(),
            2,
            "{name} took the new declaration as a schema generation"
        );
    }
    assert!(
        !cdc_target(&result.targets, "null_kept").source().columns[1].nullable,
        "the tracked column is NOT NULL too"
    );
    assert_eq!(
        values("null_kept"),
        [Value::Int64(5), Value::Int64(0), Value::Int64(9)]
    );
    assert_eq!(
        values("int_kept"),
        [
            Value::Int64(32767),
            Value::Int64(-32768),
            Value::Null,
            Value::Int64(-1),
            Value::Int64(-32768)
        ]
    );
    assert_eq!(
        values("text_kept"),
        [
            Value::Utf8("abcdefgh".to_owned()),
            Value::Utf8("ab   ".to_owned()),
            Value::Utf8("🦆🦆🦆🦆🦆🦆🦆é".to_owned()),
            Value::Utf8("newvalue".to_owned())
        ]
    );
    let labels = values("enum_kept")
        .into_iter()
        .map(|value| match value {
            Value::Enum { label, .. } | Value::Utf8(label) => label,
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, ["alpha", "gamma", "Null", "gamma"]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn purged_file_position_marks_the_source_for_resnapshot() {
    let mysql = MysqlContainer::start_file_position().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE events (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(32));\
             INSERT INTO events VALUES (1,'before');",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("purge DSN"));
    let report = probe(&pool, "app").await.expect("probe purge source");
    let workspace = tempfile::tempdir().expect("purge workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("purge metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register purge source");
    let source = report.tables.first().expect("events source");
    let store = TableStore::open(
        workspace.path().join("events"),
        source.table_schema().expect("events schema"),
        StoreOptions::default(),
    )
    .expect("events store");
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![SnapshotTarget::new(source.clone(), store).expect("purge snapshot target")],
        SnapshotOptions::default(),
    )
    .await
    .expect("purge baseline snapshot");
    let captured_file = match &snapshot.position {
        pintail_snapshot::SnapshotPosition::FilePosition { file, .. } => file.clone(),
        _ => panic!("file-position source must capture a file checkpoint"),
    };
    mysql
        .query_batch("INSERT INTO events VALUES (2,'lost'); FLUSH BINARY LOGS;")
        .expect("rotate captured binlog");
    let active_file = mysql
        .query_batch("SHOW BINARY LOG STATUS;")
        .expect("active binlog")
        .split_whitespace()
        .next()
        .expect("active binlog file")
        .to_owned();
    assert_ne!(captured_file, active_file);
    mysql
        .query_batch(&format!("PURGE BINARY LOGS TO '{active_file}';"))
        .expect("purge captured binlog");
    assert!(
        !mysql
            .query_batch("SHOW BINARY LOGS;")
            .expect("retained binlogs")
            .contains(&captured_file)
    );
    let target = snapshot
        .targets
        .into_iter()
        .next()
        .expect("snapshot target");
    let source = target.source().clone();
    let Err(error) = run_cdc(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![CdcTarget::new(source, target.into_store()).expect("purge CDC target")],
        CdcOptions {
            blocking: false,
            auto_resnapshot: false,
            ..CdcOptions::default()
        },
    )
    .await
    else {
        panic!("purged checkpoint must fail");
    };
    assert!(matches!(error, CdcError::NeedsResync { .. }));
    assert_eq!(
        MetaStore::open(&metadata_path)
            .expect("inspect resync metadata")
            .tables_needing_resync(DATABASE_ID)
            .expect("resync tables"),
        ["events".to_owned()].into_iter().collect()
    );

    assert_auto_resnapshot_recovers(&pool, &metadata_path, workspace.path(), &report).await;
    pool.disconnect().await.expect("disconnect purge pool");
}

async fn assert_auto_resnapshot_recovers(
    pool: &Pool,
    metadata_path: &Path,
    workspace: &Path,
    report: &ProbeReport,
) {
    let source = report.tables.first().expect("recovery source");
    let store = TableStore::open(
        workspace.join("events"),
        source.table_schema().expect("recovery schema"),
        StoreOptions::default(),
    )
    .expect("reopen resnapshot store");
    let recovered = run_cdc(
        pool,
        metadata_path,
        DATABASE_ID,
        report,
        vec![CdcTarget::new(source.clone(), store).expect("recovery target")],
        CdcOptions {
            blocking: false,
            ..CdcOptions::default()
        },
    )
    .await
    .expect("automatic resnapshot recovery");
    let rows = recovered.targets[0]
        .store()
        .snapshot()
        .scan()
        .expect("recovered rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].values()[1], Value::Utf8("lost".to_owned()));
    assert!(
        MetaStore::open(metadata_path)
            .expect("inspect recovery metadata")
            .tables_needing_resync(DATABASE_ID)
            .expect("recovered table states")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn m4_cdc_crud_gipk_append_and_type_fidelity() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(source_schema())
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("CDC DSN"));
    let report = probe(&pool, "app").await.expect("probe CDC source");
    let workspace = tempfile::tempdir().expect("CDC workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register database");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("source schema"),
                StoreOptions::default(),
            )
            .expect("open snapshot store");
            SnapshotTarget::new(source.clone(), store).expect("snapshot target")
        })
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions {
            workers: 3,
            chunk_rows: 2,
            ..SnapshotOptions::default()
        },
    )
    .await
    .expect("initial snapshot");

    mysql
        .query_batch(cdc_mutations())
        .unwrap_or_else(|error| panic!("{error}"));
    let cdc_targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("CDC target")
        })
        .collect();
    let result = run_cdc(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        cdc_targets,
        CdcOptions {
            blocking: false,
            max_transaction_bytes: 1,
            ..CdcOptions::default()
        },
    )
    .await
    .expect("finite CDC catch-up");
    assert!(result.commits >= 4);
    assert_eq!(result.mutations, 7);
    assert_eq!(result.checkpoint.kind, "gtid");
    assert_replica(&result.targets);
    assert_eq!(
        MetaStore::open(&metadata_path)
            .expect("decode failure metadata")
            .tables_needing_resync(DATABASE_ID)
            .expect("decode failure table"),
        ["decode_fail".to_owned()].into_iter().collect()
    );
    let errors = dlq_errors(&metadata_path);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("unsupported binlog character set utf16"));
    assert_restart_replay(
        &mysql,
        &pool,
        &metadata_path,
        workspace.path(),
        &report,
        result.targets,
        result.checkpoint,
    )
    .await;
    pool.disconnect().await.expect("disconnect source pool");
}

async fn assert_restart_replay(
    mysql: &MysqlContainer,
    pool: &Pool,
    metadata_path: &Path,
    workspace: &Path,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
    checkpoint_before: CdcCheckpoint,
) {
    mysql
        .query_batch(
            "START TRANSACTION;\
               UPDATE primary_rows SET value='ONE-again' WHERE id=1;\
               INSERT INTO primary_rows VALUES (5,'five');\
               INSERT INTO append_rows VALUES ('replayed-once');\
               UPDATE gipk_rows SET value='updated-again' WHERE value='updated';\
             COMMIT;",
        )
        .expect("restart mutations");
    let floor = stored_floor(&targets);
    let first = finite_catch_up(pool, metadata_path, report, targets)
        .await
        .expect("first restart catch-up");
    assert_eq!(first.mutations, 4);
    let checkpoint_after = first.checkpoint.clone();
    assert_restart_rows(&first.targets);
    let highest = stored_floor(&first.targets);
    assert!(highest > floor, "the transaction stored newer versions");
    drop(first.targets);

    // The state a process killed between writing the batch and committing
    // its checkpoint leaves behind: the rows stored, the checkpoint still at
    // the batch's start, and the record of what the apply was writing. A
    // checkpoint rewound without that record reads as a source whose
    // numbering restarted, which is answered by a full copy, not a replay.
    let metadata = MetaStore::open(metadata_path).expect("rewind metadata");
    metadata
        .record_cdc_apply_intent(
            DATABASE_ID,
            &CdcApplyIntent {
                floor,
                highest,
                binlog_file: checkpoint_before.binlog_file.clone(),
                binlog_pos: checkpoint_before.binlog_pos,
            },
        )
        .expect("record the unfinished apply");
    metadata
        .upsert_snapshot_checkpoint(
            DATABASE_ID,
            &checkpoint_before.kind,
            checkpoint_before.gtid_set.as_deref(),
            Some(&checkpoint_before.binlog_file),
            Some(checkpoint_before.binlog_pos),
            "2026-07-30T02:00:00Z",
        )
        .expect("rewind durable checkpoint");
    let reopened = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.join(&source.name),
                source.table_schema().expect("reopen schema"),
                StoreOptions::default(),
            )
            .expect("reopen CDC store");
            CdcTarget::new(source.clone(), store).expect("reopened CDC target")
        })
        .collect();
    let replay = finite_catch_up(pool, metadata_path, report, reopened)
        .await
        .expect("replayed catch-up");
    // The stream hands the transaction over again - four mutations read -
    // and every table drops the rows it already holds: the same rows, the
    // same versions, nothing stored twice and no copy from the source.
    assert_eq!(replay.mutations, 4);
    assert_eq!(replay.checkpoint, checkpoint_after);
    assert_restart_rows(&replay.targets);
    assert_eq!(stored_floor(&replay.targets), highest);
    let metadata = MetaStore::open(metadata_path).expect("replayed metadata");
    assert_eq!(
        metadata
            .cdc_apply_intent(DATABASE_ID)
            .expect("apply intent"),
        None,
        "the checkpoint that covers the batch clears its record"
    );
    assert_eq!(
        metadata
            .tables_needing_resync(DATABASE_ID)
            .expect("tables to recopy"),
        ["decode_fail".to_owned()].into_iter().collect(),
        "the replay was not answered by a copy"
    );
}

fn stored_floor(targets: &[CdcTarget]) -> u64 {
    targets
        .iter()
        .filter_map(|target| target.store().snapshot().max_row_version())
        .max()
        .unwrap_or(0)
}

async fn finite_catch_up(
    pool: &Pool,
    metadata_path: &Path,
    report: &ProbeReport,
    targets: Vec<CdcTarget>,
) -> Result<pintail_cdc::CdcResult, pintail_cdc::CdcError> {
    run_cdc(
        pool,
        metadata_path,
        DATABASE_ID,
        report,
        targets,
        CdcOptions {
            blocking: false,
            ..CdcOptions::default()
        },
    )
    .await
}

fn assert_restart_rows(targets: &[CdcTarget]) {
    let targets = targets
        .iter()
        .map(|target| (target.source().name.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let primary = targets["primary_rows"]
        .store()
        .snapshot()
        .scan()
        .expect("restart primary scan");
    assert_eq!(primary.len(), 4);
    assert_eq!(primary[0].values()[1], Value::Utf8("ONE-again".to_owned()));
    let append = targets["append_rows"]
        .store()
        .snapshot()
        .scan()
        .expect("restart append scan");
    assert_eq!(append.len(), 4);
    assert_eq!(
        append
            .iter()
            .filter(|row| row.values()[0] == Value::Utf8("replayed-once".to_owned()))
            .count(),
        1
    );
}

fn assert_replica(targets: &[CdcTarget]) {
    let targets = targets
        .iter()
        .map(|target| (target.source().name.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let primary = targets["primary_rows"]
        .store()
        .snapshot()
        .scan()
        .expect("primary scan");
    assert_eq!(primary.len(), 3);
    assert_eq!(primary[0].values()[1], Value::Utf8("ONE".to_owned()));
    assert_eq!(primary[1].values()[0], Value::UInt64(3));
    assert_eq!(primary[2].values()[1], Value::Utf8("emoji-🪿".to_owned()));

    let append = targets["append_rows"]
        .store()
        .snapshot()
        .scan()
        .expect("append scan");
    assert_eq!(append.len(), 3);
    assert_eq!(append[2].values()[0], Value::Utf8("after".to_owned()));

    let gipk = targets["gipk_rows"]
        .store()
        .snapshot()
        .scan()
        .expect("GIPK scan");
    assert_eq!(gipk.len(), 2);
    assert!(
        gipk.iter()
            .any(|row| row.values().contains(&Value::Utf8("updated".to_owned())))
    );

    assert_type_fidelity(targets["type_rows"]);
    // The rows above were read by their tables' compiled plans. The
    // counters are the process's, so other tests may have added to them.
    let (planned, general, compiled) = pintail_cdc::row_decode_counters();
    assert!(planned >= 6, "{planned} rows events were read by a plan");
    assert!(compiled <= planned, "{compiled} plans for {planned} events");
    assert!(
        general <= planned,
        "{general} rows events were left to the general decoder"
    );
}

fn assert_type_fidelity(types: &CdcTarget) {
    let rows = types.store().snapshot().scan().expect("type fidelity scan");
    assert_eq!(rows.len(), 2);
    let columns = types
        .source()
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    // A BIT column is its unsigned number at every width, and NULL stays
    // NULL: the first row arrived by the copy, the second by the stream.
    let bits = |row: usize| {
        ["bit_value", "bit1", "bit8", "bit10", "bit64"]
            .map(|name| rows[row].values()[columns[name]].clone())
            .to_vec()
    };
    assert_eq!(
        bits(0),
        [
            Value::UInt64(0),
            Value::UInt64(1),
            Value::Null,
            Value::UInt64(682),
            Value::UInt64(u64::MAX),
        ]
    );
    assert_eq!(
        bits(1),
        [
            Value::UInt64(341),
            Value::UInt64(0),
            Value::UInt64(128),
            Value::Null,
            Value::UInt64(0x8000_0000_0000_0001),
        ]
    );
    let row = &rows[1];
    // MySQL returns the all-zero date from a SELECT, does not match it with
    // IS NULL, and counts it in COUNT(column); the replica preserves it
    // rather than inverting all three.
    assert_eq!(
        row.values()[columns["date_value"]],
        Value::Utf8("0000-00-00".to_owned())
    );
    assert_eq!(
        row.values()[columns["datetime_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned())
    );
    assert_eq!(
        row.values()[columns["timestamp_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned())
    );
    assert_eq!(
        row.values()[columns["decimal_value"]],
        Value::Utf8("1234567890123456789012345678.1234567890".to_owned())
    );
    assert_eq!(
        row.values()[columns["latin_value"]],
        Value::Utf8("café".to_owned())
    );
    assert_eq!(
        row.values()[columns["enum_value"]],
        Value::Utf8("βeta".to_owned())
    );
    assert_eq!(
        row.values()[columns["set_value"]],
        Value::Utf8("red,blue".to_owned())
    );
    assert_eq!(
        row.values()[columns["json_value"]],
        Value::Utf8("{\"a\": 1, \"b\": [true, null]}".to_owned())
    );
    assert_eq!(
        row.values()[columns["binary_value"]],
        Value::Binary(vec![0, 255, 16])
    );
    assert_eq!(
        row.values()[columns["blob_value"]],
        Value::Binary(vec![0xde, 0xad, 0xbe, 0xef])
    );
}

fn compatibility_schema() -> &'static str {
    "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
     GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
       ON *.* TO 'pintail'@'%';\
     CREATE TABLE compat_events (\
       id BIGINT UNSIGNED PRIMARY KEY,\
       value VARCHAR(64),\
       decimal_value DECIMAL(38,10),\
       date_value DATE,\
       datetime_value DATETIME(6),\
       timestamp_value TIMESTAMP(6) NULL,\
       latin_value VARCHAR(32) CHARACTER SET latin1,\
       enum_value ENUM('alpha','βeta'),\
       set_value SET('red','green','blue'),\
       bit_value BIT(9),\
       binary_value VARBINARY(8),\
       blob_value BLOB,\
       utiny_value TINYINT UNSIGNED,\
       usmall_value SMALLINT UNSIGNED,\
       umedium_value MEDIUMINT UNSIGNED,\
       uint_value INT UNSIGNED,\
       ubig_value BIGINT UNSIGNED\
     ) DEFAULT CHARACTER SET utf8mb4;\
     INSERT INTO compat_events VALUES \
       (1,'before',0.0000000000,'1000-01-01','2024-02-29 12:34:56.123456',\
        '1970-01-01 00:00:01.000001','plain','alpha','green',b'0',X'',X'',\
        200,65535,16777215,3000000000,18446744073709551615),\
       (2,'delete',1.0000000000,'2000-01-01','2000-01-01 00:00:00.000000',\
        '2000-01-01 00:00:00.000000','plain','alpha','green',b'1',0x01,0x02,\
        0,0,0,0,0);\
     CREATE TABLE myisam_events (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64)) \
       ENGINE=MyISAM DEFAULT CHARACTER SET utf8mb4;\
     INSERT INTO myisam_events VALUES (1,'before');\
     CREATE TABLE compat_generated (\
       id BIGINT UNSIGNED PRIMARY KEY,\
       value VARCHAR(64),\
       value_upper VARCHAR(64) GENERATED ALWAYS AS (UPPER(value)) VIRTUAL\
     ) DEFAULT CHARACTER SET utf8mb4;\
     INSERT INTO compat_generated (id, value) VALUES (1,'before'),(2,'delete');"
}

fn compatibility_mutations() -> &'static str {
    "START TRANSACTION;\
       UPDATE compat_events SET value='updated' WHERE id=1;\
       DELETE FROM compat_events WHERE id=2;\
       INSERT INTO compat_events VALUES (\
         3,'inserted',1234567890123456789012345678.1234567890,\
         '0000-00-00','0000-00-00 00:00:00.000000','0000-00-00 00:00:00.000000',\
         _latin1 0x636166E9,'βeta','red,blue',b'101010101',0x00FF10,0xDEADBEEF,\
         255,65534,16777214,4294967295,9223372036854775808\
       );\
     COMMIT;\
     INSERT INTO myisam_events VALUES (2,'temporary');\
     UPDATE myisam_events SET value='updated' WHERE id=1;\
     DELETE FROM myisam_events WHERE id=2;\
     UPDATE compat_generated SET value='updated' WHERE id=1;\
     DELETE FROM compat_generated WHERE id=2;\
     INSERT INTO compat_generated (id, value) VALUES (3,'inserted');"
}

fn source_schema() -> &'static str {
    "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
     GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
       ON *.* TO 'pintail'@'%';\
     CREATE TABLE primary_rows (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
     INSERT INTO primary_rows VALUES (1,'one'),(2,'two'),(3,'three');\
     CREATE TABLE append_rows (value VARCHAR(64));\
     INSERT INTO append_rows VALUES ('before-a'),('before-b');\
     SET SESSION sql_generate_invisible_primary_key=ON;\
     CREATE TABLE gipk_rows (value VARCHAR(64));\
     SET SESSION sql_generate_invisible_primary_key=OFF;\
     INSERT INTO gipk_rows (value) VALUES ('first');\
     CREATE TABLE decode_fail (\
       id BIGINT UNSIGNED PRIMARY KEY,\
       value VARCHAR(32) CHARACTER SET utf16\
     );\
     CREATE TABLE type_rows (\
       id BIGINT UNSIGNED PRIMARY KEY,\
       decimal_value DECIMAL(38,10),\
       bit_value BIT(9),\
       date_value DATE,\
       datetime_value DATETIME(6),\
       timestamp_value TIMESTAMP(6) NULL,\
       latin_value VARCHAR(32) CHARACTER SET latin1,\
       enum_value ENUM('alpha','βeta'),\
       set_value SET('red','green','blue'),\
       json_value JSON,\
       binary_value VARBINARY(8),\
       blob_value BLOB,\
       bit1 BIT(1),\
       bit8 BIT(8),\
       bit10 BIT(10),\
       bit64 BIT(64)\
     );\
     INSERT INTO type_rows VALUES \
       (1,0.0000000000,b'0','1000-01-01','2024-02-29 12:34:56.123456',\
        '1970-01-01 00:00:01.000001','plain','alpha','green',JSON_OBJECT(),X'',X'',\
        b'1',NULL,b'1010101010',X'FFFFFFFFFFFFFFFF');"
}

fn cdc_mutations() -> &'static str {
    "START TRANSACTION;\
       UPDATE primary_rows SET value='ONE' WHERE id=1;\
       DELETE FROM primary_rows WHERE id=2;\
       INSERT INTO primary_rows VALUES (4,'emoji-🪿');\
     COMMIT;\
     INSERT INTO append_rows VALUES ('after');\
     UPDATE gipk_rows SET value='updated' WHERE value='first';\
     INSERT INTO gipk_rows (value) VALUES ('second');\
     INSERT INTO decode_fail VALUES (1,CONVERT('hello' USING utf16));\
     INSERT INTO type_rows VALUES (\
       2,1234567890123456789012345678.1234567890,b'101010101',\
       '0000-00-00','0000-00-00 00:00:00.000000','0000-00-00 00:00:00.000000',\
       _latin1 0x636166E9,'βeta','red,blue',\
       JSON_OBJECT('b',JSON_ARRAY(TRUE,NULL),'a',1),0x00FF10,0xDEADBEEF,\
       b'0',X'80',NULL,X'8000000000000001'\
     );"
}

fn spawn_sustained_writes(
    mysql: &MysqlContainer,
    rows: u64,
    gate_after: u64,
    gate_name: &str,
) -> Child {
    assert!(
        gate_after > 0 && gate_after < rows,
        "writer gate must split the source writes"
    );
    let sql = (0..rows).fold(String::new(), |mut sql, id| {
        write!(
            sql,
            "INSERT INTO restart_events VALUES ({id},'event-{id}');\
                 DO SLEEP(0.02);"
        )
        .expect("build sustained source writes");
        if id + 1 == gate_after {
            write!(
                sql,
                "SELECT GET_LOCK('{gate_name}', 30);\
                 DO RELEASE_LOCK('{gate_name}');"
            )
            .expect("build sustained-writer gate");
        }
        sql
    });
    Command::new("docker")
        .args([
            "exec",
            &mysql.name,
            &mysql.client,
            "--user=root",
            "--password=pintail-root",
            "--database=app",
            "--execute",
            &sql,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sustained source writer")
}

fn kill_cdc_worker(dsn: &str, metadata_path: &Path, workspace: &Path, database_id: &str) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind CDC acknowledgement");
    let address = listener.local_addr().expect("CDC acknowledgement address");
    listener
        .set_nonblocking(true)
        .expect("nonblocking CDC acknowledgement");
    let executable = std::env::current_exe().expect("CDC test executable");
    let mut child = Command::new(executable)
        .args(["--exact", "cdc_crash_worker", "--ignored"])
        .env("PINTAIL_CDC_CRASH_DSN", dsn)
        .env("PINTAIL_CDC_CRASH_META", metadata_path)
        .env("PINTAIL_CDC_CRASH_WORKSPACE", workspace)
        .env("PINTAIL_CDC_CRASH_DATABASE", database_id)
        .env("PINTAIL_CDC_CRASH_ACK", address.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn CDC crash worker");
    let mut acknowledgement = None;
    for _ in 0..3_000 {
        if let Some(status) = child.try_wait().expect("poll CDC crash worker") {
            panic!("CDC crash worker exited before SIGKILL: {status}");
        }
        match listener.accept() {
            Ok((stream, _)) => {
                acknowledgement = Some(stream);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept CDC acknowledgement: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut acknowledgement = acknowledgement.unwrap_or_else(|| {
        let _kill = child.kill();
        panic!("CDC crash worker did not connect its acknowledgement socket")
    });
    acknowledgement
        .set_nonblocking(false)
        .expect("blocking CDC acknowledgement");
    acknowledgement
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("CDC acknowledgement timeout");
    acknowledgement
        .read_exact(&mut [0_u8; 10])
        .expect("ten durable CDC checkpoints");
    child.kill().expect("SIGKILL CDC worker");
    let status = child.wait().expect("reap CDC crash worker");
    assert!(!status.success(), "SIGKILLed CDC worker must fail");
}

fn checked_output(command: &mut Command, action: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("{action}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format_output_error(action, &output))
    }
}

fn format_output_error(action: &str, output: &Output) -> String {
    format!(
        "{action} failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn docker_host() -> Result<String, String> {
    let context = checked_output(
        Command::new("docker").args(["context", "show"]),
        "read Docker context",
    )?;
    let context = String::from_utf8(context.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    let endpoint = checked_output(
        Command::new("docker").args([
            "context",
            "inspect",
            &context,
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]),
        "read Docker endpoint",
    )?;
    let endpoint = String::from_utf8(endpoint.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    if let Some(target) = endpoint.strip_prefix("ssh://") {
        let target = target.split('@').next_back().unwrap_or(target);
        // A bracketed IPv6 literal is the host itself; splitting it at the
        // first colon would hand SSH a fragment.
        if let Some(literal) = target.strip_prefix('[')
            && let Some((address, _)) = literal.split_once(']')
        {
            return Ok(address.to_owned());
        }
        let target = target.split(':').next().unwrap_or(target);
        let config = checked_output(
            Command::new("ssh").args(["-G", target]),
            "resolve Docker SSH host",
        )?;
        return String::from_utf8(config.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .find_map(|line| line.strip_prefix("hostname ").map(str::to_owned))
            .ok_or_else(|| "SSH configuration did not expose a hostname".to_owned());
    }
    if let Some(endpoint) = endpoint
        .strip_prefix("tcp://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .or_else(|| endpoint.strip_prefix("https://"))
    {
        return Ok(endpoint
            .trim_start_matches('[')
            .split([']', ':'])
            .next()
            .unwrap_or(endpoint)
            .to_owned());
    }
    Ok("127.0.0.1".to_owned())
}

/// A host as a DSN authority: an IPv6 literal bracketed, anything else as is.
fn dsn_host(host: &str) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.contains(':') {
        format!("[{bare}]")
    } else {
        bare.to_owned()
    }
}

/// A probe lists the source's tables and then reads each one's shape, one
/// statement at a time, while the source keeps executing. A table dropped
/// after it was listed is left out of the report; it must not fail the
/// probe, because a change-capture cycle reprobes at every schema change
/// and a failed reprobe fails the cycle for every table of the database.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_probe_leaves_out_tables_dropped_while_it_runs() {
    const CHURNED: usize = 12;
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE anchor (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));\
             CREATE TABLE zenith (id BIGINT UNSIGNED PRIMARY KEY, value VARCHAR(64));",
        )
        .expect("churn source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("churn DSN"));
    let owner = Pool::new(
        Opts::from_url(
            &mysql
                .dsn()
                .replacen("pintail:pintail@", "root:pintail-root@", 1),
        )
        .expect("churn owner DSN"),
    );

    // Tables that sort between the two that stay, created and dropped in a
    // loop for as long as the probes run.
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        let owner = owner.clone();
        tokio::spawn(async move {
            let mut connection = owner.get_conn().await.expect("churn connection");
            let mut rounds = 0_u64;
            while !stop.load(Ordering::Acquire) {
                for index in 0..CHURNED {
                    connection
                        .query_drop(format!(
                            "CREATE TABLE churn_{index:02} (id BIGINT UNSIGNED PRIMARY KEY, \
                               label VARCHAR(16), KEY by_label (label))"
                        ))
                        .await
                        .expect("create churned table");
                }
                for index in 0..CHURNED {
                    connection
                        .query_drop(format!("DROP TABLE churn_{index:02}"))
                        .await
                        .expect("drop churned table");
                }
                rounds += 1;
            }
            rounds
        })
    };

    let mut partial_listings = 0_usize;
    for attempt in 0..150 {
        let report = probe(&pool, "app")
            .await
            .unwrap_or_else(|error| panic!("probe {attempt} failed under table churn: {error}"));
        let names = report
            .tables
            .iter()
            .map(|table| table.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"anchor") && names.contains(&"zenith"),
            "probe {attempt} lost a table nobody dropped: {names:?}"
        );
        for table in &report.tables {
            assert!(
                !table.columns.is_empty(),
                "probe {attempt} reported {} without columns",
                table.name
            );
        }
        let churned = names.len() - 2;
        if churned > 0 && churned < CHURNED {
            partial_listings += 1;
        }
    }
    stop.store(true, Ordering::Release);
    let rounds = churn.await.expect("churn task");
    assert!(rounds > 0, "the churn never completed a round");
    // The probes really did run across creations and drops, not beside them.
    assert!(
        partial_listings > 0,
        "no probe saw the source mid-churn in {rounds} rounds"
    );
    owner.disconnect().await.expect("disconnect churn owner");
    pool.disconnect().await.expect("disconnect churn pool");
}

/// A failed cycle writes its error onto every table of the database. The
/// next cycle lifts it as soon as it has applied everything the source
/// held, while it is still following the stream, not when its window ends.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn a_followed_catch_up_lifts_an_earlier_failure_once_it_has_caught_up() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE ledger (id BIGINT UNSIGNED PRIMARY KEY, note VARCHAR(32) NOT NULL);\
             CREATE TABLE tally (id BIGINT UNSIGNED PRIMARY KEY, total BIGINT NOT NULL);\
             INSERT INTO ledger VALUES (1,'seed');\
             INSERT INTO tally VALUES (1,10);",
        )
        .expect("lift source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("lift DSN"));
    let report = probe(&pool, "app").await.expect("probe lift source");
    let workspace = tempfile::tempdir().expect("lift workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("lift metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register lift source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("lift schema"),
                StoreOptions::default(),
            )
            .expect("lift store");
            SnapshotTarget::new(source.clone(), store).expect("lift snapshot target")
        })
        .collect::<Vec<_>>();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("lift baseline snapshot");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("lift CDC target")
        })
        .collect::<Vec<_>>();

    // What a failed cycle leaves behind, and a write for the next to apply.
    let states = |metadata_path: &Path| {
        let metadata = MetaStore::open(metadata_path).expect("inspect lift states");
        let database = metadata
            .database(DATABASE_ID)
            .expect("database row")
            .expect("database exists")
            .state;
        let tables = metadata
            .tables(DATABASE_ID)
            .expect("table rows")
            .into_iter()
            .map(|table| (table.name, table.state, table.last_error))
            .collect::<Vec<_>>();
        (database, tables)
    };
    {
        let metadata = MetaStore::open(&metadata_path).expect("lift metadata");
        metadata
            .set_database_replication_state(DATABASE_ID, "cdc", "2026-07-30T00:00:01Z")
            .expect("streaming state");
        metadata
            .fail_database_job(
                DATABASE_ID,
                "the earlier cycle failed",
                "2026-07-30T00:00:02Z",
            )
            .expect("record the earlier failure");
    }
    let (database, tables) = states(&metadata_path);
    assert_eq!(database, "error");
    assert_eq!(tables.len(), 2);
    assert!(tables.iter().all(|(_, state, _)| state == "error"));
    mysql
        .query_batch("INSERT INTO ledger VALUES (2,'after');")
        .expect("lift write");

    let stop = pintail_cdc::CycleStop::new();
    let cycle = run_cdc(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        CdcOptions {
            blocking: false,
            follow: Some(Duration::from_secs(600)),
            stop: Some(stop.clone()),
            ..CdcOptions::default()
        },
    );
    // Watches the recorded state while the cycle is on its stream, and ends
    // the cycle only after it has seen the answer.
    let watch = async {
        let waited = std::time::Instant::now();
        let seen = loop {
            let (database, tables) = states(&metadata_path);
            // No write touches `tally`, so only the lift clears it.
            let lifted = tables.iter().all(|(_, state, _)| state == "streaming");
            if lifted || waited.elapsed() > Duration::from_secs(30) {
                break (database, tables);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        stop.request();
        seen
    };
    let (outcome, (database, tables)) = tokio::join!(cycle, watch);
    let outcome = outcome.expect("followed lift catch-up");
    assert_eq!(
        database, "streaming",
        "the cycle was still following and had not lifted the failure: {tables:?}"
    );
    for (name, state, last_error) in &tables {
        assert_eq!(state, "streaming", "{name}");
        assert_eq!(last_error, &None, "{name}");
    }
    assert_eq!(
        cdc_target(&outcome.targets, "ledger")
            .store()
            .snapshot()
            .scan()
            .expect("lift scan")
            .len(),
        2
    );
    pool.disconnect().await.expect("disconnect lift pool");
}

/// A source for the followed-stream tests: a file-position source, whose
/// transactions open with an anonymous GTID event, holding one table with a
/// text column wide enough for a row of tens of megabytes, copied and ready
/// for change capture.
struct FollowedSource {
    mysql: MysqlContainer,
    report: ProbeReport,
    workspace: tempfile::TempDir,
    metadata_path: std::path::PathBuf,
    targets: Vec<CdcTarget>,
}

async fn followed_source() -> FollowedSource {
    let mysql = MysqlContainer::start_file_position().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
               ON *.* TO 'pintail'@'%';\
             CREATE TABLE scrolls (id BIGINT UNSIGNED PRIMARY KEY, body LONGTEXT NOT NULL);\
             INSERT INTO scrolls VALUES (1,'seed');",
        )
        .expect("followed source schema");
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("followed DSN"));
    let report = probe(&pool, "app").await.expect("probe followed source");
    let workspace = tempfile::tempdir().expect("followed workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("followed metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-07-30T00:00:00Z",
        )
        .expect("register followed source");
    let snapshot_targets = report
        .tables
        .iter()
        .map(|source| {
            let store = TableStore::open(
                workspace.path().join(&source.name),
                source.table_schema().expect("followed schema"),
                StoreOptions::default(),
            )
            .expect("followed store");
            SnapshotTarget::new(source.clone(), store).expect("followed snapshot target")
        })
        .collect::<Vec<_>>();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions::default(),
    )
    .await
    .expect("followed baseline snapshot");
    pool.disconnect()
        .await
        .expect("disconnect followed setup pool");
    let targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            CdcTarget::new(source, target.into_store()).expect("followed CDC target")
        })
        .collect::<Vec<_>>();
    FollowedSource {
        mysql,
        report,
        workspace,
        metadata_path,
        targets,
    }
}

/// The rows of `scrolls`, as id and body length.
fn scroll_lengths(targets: &[CdcTarget]) -> Vec<(u64, usize)> {
    let mut rows = cdc_target(targets, "scrolls")
        .store()
        .snapshot()
        .scan()
        .expect("scan scrolls")
        .into_iter()
        .map(|row| {
            let values = row.values();
            let id = match &values[0] {
                Value::UInt64(id) => *id,
                other => panic!("unexpected id {other:?}"),
            };
            let length = match &values[1] {
                Value::Utf8(body) => body.len(),
                other => panic!("unexpected body {other:?}"),
            };
            (id, length)
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows
}

/// A relay between change capture and its source that can stop handing the
/// source's bytes on, as a source that has stopped sending looks from the
/// replica's side: the connection stays open and nothing arrives.
struct Relay {
    port: u16,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    /// Connections accepted before this count hand nothing more on.
    hold_below: Arc<std::sync::atomic::AtomicUsize>,
    /// Every connection hands on at most this many of the source's bytes.
    hold_after_bytes: Arc<std::sync::atomic::AtomicUsize>,
}

impl Relay {
    fn start(mysql: &MysqlContainer) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind relay");
        let port = listener.local_addr().expect("relay address").port();
        let upstream = (
            mysql
                .host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            mysql.port,
        );
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hold_below = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hold_after_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        {
            let accepted = Arc::clone(&accepted);
            let hold_below = Arc::clone(&hold_below);
            let hold_after_bytes = Arc::clone(&hold_after_bytes);
            thread::spawn(move || {
                for client in listener.incoming() {
                    let Ok(client) = client else { return };
                    let Ok(server) = TcpStream::connect((upstream.0.as_str(), upstream.1)) else {
                        return;
                    };
                    let index = accepted.fetch_add(1, Ordering::SeqCst);
                    let (Ok(client_reader), Ok(server_writer)) =
                        (client.try_clone(), server.try_clone())
                    else {
                        return;
                    };
                    thread::spawn(move || {
                        let (mut from, mut to) = (client_reader, server_writer);
                        let _copied = std::io::copy(&mut from, &mut to);
                        let _closed = to.shutdown(std::net::Shutdown::Write);
                    });
                    let hold_below = Arc::clone(&hold_below);
                    let hold_after_bytes = Arc::clone(&hold_after_bytes);
                    thread::spawn(move || {
                        let (mut from, mut to) = (server, client);
                        let mut buffer = vec![0_u8; 64 * 1024];
                        let mut forwarded = 0_usize;
                        loop {
                            let allowed = hold_after_bytes
                                .load(Ordering::SeqCst)
                                .saturating_sub(forwarded);
                            if index < hold_below.load(Ordering::SeqCst) || allowed == 0 {
                                thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            let limit = buffer.len().min(allowed);
                            match from.read(&mut buffer[..limit]) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    // A hold raised while this read was
                                    // waiting keeps what it brought too.
                                    while index < hold_below.load(Ordering::SeqCst) {
                                        thread::sleep(Duration::from_millis(10));
                                    }
                                    if to.write_all(&buffer[..read]).is_err() {
                                        break;
                                    }
                                    forwarded += read;
                                }
                            }
                        }
                        let _closed = to.shutdown(std::net::Shutdown::Write);
                    });
                }
            });
        }
        Self {
            port,
            accepted,
            hold_below,
            hold_after_bytes,
        }
    }

    fn dsn(&self) -> String {
        format!("mysql://pintail:pintail@127.0.0.1:{}/app", self.port)
    }

    /// Holds every connection accepted so far; later ones pass.
    fn hold_existing(&self) {
        self.hold_below
            .store(self.accepted.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Holds every connection, existing and new.
    fn hold_all(&self) {
        self.hold_below.store(usize::MAX, Ordering::SeqCst);
    }
}

/// A followed cycle whose window closes while a large event is still on its
/// way must stay for it. Ending on the clock left the event unread, and the
/// next cycle - starting from the same checkpoint with the same window - did
/// the same, so every cycle succeeded and the mirror never moved again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_followed_catch_up_waits_for_an_event_still_arriving_when_its_window_closes() {
    let source = followed_source().await;
    let wide = 48 * 1024 * 1024;
    source
        .mysql
        .query_batch(&format!(
            "INSERT INTO scrolls VALUES (2, REPEAT('a', {wide}));\
             INSERT INTO scrolls VALUES (3, 'after');"
        ))
        .expect("wide row");
    let pool = Pool::new(Opts::from_url(&source.mysql.dsn()).expect("followed DSN"));
    let result = run_cdc(
        &pool,
        &source.metadata_path,
        DATABASE_ID,
        &source.report,
        source.targets,
        CdcOptions {
            blocking: false,
            // Closed before the stream has handed anything over.
            follow: Some(Duration::from_millis(1)),
            ..CdcOptions::default()
        },
    )
    .await
    .expect("followed catch-up");
    assert_eq!(result.mutations, 2, "both writes are applied by one cycle");
    assert_eq!(
        scroll_lengths(&result.targets),
        vec![(1, 4), (2, wide), (3, 5)]
    );
    pool.disconnect().await.expect("disconnect followed pool");
    drop(source.workspace);
}

/// A source that stops sending in the middle of a transaction ends the
/// cycle with the reason after the stall bound, and the next cycle applies
/// the transaction from the checkpoint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_stream_that_stops_inside_a_transaction_ends_the_cycle_with_the_reason() {
    let source = followed_source().await;
    let wide = 24 * 1024 * 1024;
    source
        .mysql
        .query_batch(&format!(
            "INSERT INTO scrolls VALUES (2, REPEAT('b', {wide}));"
        ))
        .expect("wide row");
    let relay = Relay::start(&source.mysql);
    // The wide row's event is the only thing in the stream larger than a
    // few kilobytes: the stream stops partway through it.
    relay
        .hold_after_bytes
        .store(4 * 1024 * 1024, Ordering::SeqCst);
    let pool = Pool::new(Opts::from_url(&relay.dsn()).expect("relay DSN"));
    let stalled_before = pintail_cdc::streams_stalled();
    let options = || CdcOptions {
        blocking: false,
        follow: Some(Duration::from_millis(200)),
        stream_stall: Duration::from_secs(2),
        ..CdcOptions::default()
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        run_cdc(
            &pool,
            &source.metadata_path,
            DATABASE_ID,
            &source.report,
            source.targets,
            options(),
        ),
    )
    .await
    .expect("the stalled cycle ended");
    let Err(error) = outcome else {
        panic!("a stalled stream fails the cycle");
    };
    assert!(
        error.to_string().contains("inside a transaction"),
        "unexpected error: {error}"
    );
    assert_eq!(pintail_cdc::streams_stalled(), stalled_before + 1);
    drop(pool);

    // The next cycle reopens the tables and starts from the checkpoint.
    relay.hold_after_bytes.store(usize::MAX, Ordering::SeqCst);
    let targets = source
        .report
        .tables
        .iter()
        .map(|table| {
            CdcTarget::open_tracked(
                &source.metadata_path,
                DATABASE_ID,
                table.clone(),
                source.workspace.path().join(&table.name),
                StoreOptions::default(),
            )
            .expect("reopen target")
        })
        .collect::<Vec<_>>();
    let pool = Pool::new(Opts::from_url(&relay.dsn()).expect("relay DSN"));
    let result = run_cdc(
        &pool,
        &source.metadata_path,
        DATABASE_ID,
        &source.report,
        targets,
        options(),
    )
    .await
    .expect("the next cycle");
    assert_eq!(result.mutations, 1);
    assert_eq!(scroll_lengths(&result.targets), vec![(1, 4), (2, wide)]);
    pool.disconnect().await.expect("disconnect relay pool");
}

/// A followed cycle reads the end of the source's log when its window
/// closes. A source that does not answer must not hold the cycle: past the
/// stall bound it ends as it would have without the answer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_source_that_does_not_report_its_log_end_does_not_hold_a_followed_cycle() {
    let source = followed_source().await;
    let relay = Relay::start(&source.mysql);
    let pool = Pool::new(Opts::from_url(&relay.dsn()).expect("relay DSN"));
    let unanswered_before = pintail_cdc::log_end_unanswered();
    let cycle = run_cdc(
        &pool,
        &source.metadata_path,
        DATABASE_ID,
        &source.report,
        source.targets,
        CdcOptions {
            blocking: false,
            follow: Some(Duration::from_secs(2)),
            stream_stall: Duration::from_secs(2),
            ..CdcOptions::default()
        },
    );
    let hold = async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        relay.hold_all();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(cycle, hold) })
            .await
            .expect("the followed cycle ended");
    let result = result.expect("followed catch-up");
    assert_eq!(result.mutations, 0);
    assert_eq!(pintail_cdc::log_end_unanswered(), unanswered_before + 1);
}

/// A followed cycle whose window closed behind the source's log reads on,
/// but not forever: a stream that then hands nothing over for the stall
/// bound ends the cycle between transactions, and the next one applies
/// what it missed from the checkpoint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_followed_cycle_reading_on_past_its_window_gives_up_on_a_silent_stream() {
    let source = followed_source().await;
    let relay = Relay::start(&source.mysql);
    let pool = Pool::new(Opts::from_url(&relay.dsn()).expect("relay DSN"));
    let extended_before = pintail_cdc::follows_extended();
    let stalled_before = pintail_cdc::streams_stalled();
    let options = || CdcOptions {
        blocking: false,
        follow: Some(Duration::from_secs(2)),
        stream_stall: Duration::from_secs(2),
        ..CdcOptions::default()
    };
    let cycle = run_cdc(
        &pool,
        &source.metadata_path,
        DATABASE_ID,
        &source.report,
        source.targets,
        options(),
    );
    // Once the stream is open and quiet, it stops handing anything over,
    // and the source is written: the end of its log moves past the stream.
    let hold = async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        relay.hold_existing();
        source
            .mysql
            .query_batch("INSERT INTO scrolls VALUES (2,'missed');")
            .expect("write behind the held stream");
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(cycle, hold) })
            .await
            .expect("the followed cycle ended");
    let result = result.expect("followed catch-up");
    assert_eq!(result.mutations, 0);
    assert_eq!(pintail_cdc::follows_extended(), extended_before + 1);
    assert_eq!(pintail_cdc::streams_stalled(), stalled_before + 1);
    drop(pool);

    let pool = Pool::new(Opts::from_url(&source.mysql.dsn()).expect("followed DSN"));
    let result = run_cdc(
        &pool,
        &source.metadata_path,
        DATABASE_ID,
        &source.report,
        result.targets,
        CdcOptions {
            follow: Some(Duration::from_millis(200)),
            ..options()
        },
    )
    .await
    .expect("the next cycle");
    assert_eq!(result.mutations, 1);
    assert_eq!(scroll_lengths(&result.targets), vec![(1, 4), (2, 6)]);
    pool.disconnect().await.expect("disconnect followed pool");
}
