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
use pintail_meta::MetaStore;
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
    assert!(
        cdc_target(&targets, "truncate_me")
            .store()
            .snapshot()
            .scan()
            .unwrap()
            .is_empty()
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
    assert!(
        MetaStore::open(&metadata_path)
            .expect("compatibility metadata state")
            .tables_needing_resync(DATABASE_ID)
            .expect("compatibility table state")
            .is_empty(),
        "{}",
        variant.label
    );
    assert!(dlq_errors(&metadata_path).is_empty(), "{}", variant.label);
    assert_eq!(result.checkpoint.kind, "filepos", "{}", variant.label);
    assert_eq!(result.mutations, 9, "{}", variant.label);
    assert_compatibility_rows(variant.label, &result.targets);
    let logs_virtual = !variant.label.starts_with("mariadb");
    assert_virtual_column_rows(variant.label, &report, &result.targets, logs_virtual);
    assert_virtual_column_added_mid_stream_is_recopied(
        variant.label,
        &mysql,
        &pool,
        &metadata_path,
        workspace.path(),
        result.targets,
        logs_virtual,
    )
    .await;
    pool.disconnect()
        .await
        .expect("disconnect compatibility pool");
}

/// `MySQL` writes a `VIRTUAL` generated column into its row images, so the
/// column replicates like any other; `MariaDB` leaves it out of the binary
/// log, so the probe skips it and the schema is the physical one.
fn assert_virtual_column_rows(
    label: &str,
    report: &ProbeReport,
    targets: &[CdcTarget],
    logs_virtual: bool,
) {
    let probed = report
        .tables
        .iter()
        .find(|table| table.name == "compat_generated")
        .expect("probed generated table");
    let target = cdc_target(targets, "compat_generated");
    let rows = target.store().snapshot().scan().expect("generated scan");
    assert_eq!(rows.len(), 2, "{label}");
    if logs_virtual {
        assert_eq!(probed.columns.len(), 3, "{label} keeps the virtual column");
        assert!(probed.warnings.is_empty(), "{label}: {:?}", probed.warnings);
        // The snapshot row (id 1, then updated) and the streamed insert
        // both carry the computed value at the column's own ordinal.
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
        for row in &rows {
            assert_eq!(row.values().len(), 2, "{label}");
        }
    }
}

/// A `VIRTUAL` column added to a live table cannot evolve in place - the
/// rows already copied would read NULL where the source computes values -
/// so the stream quarantines the table, and the recopy that follows carries
/// the values for every row. Where the server does not log the column, the
/// ALTER changes nothing the stream can see and the table stays live.
async fn assert_virtual_column_added_mid_stream_is_recopied(
    label: &str,
    mysql: &MysqlContainer,
    pool: &Pool,
    metadata_path: &Path,
    workspace: &Path,
    targets: Vec<CdcTarget>,
    logs_virtual: bool,
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
        .tables_needing_resync(DATABASE_ID)
        .expect("flagged tables");
    if !logs_virtual {
        assert!(flagged.is_empty(), "{label}: {flagged:?}");
        let rows = cdc_target(&result.targets, "compat_generated")
            .store()
            .snapshot()
            .scan()
            .expect("generated scan");
        assert_eq!(rows.len(), 3, "{label}: the insert streamed through");
        return;
    }
    assert_eq!(
        flagged.into_iter().collect::<Vec<_>>(),
        vec!["compat_generated".to_owned()],
        "{label}: the table is recopied, not evolved in place"
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
    let first = finite_catch_up(pool, metadata_path, report, targets)
        .await
        .expect("first restart catch-up");
    assert_eq!(first.mutations, 4);
    let checkpoint_after = first.checkpoint.clone();
    assert_restart_rows(&first.targets);
    drop(first.targets);

    MetaStore::open(metadata_path)
        .expect("rewind metadata")
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
    assert_eq!(replay.mutations, 4);
    assert_eq!(replay.checkpoint, checkpoint_after);
    assert_restart_rows(&replay.targets);
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

    let types = targets["type_rows"];
    let rows = types.store().snapshot().scan().expect("type fidelity scan");
    assert_eq!(rows.len(), 2);
    let columns = types
        .source()
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name.as_str(), index))
        .collect::<BTreeMap<_, _>>();
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
        Value::Utf8("{\"a\":1,\"b\":[true,null]}".to_owned())
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
       blob_value BLOB\
     );\
     INSERT INTO type_rows VALUES \
       (1,0.0000000000,b'0','1000-01-01','2024-02-29 12:34:56.123456',\
        '1970-01-01 00:00:01.000001','plain','alpha','green',JSON_OBJECT(),X'',X'');"
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
       JSON_OBJECT('b',JSON_ARRAY(TRUE,NULL),'a',1),0x00FF10,0xDEADBEEF\
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
