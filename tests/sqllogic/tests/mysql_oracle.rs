use std::{
    fmt::Write as _,
    io::Write as _,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const EVENTS_ID: TableId = TableId::new(1);
const USERS_ID: TableId = TableId::new(2);
const MEMORY_LIMIT: usize = 4 * 1024 * 1024;
const EXPECTED_CASES: usize = 600;

struct OracleCase {
    family: &'static str,
    sql: String,
}

struct MysqlContainer {
    name: String,
}

impl MysqlContainer {
    fn start() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let name = format!("pintail-mysql-oracle-{}-{nonce}", std::process::id());
        checked_output(
            Command::new("docker").args([
                "run",
                "--detach",
                "--name",
                &name,
                "--env",
                "MYSQL_ALLOW_EMPTY_PASSWORD=yes",
                "--env",
                "MYSQL_DATABASE=app",
                "mysql:8.4",
                "--skip-log-bin",
                "--default-time-zone=+00:00",
            ]),
            "start MySQL 8.4 oracle",
        )?;

        let container = Self { name };
        let mut consecutive_connections = 0;
        for _ in 0..120 {
            let connected = Command::new("docker")
                .args([
                    "exec",
                    &container.name,
                    "mysql",
                    "--user=root",
                    "--database=app",
                    "--execute=SELECT 1",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if connected {
                consecutive_connections += 1;
            } else {
                consecutive_connections = 0;
            }
            if consecutive_connections == 5 {
                return Ok(container);
            }
            thread::sleep(Duration::from_millis(500));
        }

        Err("MySQL 8.4 did not become ready within 60 seconds".to_owned())
    }

    fn query_batch(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "--interactive",
                &self.name,
                "mysql",
                "--user=root",
                "--database=app",
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
            .ok_or_else(|| "MySQL client stdin was not piped".to_owned())?
            .write_all(sql.as_bytes())
            .map_err(|error| format!("write MySQL query batch: {error}"))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for MySQL query batch: {error}"))?;
        if !output.status.success() {
            return Err(format_output_error("execute MySQL query batch", &output));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| format!("MySQL emitted non-UTF-8 output: {error}"))
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

#[test]
#[ignore = "requires Docker and the mysql:8.4 image; run explicitly as documented"]
fn matches_mysql_8_4_for_six_hundred_queries() {
    run_oracle().unwrap_or_else(|error| panic!("{error}"));
}

#[allow(clippy::too_many_lines)]
fn run_oracle() -> Result<(), String> {
    let mysql = MysqlContainer::start()?;
    mysql.query_batch(
        "CREATE TABLE events (\
           id BIGINT UNSIGNED PRIMARY KEY,\
           name VARCHAR(32) NOT NULL,\
           score BIGINT NOT NULL,\
           active BOOLEAN NOT NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         CREATE TABLE users (\
           id BIGINT UNSIGNED PRIMARY KEY,\
           name VARCHAR(32) NOT NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         INSERT INTO events VALUES\
           (1,'event-01',10,0),(2,'event-02',20,1),\
           (3,'event-03',30,0),(4,'event-04',40,1),\
           (5,'event-05',50,0),(6,'event-06',60,1),\
           (7,'event-07',70,0),(8,'event-08',80,1),\
           (9,'event-09',90,0),(10,'event-10',100,1);\
         INSERT INTO users VALUES\
           (1,'user-01'),(2,'user-02'),(3,'user-03'),(4,'user-04'),\
           (5,'user-05'),(6,'user-06'),(7,'user-07'),(8,'user-08'),\
           (9,'user-09'),(10,'user-10');",
    )?;

    let events_directory =
        tempfile::tempdir().map_err(|error| format!("events tempdir: {error}"))?;
    let users_directory = tempfile::tempdir().map_err(|error| format!("users tempdir: {error}"))?;
    let events_schema = events_schema()?;
    let users_schema = users_schema()?;
    let mut events = TableStore::open(
        events_directory.path(),
        events_schema.clone(),
        StoreOptions::default(),
    )
    .map_err(|error| format!("open events: {error}"))?;
    let mut users = TableStore::open(
        users_directory.path(),
        users_schema.clone(),
        StoreOptions::default(),
    )
    .map_err(|error| format!("open users: {error}"))?;
    events
        .ingest((1..=10).map(event_row).collect())
        .map_err(|error| format!("ingest events: {error}"))?;
    users
        .ingest((1..=10).map(user_row).collect())
        .map_err(|error| format!("ingest users: {error}"))?;
    let events_snapshot = events.snapshot();
    let users_snapshot = users.snapshot();
    let catalog = catalog(events_schema, users_schema)?;
    let provider = SnapshotScanProvider::new([
        (DATABASE_ID, EVENTS_ID, &events_snapshot),
        (DATABASE_ID, USERS_ID, &users_snapshot),
    ])
    .map_err(|error| format!("create snapshot provider: {error}"))?;

    let cases = oracle_cases();
    if cases.len() != EXPECTED_CASES {
        return Err(format!(
            "oracle generator produced {} cases, expected {EXPECTED_CASES}",
            cases.len()
        ));
    }
    let mysql_results = execute_mysql_cases(&mysql, &cases)?;
    let mut failures = Vec::new();
    for (index, (case, expected)) in cases.iter().zip(&mysql_results).enumerate() {
        let actual = execute_pintail(&case.sql, &catalog, &provider)
            .map_err(|error| format!("case {index} ({}) `{}`: {error}", case.family, case.sql))?;
        if actual != *expected {
            failures.push(format!(
                "case {index} ({})\nSQL: {}\nMySQL: {expected:?}\nPintail: {actual:?}",
                case.family, case.sql
            ));
            if failures.len() == 10 {
                break;
            }
        }
    }
    if failures.is_empty() {
        println!(
            "all {EXPECTED_CASES} generated queries matched MySQL 8.4 across 11 operator families"
        );
        Ok(())
    } else {
        Err(format!(
            "{} differential mismatch(es), showing at most 10:\n{}",
            failures.len(),
            failures.join("\n\n")
        ))
    }
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

fn execute_mysql_cases(
    mysql: &MysqlContainer,
    cases: &[OracleCase],
) -> Result<Vec<Vec<String>>, String> {
    let mut sql = String::new();
    for (index, case) in cases.iter().enumerate() {
        writeln!(sql, "SELECT '__PINTAIL_CASE_{index}__';")
            .expect("writing to an owned string cannot fail");
        sql.push_str(&case.sql);
        sql.push_str(";\n");
    }
    writeln!(sql, "SELECT '__PINTAIL_CASE_{}__';", cases.len())
        .expect("writing to an owned string cannot fail");

    let output = mysql.query_batch(&sql)?;
    let mut results = vec![Vec::new(); cases.len()];
    let mut current = None;
    for line in output.lines() {
        if let Some(index) = parse_marker(line) {
            current = (index < cases.len()).then_some(index);
        } else if let Some(index) = current {
            results[index].push(line.to_owned());
        } else if !line.is_empty() {
            return Err(format!(
                "unexpected MySQL output before first marker: {line}"
            ));
        }
    }
    Ok(results)
}

fn parse_marker(line: &str) -> Option<usize> {
    line.strip_prefix("__PINTAIL_CASE_")
        .and_then(|value| value.strip_suffix("__"))
        .and_then(|value| value.parse().ok())
}

fn execute_pintail(
    sql: &str,
    catalog: &CatalogSnapshot,
    provider: &SnapshotScanProvider<'_>,
) -> Result<Vec<String>, String> {
    let statement = parse_statement(sql).map_err(|error| format!("parse: {error}"))?;
    let bound = Binder::new(catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| format!("bind: {error}"))?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical =
        PhysicalPlanner::plan(logical).map_err(|error| format!("physical plan: {error}"))?;
    let mut execution = Execution::start(physical, provider, MEMORY_LIMIT)
        .map_err(|error| format!("start execution: {error}"))?;
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| format!("execute: {error}"))?
    {
        for row in batch.selection().selected_rows() {
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column
                        .value(row)
                        .map(canonical_value)
                        .ok_or_else(|| "result row falls outside a column vector".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values.join("\t"));
        }
    }
    Ok(rows)
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(value) => u8::from(*value).to_string(),
        Value::Int64(value) => value.to_string(),
        Value::UInt64(value) => value.to_string(),
        Value::Float64(value) => value.get().to_string(),
        Value::Utf8(value) => value.clone(),
        Value::Binary(value) => String::from_utf8_lossy(value).into_owned(),
    }
}

#[allow(clippy::too_many_lines)]
fn oracle_cases() -> Vec<OracleCase> {
    let mut cases = Vec::with_capacity(EXPECTED_CASES);
    for value in 0..100 {
        cases.push(OracleCase {
            family: "arithmetic and predicates",
            sql: format!(
                "SELECT {value} + 7, {value} * 3, {value} % 7, \
                 {value} BETWEEN 10 AND 80, {value} IN (3, 17, 42, 88)"
            ),
        });
    }
    for value in 0..100 {
        cases.push(OracleCase {
            family: "strings",
            sql: format!(
                "SELECT CONCAT(LOWER('MiXeD'), '-', {value}), \
                 SUBSTRING('abcdef', 2, 3), TRIM('  pintail  '), \
                 REPLACE('a-b-c', '-', '_'), LEFT('abcdef', 3), \
                 RIGHT('abcdef', 2), LOCATE('tail', 'pintail'), \
                 CONVERT({value}, CHAR), CONVERT('MiXeD' USING utf8mb4)"
            ),
        });
    }
    for value in 0..100 {
        cases.push(OracleCase {
            family: "conditionals and nulls",
            sql: format!(
                "SELECT IF({value} % 2 = 0, 'even', 'odd'), \
                 CASE WHEN {value} < 25 THEN 'low' \
                      WHEN {value} < 75 THEN 'mid' ELSE 'high' END, \
                 IFNULL(NULL, {value}), COALESCE(NULL, NULL, {value} + 1), \
                 NULLIF({value} % 5, 0)"
            ),
        });
    }
    for value in 0..100 {
        let days = value % 28;
        cases.push(OracleCase {
            family: "date and time",
            sql: format!(
                "SELECT DATE_ADD('2024-01-01', INTERVAL {days} DAY), \
                 DATE_SUB('2024-03-01', INTERVAL {days} DAY), \
                 DATEDIFF(DATE_ADD('2024-01-01', INTERVAL {days} DAY), '2024-01-01'), \
                 YEAR('2024-02-29'), MONTH('2024-02-29'), DAY('2024-02-29'), \
                 DATE_FORMAT('2024-02-29 12:34:56', '%Y-%m-%d %H:%i:%s')"
            ),
        });
    }
    for value in 0..50 {
        cases.push(OracleCase {
            family: "constant subqueries",
            sql: format!(
                "SELECT (SELECT {value} + 1), \
                 {value} IN (SELECT 1 UNION ALL SELECT 50 UNION ALL SELECT 99), \
                 {value} NOT IN (SELECT 101 UNION ALL SELECT 102), \
                 (SELECT NULL)"
            ),
        });
    }
    for value in 0..25 {
        let threshold = value % 10 + 1;
        let limit = value % 4 + 1;
        let sql = if value % 2 == 0 {
            format!(
                "SELECT id, (SELECT MAX(id) FROM users), \
                 id IN (SELECT id FROM users WHERE id >= {threshold}) \
                 FROM events WHERE id = {threshold} ORDER BY id"
            )
        } else {
            format!(
                "SELECT id, name FROM events \
                 WHERE id IN (SELECT id FROM users WHERE id >= {threshold}) \
                 ORDER BY id LIMIT {limit}"
            )
        };
        cases.push(OracleCase {
            family: "relational subqueries",
            sql,
        });
    }
    for value in 0..25 {
        let threshold = value % 8 + 1;
        cases.push(OracleCase {
            family: "common table expressions",
            sql: format!(
                "WITH recent (event_id, label, flag) AS (\
                   SELECT id, name, active FROM events WHERE id >= {threshold}\
                 ) \
                 SELECT flag, COUNT(*), MIN(label), MAX(label) FROM recent \
                 GROUP BY flag HAVING COUNT(*) >= 1 ORDER BY flag"
            ),
        });
    }
    for value in 0..25 {
        let threshold = value % 10 + 1;
        let limit = value % 4 + 1;
        cases.push(OracleCase {
            family: "filter and sort",
            sql: format!(
                "SELECT id, name, score FROM events \
                 WHERE id >= {threshold} ORDER BY name DESC LIMIT {limit}"
            ),
        });
    }
    for value in 0..25 {
        let threshold = value % 8 + 1;
        cases.push(OracleCase {
            family: "hash aggregate",
            sql: format!(
                "SELECT active, COUNT(*), SUM(score), MIN(name), MAX(name) \
                 FROM events WHERE id >= {threshold} \
                 GROUP BY active HAVING COUNT(*) >= 1 ORDER BY active"
            ),
        });
    }
    for value in 0..25 {
        let threshold = value % 10 + 1;
        let limit = value % 5 + 1;
        cases.push(OracleCase {
            family: "hash join",
            sql: format!(
                "SELECT e.id, e.name, u.name FROM events AS e \
                 INNER JOIN users AS u ON e.id = u.id \
                 WHERE e.id >= {threshold} ORDER BY e.id LIMIT {limit}"
            ),
        });
    }
    for value in 0..25 {
        let limit = value % 3 + 1;
        cases.push(OracleCase {
            family: "union all",
            sql: format!(
                "SELECT {value} AS value UNION ALL SELECT {} UNION ALL SELECT {} \
                 ORDER BY value LIMIT {limit}",
                value + 2,
                value + 1
            ),
        });
    }
    cases
}

fn catalog(
    events_schema: TableSchema,
    users_schema: TableSchema,
) -> Result<CatalogSnapshot, String> {
    let events = TableEntry::new(
        EVENTS_ID,
        "events",
        events_schema,
        TableStatistics::with_row_count(10),
    )
    .map_err(|error| error.to_string())?;
    let users = TableEntry::new(
        USERS_ID,
        "users",
        users_schema,
        TableStatistics::with_row_count(10),
    )
    .map_err(|error| error.to_string())?;
    let database = DatabaseEntry::new(DATABASE_ID, "app", [events, users])
        .map_err(|error| error.to_string())?;
    CatalogSnapshot::new([database]).map_err(|error| error.to_string())
}

fn events_schema() -> Result<TableSchema, String> {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "score", DataType::Int64, false),
            Column::new(4, "active", DataType::Boolean, false),
        ],
    )
    .map_err(|error| error.to_string())
}

fn users_schema() -> Result<TableSchema, String> {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .map_err(|error| error.to_string())
}

fn event_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("non-empty event key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("event-{id:02}")),
            Value::Int64(i64::try_from(id * 10).expect("small seed score")),
            Value::Boolean(id % 2 == 0),
        ],
        id,
        false,
    )
}

fn user_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("non-empty user key"),
        vec![Value::UInt64(id), Value::Utf8(format!("user-{id:02}"))],
        id,
        false,
    )
}
