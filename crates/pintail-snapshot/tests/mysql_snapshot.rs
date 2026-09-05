use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mysql_async::{Opts, Pool, prelude::Queryable};
use pintail_meta::MetaStore;
use pintail_probe::{SourceFlavor, probe};
use pintail_snapshot::{
    SnapshotOptions, SnapshotPosition, SnapshotTarget, run_snapshot, run_snapshot_with_progress,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, Value};

const DATABASE_ID: &str = "m3-source";
const FACT_ROWS: u64 = 100_000;
const RESUME_ROWS: u64 = 100_000;

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
                "--server-id=84",
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
        let name = format!("pintail-m3-{label}-{}-{nonce}", std::process::id());
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
        checked_output(&mut command, &format!("start {label} snapshot source"))?;
        let host = docker_host()?;
        let port_output = checked_output(
            Command::new("docker").args(["port", &name, "3306/tcp"]),
            "inspect MySQL published port",
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
        // and then restart; require consecutive successes so a probe cannot
        // land in that window.
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
        Err(format!("{label} did not become ready within 120 seconds"))
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
async fn m3_snapshot_basic_resume_type_fidelity_and_pk_matrix() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(&source_schema())
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("snapshot DSN"));
    let report = probe(&pool, "app").await.expect("probe source");
    assert!(report.capabilities.log_bin);
    assert!(report.capabilities.row_binlog);
    assert!(report.capabilities.full_row_image);
    assert!(report.capabilities.full_row_metadata);
    assert!(report.capabilities.replication_grants);
    assert!(report.capabilities.global_read_lock);
    assert_key_modes(&report);
    assert_type_mapping(&report);

    let workspace = tempfile::tempdir().expect("snapshot workspace");
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

    let resume_directory = workspace.path().join("resume_rows");
    let resume_source = report
        .tables
        .iter()
        .find(|table| table.name == "resume_rows")
        .expect("resume table")
        .clone();
    kill_snapshot_worker(&mysql.dsn(), &metadata_path, &resume_directory, DATABASE_ID);
    let resumed_target = target(&resume_source, &resume_directory);
    let resumed = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![resumed_target],
        SnapshotOptions {
            workers: 1,
            chunk_rows: 1_000,
            ..SnapshotOptions::default()
        },
    )
    .await
    .expect("resume snapshot");
    assert!(resumed.globally_consistent);
    assert_eq!(resumed.tables[0].chunks, 100);
    assert_eq!(resumed.tables[0].rows, RESUME_ROWS);
    assert_eq!(
        resumed.targets[0]
            .store()
            .snapshot()
            .scan()
            .expect("resumed rows")
            .len() as u64,
        RESUME_ROWS
    );
    drop(resumed);

    let targets = report
        .tables
        .iter()
        .filter(|table| table.name != "resume_rows" && table.name != "digits")
        .map(|source| {
            let directory = workspace.path().join(format!("table-{}", source.name));
            target(source, &directory)
        })
        .collect();
    let result = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        SnapshotOptions {
            workers: 4,
            chunk_rows: 25_000,
            ..SnapshotOptions::default()
        },
    )
    .await
    .expect("complete M3 snapshot");
    assert!(result.globally_consistent);
    assert!(result.consistency_warning.is_none());
    for target in &result.targets {
        if target.source().name.starts_with("fact_") {
            assert_fact_checksum(&pool, target).await;
        }
    }
    assert_type_values(&result.targets);
    assert_pk_counts(&result.targets);
    pool.disconnect().await.expect("disconnect source pool");
}

/// Global `Com_select` on the source: how many SELECTs every session has
/// issued since the server started.
async fn source_selects(pool: &Pool) -> u64 {
    let mut connection = pool.get_conn().await.expect("status connection");
    let row: Option<(String, String)> = connection
        .query_first("SHOW GLOBAL STATUS LIKE 'Com_select'")
        .await
        .expect("Com_select");
    row.expect("Com_select row")
        .1
        .parse()
        .expect("Com_select value")
}

