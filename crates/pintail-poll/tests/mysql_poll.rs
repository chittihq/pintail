use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use mysql_async::{Opts, Pool};
use pintail_meta::MetaStore;
use pintail_poll::{PollOptions, PollStrategy, PollTarget, run_poll_cycle};
use pintail_probe::probe;
use pintail_snapshot::{SnapshotOptions, SnapshotTarget, run_snapshot};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::Value;

const DATABASE_ID: &str = "m5-poll-source";

struct MysqlContainer {
    name: String,
    host: String,
    port: u16,
}

impl MysqlContainer {
    fn start() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let name = format!("pintail-m5-poll-{}-{nonce}", std::process::id());
        checked_output(
            Command::new("docker").args([
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
                "mysql:8.4",
                "--skip-log-bin",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ]),
            "start polling source",
        )?;
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
        let container = Self { name, host, port };
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
        Err("polling source did not become ready within 120 seconds".to_owned())
    }

    fn dsn(&self) -> String {
        format!("mysql://pintail:pintail@{}:{}/app", self.host, self.port)
    }

    fn query_batch(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "--interactive",
                &self.name,
                "mysql",
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
async fn polling_crud_delete_repair_unique_reuse_cascade_and_noop_storage() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(SOURCE_SCHEMA)
        .unwrap_or_else(|error| panic!("{error}"));
    let pool = Pool::new(Opts::from_url(&mysql.dsn()).expect("poll DSN"));
    let report = probe(&pool, "app").await.expect("probe polling source");
    assert!(!report.capabilities.log_bin);
    let child = report
        .tables
        .iter()
        .find(|table| table.name == "cascade_child")
        .expect("cascade child metadata");
    assert!(child.requires_reconciliation);

    let workspace = tempfile::tempdir().expect("poll workspace");
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
    let table_names = [
        "append_rows",
        "cascade_child",
        "cascade_parent",
        "composite_rows",
        "cursor_rows",
        "keyed_rows",
    ];
    let snapshot_targets = report
        .tables
        .iter()
        .filter(|table| table_names.contains(&table.name.as_str()))
        .map(|source| snapshot_target(source, &workspace.path().join(&source.name)))
        .collect();
    let snapshot = run_snapshot(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        snapshot_targets,
        SnapshotOptions {
            workers: 2,
            chunk_rows: 2,
            ..SnapshotOptions::default()
        },
    )
    .await
    .expect("initial polling snapshot");
    let mut targets = snapshot
        .targets
        .into_iter()
        .map(|target| {
            let source = target.source().clone();
            PollTarget::new(source, target.into_store()).expect("poll target")
        })
        .collect();

    let baseline = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        options(false),
    )
    .await
    .expect("establish poll checkpoints");
    assert!(
        baseline
            .tables
            .iter()
            .any(|table| table.table == "cursor_rows" && table.strategy == PollStrategy::Cursor)
    );
    assert!(baseline.tables.iter().any(|table| {
        table.table == "keyed_rows" && table.strategy == PollStrategy::KeyedChecksum
    }));
    assert_eq!(outcome(&baseline, "keyed_rows").chunks_scanned, 2);
    assert_eq!(outcome(&baseline, "keyed_rows").chunks_redumped, 2);
    assert!(baseline.tables.iter().any(|table| {
        table.table == "append_rows" && table.strategy == PollStrategy::AppendRebuild
    }));
    targets = baseline.targets;

    mysql
        .query_batch(
            "UPDATE cursor_rows SET value='alpha-2',updated_at=NOW(6) WHERE id=1;\
             UPDATE cursor_rows SET deleted_at=NOW(6),updated_at=NOW(6) WHERE id=2;\
             INSERT INTO cursor_rows(email,value,updated_at) \
               VALUES ('three@example.com','gamma',NOW(6));\
             UPDATE keyed_rows SET value='one-2' WHERE id=1;\
             DELETE FROM keyed_rows WHERE id=2;\
             INSERT INTO keyed_rows VALUES (3,'three');\
             DELETE FROM composite_rows WHERE tenant_id=1 AND item_id=2;\
             UPDATE composite_rows SET value='two-one-updated' \
               WHERE tenant_id=2 AND item_id=1;\
             INSERT INTO composite_rows VALUES (2,2,'two-two');\
             INSERT INTO append_rows VALUES ('second');",
        )
        .expect("mutate polled tables");
    let changed = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        options(false),
    )
    .await
    .expect("poll changed rows");
    let cursor = outcome(&changed, "cursor_rows");
    assert_eq!((cursor.ingested, cursor.tombstones), (2, 1));
    let keyed = outcome(&changed, "keyed_rows");
    assert_eq!((keyed.ingested, keyed.tombstones), (2, 1));
    assert_eq!((keyed.chunks_scanned, keyed.chunks_redumped), (2, 1));
    assert_eq!(
        (
            outcome(&changed, "composite_rows").ingested,
            outcome(&changed, "composite_rows").tombstones,
        ),
        (2, 1)
    );
    assert_eq!(outcome(&changed, "append_rows").ingested, 2);
    targets = changed.targets;
    assert_ids(&targets, "cursor_rows", &[1, 3]);
    assert_ids(&targets, "keyed_rows", &[1, 3]);
    assert_composite_keys(&targets, "composite_rows", &[(1, 1), (2, 1), (2, 2)]);
    assert_eq!(row_count(&targets, "append_rows"), 2);

    mysql
        .query_batch(
            "DELETE FROM cursor_rows WHERE id=1;\
             INSERT INTO cursor_rows(email,value,updated_at) \
               VALUES ('one@example.com','replacement',NOW(6));",
        )
        .expect("reuse unique value");
    let repaired = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        options(false),
    )
    .await
    .expect("audit-triggered repair");
    assert!(outcome(&repaired, "cursor_rows").reconciled);
    assert_eq!(outcome(&repaired, "cursor_rows").unique_repairs, 1);
    targets = repaired.targets;
    assert_ids(&targets, "cursor_rows", &[3, 4]);

    let token_before_blindspot = MetaStore::open(&metadata_path)
        .unwrap()
        .poll_state(DATABASE_ID, "cursor_rows")
        .unwrap()
        .unwrap()
        .source_token_json;
    mysql
        .query_batch(
            "SET @pintail_boundary=(SELECT MAX(updated_at) FROM cursor_rows);\
             DELETE FROM cursor_rows WHERE id=3;\
             INSERT INTO cursor_rows(email,value,updated_at) \
               VALUES ('five@example.com','epsilon',@pintail_boundary);",
        )
        .expect("create count-neutral delete blind spot");
    let blind = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        options(false),
    )
    .await
    .expect("cursor sync before reconcile");
    assert_eq!(outcome(&blind, "cursor_rows").ingested, 1);
    assert_eq!(
        MetaStore::open(&metadata_path)
            .unwrap()
            .poll_state(DATABASE_ID, "cursor_rows")
            .unwrap()
            .unwrap()
            .source_token_json,
        token_before_blindspot,
        "count/MAX token is intentionally unchanged in the regression window"
    );
    targets = blind.targets;
    assert_ids(&targets, "cursor_rows", &[3, 4, 5]);
    let reconciled = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        options(true),
    )
    .await
    .expect("scheduled delete reconciliation");
    assert_eq!(outcome(&reconciled, "cursor_rows").tombstones, 1);
    targets = reconciled.targets;
    assert_ids(&targets, "cursor_rows", &[4, 5]);

    mysql
        .query_batch("DELETE FROM cascade_parent WHERE id=1;")
        .expect("cascade source delete");
    let mut cascade_options = options(false);
    cascade_options
        .reconcile_tables
        .insert("cascade_child".to_owned());
    let cascade = run_poll_cycle(
        &pool,
        &metadata_path,
        DATABASE_ID,
        &report,
        targets,
        cascade_options,
    )
    .await
    .expect("CDC-mode cascade reconciliation");
    assert_eq!(outcome(&cascade, "cascade_child").tombstones, 1);
    targets = cascade.targets;
    assert_ids(&targets, "cascade_child", &[11]);

    let before = table_storage_bytes(workspace.path());
    for _ in 0..10 {
        let idle = run_poll_cycle(
            &pool,
            &metadata_path,
            DATABASE_ID,
            &report,
            targets,
            PollOptions {
                force: true,
                ..options(false)
            },
        )
        .await
        .expect("idle forced polling scan");
        assert!(
            idle.tables
                .iter()
                .all(|table| table.ingested == 0 && table.tombstones == 0)
        );
        assert_eq!(outcome(&idle, "keyed_rows").chunks_redumped, 0);
        targets = idle.targets;
    }
    assert_eq!(table_storage_bytes(workspace.path()), before);
    pool.disconnect().await.expect("disconnect polling source");
}

