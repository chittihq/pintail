use std::{
    collections::BTreeMap,
    io::Write as _,
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use mysql_async::{Opts, Pool};
use pintail_cdc::{CdcOptions, CdcTarget, run_cdc};
use pintail_meta::MetaStore;
use pintail_probe::probe;
use pintail_snapshot::{SnapshotOptions, SnapshotTarget, run_snapshot};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::Value;

const DATABASE_ID: &str = "m4-source";

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
        let name = format!("pintail-m4-mysql84-{}-{nonce}", std::process::id());
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
                "--server-id=184",
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--gtid-mode=ON",
                "--enforce-gtid-consistency=ON",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ]),
            "start MySQL 8.4 CDC source",
        )?;
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
        let container = Self { name, host, port };
        for _ in 0..120 {
            if container.query_batch("SELECT 1;").is_ok() {
                return Ok(container);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err("MySQL 8.4 did not become ready within 60 seconds".to_owned())
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
            ..CdcOptions::default()
        },
    )
    .await
    .expect("finite CDC catch-up");
    assert!(result.commits >= 4);
    assert_eq!(result.mutations, 7);
    assert_eq!(result.checkpoint.kind, "gtid");
    assert_replica(&result.targets);
    pool.disconnect().await.expect("disconnect source pool");
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
    assert_eq!(row.values()[columns["date_value"]], Value::Null);
    assert_eq!(row.values()[columns["datetime_value"]], Value::Null);
    assert_eq!(row.values()[columns["timestamp_value"]], Value::Null);
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
     INSERT INTO type_rows VALUES (\
       2,1234567890123456789012345678.1234567890,b'101010101',\
       '0000-00-00','0000-00-00 00:00:00.000000','0000-00-00 00:00:00.000000',\
       _latin1 0x636166E9,'βeta','red,blue',\
       JSON_OBJECT('b',JSON_ARRAY(TRUE,NULL),'a',1),0x00FF10,0xDEADBEEF\
     );"
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