/// A resumed table advances past its journaled chunks by their upper keys:
/// the source answers one empty tail page per table, not every chunk again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_resumed_snapshot_does_not_reread_the_chunks_it_already_holds() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(&source_schema())
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("snapshot DSN"));
    let report = probe(&pool, "app").await.expect("probe source");
    let workspace = tempfile::tempdir().expect("snapshot workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-09-05T00:00:00Z",
        )
        .expect("register database");
    let source = report
        .tables
        .iter()
        .find(|table| table.name == "resume_rows")
        .expect("resume table")
        .clone();
    let directory = workspace.path().join("resume_rows");
    let options = SnapshotOptions {
        workers: 1,
        chunk_rows: 1_000,
        ..SnapshotOptions::default()
    };
    let first = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![target(&source, &directory)],
        options.clone(),
    )
    .await
    .expect("first copy");
    assert_eq!(first.tables[0].chunks, 100);
    drop(first);

    let before = source_selects(&pool).await;
    let resumed = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![target(&source, &directory)],
        options,
    )
    .await
    .expect("resume over a complete journal");
    let selects = source_selects(&pool).await - before;
    assert_eq!(resumed.tables[0].chunks, 100);
    assert_eq!(resumed.tables[0].rows, RESUME_ROWS);
    // The tail page that proves the table ended, plus the status reads
    // around it; a hundred and more means the chunks were read again.
    assert!(
        selects <= 4,
        "a resume over a complete journal issued {selects} SELECTs against the source"
    );
    pool.disconnect().await.expect("disconnect source pool");
}

/// One table that cannot be copied is flagged for a resync; the others
/// complete and the run reports the failure instead of becoming one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
async fn a_table_that_cannot_be_copied_is_flagged_and_the_rest_complete() {
    use std::os::unix::fs::PermissionsExt as _;

    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(&source_schema())
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("snapshot DSN"));
    let report = probe(&pool, "app").await.expect("probe source");
    let workspace = tempfile::tempdir().expect("snapshot workspace");
    let metadata_path = workspace.path().join("pintail-meta.db");
    MetaStore::open(&metadata_path)
        .expect("metadata")
        .upsert_database(
            DATABASE_ID,
            "app",
            mysql.dsn().as_bytes(),
            "2026-09-05T00:00:00Z",
        )
        .expect("register database");
    let find = |name: &str| {
        report
            .tables
            .iter()
            .find(|table| table.name == name)
            .expect("probed table")
            .clone()
    };
    let healthy = target(
        &find("primary_table"),
        &workspace.path().join("primary_table"),
    );
    let broken_directory = workspace.path().join("composite_table");
    let broken = target(&find("composite_table"), &broken_directory);
    // The store is open; its directory is now read-only, so the first
    // segment the copy writes fails as a storage error of this table alone.
    std::fs::set_permissions(&broken_directory, std::fs::Permissions::from_mode(0o555))
        .expect("read-only table directory");

    let result = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        vec![healthy, broken],
        SnapshotOptions {
            workers: 1,
            chunk_rows: 1_000,
            ..SnapshotOptions::default()
        },
    )
    .await;
    std::fs::set_permissions(&broken_directory, std::fs::Permissions::from_mode(0o755))
        .expect("restore permissions");
    let result = result.expect("the run completes despite one table");
    assert_eq!(
        result
            .failed
            .iter()
            .map(|failure| failure.table.as_str())
            .collect::<Vec<_>>(),
        vec!["composite_table"]
    );
    assert_eq!(
        result
            .tables
            .iter()
            .map(|table| table.table.as_str())
            .collect::<Vec<_>>(),
        vec!["primary_table"]
    );
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    let states = metadata
        .tables(DATABASE_ID)
        .expect("tables")
        .into_iter()
        .map(|table| (table.name, table.state, table.copy_complete))
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            (
                "composite_table".to_owned(),
                "needs_resync".to_owned(),
                false
            ),
            ("primary_table".to_owned(), "pending".to_owned(), true),
        ]
    );
    pool.disconnect().await.expect("disconnect source pool");
}

struct CompatibilityVariant {
    label: &'static str,
    image: &'static str,
    client: &'static str,
    arguments: &'static [&'static str],
    flavor: SourceFlavor,
    log_bin: bool,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MySQL 5.7/8.4 and MariaDB 11 on the configured Docker host"]