fn options(reconcile: bool) -> PollOptions {
    PollOptions {
        chunk_rows: 2,
        reconcile,
        soft_delete_columns: [("cursor_rows".to_owned(), "deleted_at".to_owned())]
            .into_iter()
            .collect(),
        ..PollOptions::default()
    }
}

fn snapshot_target(source: &pintail_probe::SourceTable, directory: &Path) -> SnapshotTarget {
    let store = TableStore::open(
        directory,
        source.table_schema().expect("source schema"),
        StoreOptions::default(),
    )
    .expect("open polling store");
    SnapshotTarget::new(source.clone(), store).expect("snapshot target")
}

fn outcome<'a>(
    result: &'a pintail_poll::PollResult,
    table: &str,
) -> &'a pintail_poll::TablePollOutcome {
    result
        .tables
        .iter()
        .find(|outcome| outcome.table == table)
        .unwrap_or_else(|| panic!("missing {table} outcome"))
}

fn table<'a>(targets: &'a [PollTarget], name: &str) -> &'a PollTarget {
    targets
        .iter()
        .find(|target| target.source().name == name)
        .unwrap_or_else(|| panic!("missing {name} target"))
}

fn row_count(targets: &[PollTarget], name: &str) -> usize {
    table(targets, name)
        .store()
        .snapshot()
        .scan()
        .expect("scan poll table")
        .len()
}