#[allow(clippy::too_many_lines)]
async fn snapshot_compatibility_matrix_covers_file_position_mariadb_and_polling_sources() {
    let variants = [
        CompatibilityVariant {
            label: "mysql84-filepos",
            image: "mysql:8.4",
            client: "mysql",
            arguments: &[
                "--server-id=841",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
            flavor: SourceFlavor::Mysql,
            log_bin: true,
        },
        CompatibilityVariant {
            label: "mysql57-filepos",
            image: "mysql:5.7",
            client: "mysql",
            arguments: &[
                "--server-id=57",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
            flavor: SourceFlavor::Mysql,
            log_bin: true,
        },
        CompatibilityVariant {
            label: "mariadb11-gtid",
            image: "mariadb:11",
            client: "mariadb",
            arguments: &[
                "--server-id=110",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--gtid-strict-mode=ON",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
            flavor: SourceFlavor::MariaDb,
            log_bin: true,
        },
        CompatibilityVariant {
            label: "mysql84-polling",
            image: "mysql:8.4",
            client: "mysql",
            arguments: &[
                "--skip-log-bin",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ],
            flavor: SourceFlavor::Mysql,
            log_bin: false,
        },
    ];

    for variant in variants {
        let mysql = MysqlContainer::start_variant(
            variant.label,
            variant.image,
            variant.client,
            variant.arguments,
        )
        .unwrap_or_else(|error| panic!("{}: {error}", variant.label));
        mysql
            .query_batch(compatibility_source_schema())
            .unwrap_or_else(|error| panic!("{}: {error}", variant.label));
        let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("compatibility DSN"));
        let report = probe(&pool, "app")
            .await
            .unwrap_or_else(|error| panic!("{} probe: {error}", variant.label));
        assert_eq!(report.server.flavor, variant.flavor, "{}", variant.label);
        assert_eq!(
            report.capabilities.log_bin, variant.log_bin,
            "{}",
            variant.label
        );

        let workspace = tempfile::tempdir().expect("compatibility workspace");
        let metadata_path = workspace.path().join("pintail-meta.db");
        MetaStore::open(&metadata_path)
            .expect("compatibility metadata")
            .upsert_database(
                variant.label,
                "app",
                mysql.dsn().as_bytes(),
                "2026-07-30T00:00:00Z",
            )
            .expect("register compatibility source");
        let targets = report
            .tables
            .iter()
            .map(|source| target(source, &workspace.path().join(&source.name)))
            .collect();
        let result = run_snapshot(
            &pool,
            &metadata_path,
            variant.label,
            &report,
            targets,
            SnapshotOptions {
                workers: 2,
                chunk_rows: 2,
                ..SnapshotOptions::default()
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{} snapshot: {error}", variant.label));
        assert!(result.globally_consistent, "{}", variant.label);
        if variant.log_bin {
            assert!(
                !matches!(result.position, SnapshotPosition::Unavailable),
                "{}",
                variant.label
            );
        } else {
            assert_eq!(result.position, SnapshotPosition::Unavailable);
        }
        for target in &result.targets {
            let source_count: u64 = pool
                .get_conn()
                .await
                .expect("compatibility connection")
                .query_first(format!("SELECT COUNT(*) FROM `{}`", target.source().name))
                .await
                .expect("compatibility count")
                .expect("compatibility count row");
            assert_eq!(
                target
                    .store()
                    .snapshot()
                    .scan()
                    .expect("compatibility scan")
                    .len() as u64,
                source_count,
                "{} {}",
                variant.label,
                target.source().name
            );
        }
        let fidelity = result
            .targets
            .iter()
            .find(|target| target.source().name == "compat_types")
            .expect("compatibility type table");
        let rows = fidelity
            .store()
            .snapshot()
            .scan()
            .expect("compat type scan");
        let date_index = fidelity
            .source()
            .columns
            .iter()
            .position(|column| column.name == "date_value")
            .expect("date column");
        // The all-zero date is a value every one of these sources returns,
        // not an absence; the replica preserves it.
        assert_eq!(
            rows[0].values()[date_index],
            Value::Utf8("0000-00-00".to_owned()),
            "{}",
            variant.label
        );
        assert_eq!(
            rows[1].values()[date_index],
            Value::Utf8("1000-01-01".to_owned()),
            "{}",
            variant.label
        );
        pool.disconnect()
            .await
            .expect("disconnect compatibility pool");
        drop(mysql);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "helper process for the snapshot-resume crash gate"]
async fn snapshot_crash_worker() {
    let Ok(dsn) = std::env::var("PINTAIL_SNAPSHOT_CRASH_DSN") else {
        return;
    };
    let metadata_path = std::env::var("PINTAIL_SNAPSHOT_CRASH_META").expect("crash metadata path");
    let table_directory = std::env::var("PINTAIL_SNAPSHOT_CRASH_TABLE").expect("crash table path");
    let database_id = std::env::var("PINTAIL_SNAPSHOT_CRASH_DATABASE").expect("crash database ID");
    let acknowledgement_address =
        std::env::var("PINTAIL_SNAPSHOT_CRASH_ACK").expect("crash acknowledgement address");
    let acknowledgement = Arc::new(Mutex::new(
        TcpStream::connect(acknowledgement_address).expect("connect acknowledgement socket"),
    ));
    let pool = Pool::new(Opts::from_url(&dsn).expect("crash worker DSN"));
    let report = probe(&pool, "app").await.expect("crash worker probe");
    let source = report
        .tables
        .iter()
        .find(|table| table.name == "resume_rows")
        .expect("resume source");
    let snapshot_target = target(source, Path::new(&table_directory));
    let _result = run_snapshot_with_progress(
        &pool,
        Path::new(&metadata_path),
        &database_id,
        &report,
        vec![snapshot_target],
        SnapshotOptions {
            workers: 1,
            chunk_rows: 1_000,
            ..SnapshotOptions::default()
        },
        move |_| {
            let mut acknowledgement = acknowledgement.lock().expect("acknowledgement socket lock");
            acknowledgement
                .write_all(&[1])
                .and_then(|()| acknowledgement.flush())
                .expect("acknowledge snapshot chunk");
            thread::sleep(Duration::from_millis(10));
        },
    )
    .await;
}

fn kill_snapshot_worker(
    dsn: &str,
    metadata_path: &Path,
    table_directory: &Path,
    database_id: &str,
) {
    let acknowledgement =
        TcpListener::bind(("127.0.0.1", 0)).expect("bind snapshot acknowledgement socket");
    let acknowledgement_address = acknowledgement
        .local_addr()
        .expect("snapshot acknowledgement address");
    acknowledgement
        .set_nonblocking(true)
        .expect("nonblocking acknowledgement listener");
    let executable = std::env::current_exe().expect("current integration test binary");
    let mut child = Command::new(executable)
        .args(["--exact", "snapshot_crash_worker", "--ignored"])
        .env("PINTAIL_SNAPSHOT_CRASH_DSN", dsn)
        .env("PINTAIL_SNAPSHOT_CRASH_META", metadata_path)
        .env("PINTAIL_SNAPSHOT_CRASH_TABLE", table_directory)
        .env("PINTAIL_SNAPSHOT_CRASH_DATABASE", database_id)
        .env(
            "PINTAIL_SNAPSHOT_CRASH_ACK",
            acknowledgement_address.to_string(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn snapshot crash worker");
    let mut acknowledgement_stream = None;
    for _ in 0..3_000 {
        if let Some(status) = child.try_wait().expect("poll snapshot worker") {
            panic!("snapshot worker exited before it could be killed: {status}");
        }
        match acknowledgement.accept() {
            Ok((stream, _)) => {
                acknowledgement_stream = Some(stream);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept snapshot acknowledgement: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut acknowledgement_stream = acknowledgement_stream.unwrap_or_else(|| {
        let _kill = child.kill();
        panic!("snapshot worker did not connect its acknowledgement socket")
    });
    acknowledgement_stream
        .set_nonblocking(false)
        .expect("blocking acknowledgement stream");
    acknowledgement_stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("snapshot acknowledgement timeout");
    acknowledgement_stream
        .read_exact(&mut [0_u8; 2])
        .expect("read two durable chunk acknowledgements");
    child.kill().expect("kill snapshot worker");
    let status = child.wait().expect("reap snapshot worker");
    assert!(
        !status.success(),
        "killed worker must not exit successfully"
    );
}

fn target(source: &pintail_probe::SourceTable, directory: &Path) -> SnapshotTarget {
    let store = TableStore::open(
        directory,
        source.table_schema().expect("source schema"),
        StoreOptions::default(),
    )
    .expect("open target store");
    SnapshotTarget::new(source.clone(), store).expect("snapshot target")
}

async fn assert_fact_checksum(pool: &Pool, target: &SnapshotTarget) {
    let sql = format!(
        "SELECT COUNT(*), CAST(SUM(id) AS UNSIGNED), \
         CAST(SUM(CRC32(CONCAT_WS('#', id, payload))) AS UNSIGNED) \
         FROM `{}`",
        target.source().name
    );
    let (source_count, source_sum, source_crc): (u64, u64, u64) = pool
        .get_conn()
        .await
        .expect("source connection")
        .query_first(sql)
        .await
        .expect("source checksum")
        .expect("checksum row");
    let rows = target.store().snapshot().scan().expect("fact scan");
    let mut id_sum = 0_u64;
    let mut crc_sum = 0_u64;
    for row in &rows {
        let [Value::UInt64(id), Value::Utf8(payload)] = row.values() else {
            panic!("unexpected fact row");
        };
        id_sum = id_sum.saturating_add(*id);
        crc_sum = crc_sum.saturating_add(u64::from(crc32fast::hash(
            format!("{id}#{payload}").as_bytes(),
        )));
    }
    assert_eq!(source_count, FACT_ROWS);
    assert_eq!(rows.len() as u64, source_count);
    assert_eq!(id_sum, source_sum);
    assert_eq!(crc_sum, source_crc);
}

fn assert_key_modes(report: &pintail_probe::ProbeReport) {
    let modes = report
        .tables
        .iter()
        .map(|table| (table.name.as_str(), table.key.mode))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(modes["primary_table"], KeyMode::Primary);
    assert_eq!(modes["composite_table"], KeyMode::Primary);
    assert_eq!(modes["unique_table"], KeyMode::Unique);
    assert_eq!(modes["append_table"], KeyMode::AppendRowId);
    assert_eq!(modes["gipk_table"], KeyMode::Primary);
}

fn assert_type_mapping(report: &pintail_probe::ProbeReport) {
    let table = report
        .tables
        .iter()
        .find(|table| table.name == "type_fidelity")
        .expect("type table");
    let types = table
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.pintail_type))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(types["i8_signed"], DataType::Int8);
    assert_eq!(types["i8_unsigned"], DataType::UInt8);
    assert_eq!(types["i16_signed"], DataType::Int16);
    assert_eq!(types["i32_unsigned"], DataType::UInt32);
    assert_eq!(
        types["decimal_exact"],
        DataType::Decimal {
            precision: 38,
            scale: 10
        }
    );
    assert_eq!(types["date_value"], DataType::Date32);
    assert_eq!(types["datetime_value"], DataType::DateTime64 { fsp: 6 });
    assert_eq!(types["time_value"], DataType::Time64 { fsp: 6 });
    assert_eq!(types["json_value"], DataType::Json);
    assert!(!types.contains_key("virtual_value"));
    assert!(types.contains_key("stored_value"));
}

fn assert_type_values(targets: &[SnapshotTarget]) {
    let target = targets
        .iter()
        .find(|target| target.source().name == "type_fidelity")
        .expect("type target");
    let rows = target.store().snapshot().scan().expect("type scan");
    assert_eq!(rows.len(), 2);
    let columns = target
        .source()
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        rows[0].values()[columns["date_value"]],
        Value::Utf8("1000-01-01".to_owned())
    );
    assert_eq!(
        rows[0].values()[columns["datetime_value"]],
        Value::Utf8("2024-02-29 12:34:56.123456".to_owned())
    );
    assert_eq!(
        rows[0].values()[columns["time_value"]],
        Value::Utf8("-51:04:05.600000".to_owned())
    );
    assert_eq!(
        rows[0].values()[columns["json_value"]],
        Value::Utf8("{\"a\":1,\"b\":[true,null]}".to_owned())
    );
    assert_eq!(
        rows[0].values()[columns["latin_value"]],
        Value::Utf8("café".to_owned())
    );
    // MySQL returns the all-zero date from a SELECT, does not match it with
    // IS NULL, and counts it in COUNT(column); the replica preserves it
    // rather than inverting all three.
    assert_eq!(
        rows[1].values()[columns["date_value"]],
        Value::Utf8("0000-00-00".to_owned())
    );
    assert_eq!(
        rows[1].values()[columns["datetime_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned())
    );
    assert_eq!(
        rows[1].values()[columns["timestamp_value"]],
        Value::Utf8("0000-00-00 00:00:00.000000".to_owned())
    );
}

fn assert_pk_counts(targets: &[SnapshotTarget]) {
    for (table, expected) in [
        ("primary_table", 2),
        ("composite_table", 3),
        ("unique_table", 2),
        ("append_table", 3),
        ("gipk_table", 2),
    ] {
        let target = targets
            .iter()
            .find(|target| target.source().name == table)
            .expect("PK matrix target");
        assert_eq!(
            target.store().snapshot().scan().expect("PK scan").len(),
            expected,
            "{table}"
        );
    }
}

fn compatibility_source_schema() -> &'static str {
    "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
     GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
       ON *.* TO 'pintail'@'%';\
     CREATE TABLE primary_table (id BIGINT PRIMARY KEY, value VARCHAR(32));\
     INSERT INTO primary_table VALUES (1,'one'),(2,'two'),(3,'three');\
     CREATE TABLE composite_table (tenant INT NOT NULL, id INT NOT NULL, value VARCHAR(32), \
       PRIMARY KEY (tenant,id));\
     INSERT INTO composite_table VALUES (1,1,'a'),(1,2,'b'),(2,1,'c');\
     CREATE TABLE unique_table (email VARCHAR(64) NOT NULL UNIQUE, value VARCHAR(32));\
     INSERT INTO unique_table VALUES ('a@example.com','a'),('b@example.com','b');\
     CREATE TABLE append_table (value VARCHAR(32));\
     INSERT INTO append_table VALUES ('same'),('same'),('different');\
     CREATE TABLE compat_types (\
       id BIGINT UNSIGNED PRIMARY KEY,\
       decimal_value DECIMAL(38,10),\
       date_value DATE,\
       datetime_value DATETIME(6),\
       latin_value VARCHAR(32) CHARACTER SET latin1,\
       binary_value VARBINARY(8)\
     );\
     INSERT INTO compat_types VALUES \
       (1,0.0000000000,'0000-00-00','0000-00-00 00:00:00.000000','plain',0x00),\
       (2,1234567890123456789012345678.1234567890,'1000-01-01',\
        '2024-02-29 12:34:56.123456',_latin1 0x636166E9,0x00FF);"
}

#[allow(clippy::too_many_lines)]
fn source_schema() -> String {
    let mut sql = String::from(
        "CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail';\
         GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT \
           ON *.* TO 'pintail'@'%';\
         CREATE TABLE digits (d TINYINT UNSIGNED PRIMARY KEY);\
         INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);\
         CREATE TABLE resume_rows (id BIGINT UNSIGNED PRIMARY KEY, payload VARCHAR(32));\
         INSERT INTO resume_rows \
           SELECT n, CONCAT('resume-', n) FROM (\
             SELECT d0.d + d1.d*10 + d2.d*100 + d3.d*1000 + d4.d*10000 AS n \
             FROM digits d0 CROSS JOIN digits d1 CROSS JOIN digits d2 \
             CROSS JOIN digits d3 CROSS JOIN digits d4\
           ) numbers;\
         CREATE TABLE primary_table (id BIGINT PRIMARY KEY, value VARCHAR(32));\
         INSERT INTO primary_table VALUES (1,'one'),(2,'two');\
         CREATE TABLE composite_table (tenant INT NOT NULL, id INT NOT NULL, value VARCHAR(32), \
           PRIMARY KEY (tenant,id));\
         INSERT INTO composite_table VALUES (1,1,'a'),(1,2,'b'),(2,1,'c');\
         CREATE TABLE unique_table (email VARCHAR(64) NOT NULL UNIQUE, value VARCHAR(32));\
         INSERT INTO unique_table VALUES ('a@example.com','a'),('b@example.com','b');\
         CREATE TABLE append_table (value VARCHAR(32));\
         INSERT INTO append_table VALUES ('same'),('same'),('different');\
         SET SESSION sql_generate_invisible_primary_key=ON;\
         CREATE TABLE gipk_table (value VARCHAR(32));\
         SET SESSION sql_generate_invisible_primary_key=OFF;\
         INSERT INTO gipk_table (value) VALUES ('generated-a'),('generated-b');\
         CREATE TABLE type_fidelity (\
           id BIGINT UNSIGNED PRIMARY KEY,\
           bool_value BOOLEAN,\
           i8_signed TINYINT,\
           i8_unsigned TINYINT UNSIGNED,\
           i16_signed SMALLINT,\
           i32_unsigned INT UNSIGNED,\
           i64_signed BIGINT,\
           decimal_exact DECIMAL(38,10),\
           decimal_text DECIMAL(65,20),\
           float_value FLOAT,\
           double_value DOUBLE,\
           bit_value BIT(9),\
           date_value DATE,\
           datetime_value DATETIME(6),\
           timestamp_value TIMESTAMP(6) NULL,\
           time_value TIME(6),\
           year_value YEAR,\
           latin_value VARCHAR(32) CHARACTER SET latin1,\
           enum_value ENUM('alpha','βeta'),\
           set_value SET('red','green','blue'),\
           json_value JSON,\
           binary_value VARBINARY(8),\
           blob_value BLOB,\
           geometry_value POINT,\
           stored_value BIGINT GENERATED ALWAYS AS (i64_signed + 1) STORED,\
           virtual_value BIGINT GENERATED ALWAYS AS (i64_signed + 2) VIRTUAL\
         );\
         INSERT INTO type_fidelity (\
           id,bool_value,i8_signed,i8_unsigned,i16_signed,i32_unsigned,i64_signed,\
           decimal_exact,decimal_text,float_value,double_value,bit_value,date_value,\
           datetime_value,timestamp_value,time_value,year_value,latin_value,enum_value,\
           set_value,json_value,binary_value,blob_value,geometry_value\
         ) VALUES (\
           1,TRUE,-128,255,-32768,4294967295,-9223372036854775808,\
           1234567890123456789012345678.1234567890,\
           123456789012345678901234567890123456789012345.12345678901234567890,\
           1.25,2.5,b'101010101','1000-01-01','2024-02-29 12:34:56.123456',\
           '1970-01-01 00:00:01.000001','-51:04:05.600000',1901,_latin1 0x636166E9,\
           'βeta','red,blue',JSON_OBJECT('b',JSON_ARRAY(TRUE,NULL),'a',1),\
           0x00FF10,0xDEADBEEF,ST_GeomFromText('POINT(1 2)')\
         ),(\
           2,FALSE,0,0,0,0,0,0.0000000000,0.00000000000000000000,\
           0,0,b'0','0000-00-00','0000-00-00 00:00:00.000000',\
           '0000-00-00 00:00:00.000000','00:00:00.000000',2024,'plain',\
           'alpha','green',JSON_OBJECT(),X'',X'',ST_GeomFromText('POINT(0 0)')\
         );",
    );
    for index in 0..10 {
        write!(
            sql,
            "CREATE TABLE fact_{index:02} (id BIGINT UNSIGNED PRIMARY KEY, payload VARCHAR(64));\
             INSERT INTO fact_{index:02} \
               SELECT n, CONCAT('fact_{index:02}-', n) FROM (\
                 SELECT d0.d + d1.d*10 + d2.d*100 + d3.d*1000 + d4.d*10000 AS n \
                 FROM digits d0 CROSS JOIN digits d1 CROSS JOIN digits d2 \
                 CROSS JOIN digits d3 CROSS JOIN digits d4\
               ) numbers;"
        )
        .expect("writing source schema cannot fail");
    }
    sql
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