fn assert_ids(targets: &[PollTarget], name: &str, expected: &[u64]) {
    let mut ids = table(targets, name)
        .store()
        .snapshot()
        .scan()
        .expect("scan IDs")
        .into_iter()
        .map(|row| match row.values().first() {
            Some(Value::UInt64(id)) => *id,
            value => panic!("unexpected {name} ID {value:?}"),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, expected);
}

fn assert_composite_keys(targets: &[PollTarget], name: &str, expected: &[(u64, u64)]) {
    let mut keys = table(targets, name)
        .store()
        .snapshot()
        .scan()
        .expect("scan composite keys")
        .into_iter()
        .map(|row| match row.values() {
            [Value::UInt64(left), Value::UInt64(right), ..] => (*left, *right),
            values => panic!("unexpected {name} composite values {values:?}"),
        })
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, expected);
}

fn table_storage_bytes(root: &Path) -> u64 {
    [
        "append_rows",
        "cascade_child",
        "cascade_parent",
        "composite_rows",
        "cursor_rows",
        "keyed_rows",
    ]
    .iter()
    .map(|name| directory_bytes(&root.join(name)))
    .sum()
}

fn directory_bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .expect("read table directory")
        .map(|entry| {
            let entry = entry.expect("table directory entry");
            let metadata = entry.metadata().expect("table entry metadata");
            if metadata.is_dir() {
                directory_bytes(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
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

fn checked_output(command: &mut Command, operation: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("{operation}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format_output_error(operation, &output))
    }
}

fn format_output_error(operation: &str, output: &Output) -> String {
    format!(
        "{operation} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

const SOURCE_SCHEMA: &str = "\
CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail';\
GRANT SELECT, RELOAD, REPLICATION CLIENT ON *.* TO 'pintail'@'%';\
CREATE TABLE cursor_rows (\
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,\
  email VARCHAR(128) NOT NULL UNIQUE,\
  value VARCHAR(128) NOT NULL,\
  updated_at DATETIME(6) NOT NULL,\
  deleted_at DATETIME(6) NULL\
);\
INSERT INTO cursor_rows(email,value,updated_at) VALUES\
  ('one@example.com','alpha',NOW(6)),\
  ('two@example.com','beta',NOW(6));\
CREATE TABLE keyed_rows (id BIGINT UNSIGNED NOT NULL PRIMARY KEY, value VARCHAR(128) NOT NULL);\
INSERT INTO keyed_rows VALUES (1,'one'),(2,'two');\
CREATE TABLE append_rows (value VARCHAR(128) NOT NULL);\
INSERT INTO append_rows VALUES ('first');\
CREATE TABLE composite_rows (\
  tenant_id BIGINT UNSIGNED NOT NULL,\
  item_id BIGINT UNSIGNED NOT NULL,\
  value VARCHAR(128) NOT NULL,\
  PRIMARY KEY (tenant_id,item_id)\
);\
INSERT INTO composite_rows VALUES (1,1,'one-one'),(1,2,'one-two'),(2,1,'two-one');\
CREATE TABLE cascade_parent (id BIGINT UNSIGNED NOT NULL PRIMARY KEY);\
CREATE TABLE cascade_child (\
  id BIGINT UNSIGNED NOT NULL PRIMARY KEY,\
  parent_id BIGINT UNSIGNED NOT NULL,\
  CONSTRAINT child_parent FOREIGN KEY (parent_id) REFERENCES cascade_parent(id) ON DELETE CASCADE\
);\
INSERT INTO cascade_parent VALUES (1),(2);\
INSERT INTO cascade_child VALUES (10,1),(11,2);\
";
