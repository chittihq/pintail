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
const EXPECTED_CASES: usize = 768;

struct OracleCase {
    family: &'static str,
    sql: String,
    ordered: bool,
}

#[derive(Debug)]
enum OracleValue {
    Exact(String),
    Float(String),
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
                "--tmpfs",
                "/var/lib/mysql:rw,size=2g",
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
                    "--default-character-set=utf8mb4",
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
                // Named time zones (CONVERT_TZ) need the mysql.time_zone
                // tables, which the stock image leaves empty.
                checked_output(
                    Command::new("docker").args([
                        "exec",
                        &container.name,
                        "sh",
                        "-c",
                        "mysql_tzinfo_to_sql /usr/share/zoneinfo 2>/dev/null \
                         | mysql --user=root mysql",
                    ]),
                    "load MySQL time zone tables",
                )?;
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

#[test]
fn oracle_applies_tolerance_only_to_float_results() {
    assert!(!oracle_values_equal(
        &OracleValue::Exact("9007199254740993".to_owned()),
        "9007199254740992",
    ));
    assert!(!oracle_values_equal(
        &OracleValue::Exact("01".to_owned()),
        "1",
    ));
    assert!(oracle_values_equal(
        &OracleValue::Float("0.30000000000000004".to_owned()),
        "0.3",
    ));
}

#[allow(clippy::too_many_lines)]
fn run_oracle() -> Result<(), String> {
    let mysql = MysqlContainer::start()?;
    mysql.query_batch(
        "CREATE TABLE events (\
           id BIGINT UNSIGNED PRIMARY KEY,\
           name VARCHAR(32) NOT NULL,\
           score BIGINT NOT NULL,\
           active BOOLEAN NOT NULL,\
           note VARCHAR(32) NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         CREATE TABLE users (\
           id BIGINT PRIMARY KEY,\
           name VARCHAR(32) NOT NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         INSERT INTO events VALUES\
           (1,'event-01',10,0,'Alpha'),(2,'event-02',20,1,'alpha'),\
           (3,'event-03',30,0,NULL),(4,'event-04',40,1,'Beta'),\
           (5,'event-05',50,0,'beta'),(6,'event-06',60,1,NULL),\
           (7,'event-07',70,0,'Alpha'),(8,'event-08',80,1,'alpha'),\
           (9,'event-09',90,0,NULL),(10,'event-10',100,1,'Beta');\
         INSERT INTO users VALUES\
           (1,'user-01'),(2,'user-02'),(3,'user-03'),(4,'user-04'),\
           (5,'user-05'),(6,'user-06'),(7,'user-07'),(8,'user-08');",
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
        .ingest((1..=8).map(user_row).collect())
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
        if !oracle_rows_equal(&actual, expected, case.ordered) {
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
        println!("all {EXPECTED_CASES} generated and hand-written queries matched MySQL 8.4");
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
) -> Result<Vec<Vec<OracleValue>>, String> {
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
            rows.push(values);
        }
    }
    Ok(rows)
}

fn oracle_rows_equal(actual: &[Vec<OracleValue>], expected: &[String], ordered: bool) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    if ordered {
        return actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| oracle_row_equal(actual, expected));
    }
    let mut matched = vec![false; expected.len()];
    actual.iter().all(|actual| {
        let Some(index) = expected
            .iter()
            .enumerate()
            .find(|(index, expected)| !matched[*index] && oracle_row_equal(actual, expected))
            .map(|(index, _)| index)
        else {
            return false;
        };
        matched[index] = true;
        true
    })
}

fn oracle_row_equal(actual: &[OracleValue], expected: &str) -> bool {
    let expected = expected.split('\t').collect::<Vec<_>>();
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| oracle_values_equal(actual, expected))
}

fn oracle_values_equal(actual: &OracleValue, expected: &str) -> bool {
    match actual {
        OracleValue::Exact(actual) => actual == expected,
        OracleValue::Float(actual) if actual == expected => true,
        OracleValue::Float(actual) => {
            let (Ok(actual), Ok(expected)) = (actual.parse::<f64>(), expected.parse::<f64>())
            else {
                return false;
            };
            let scale = actual.abs().max(expected.abs()).max(1.0);
            (actual - expected).abs() <= f64::EPSILON * 16.0 * scale
        }
    }
}

fn canonical_value(value: &Value) -> OracleValue {
    match value {
        Value::Null => OracleValue::Exact("NULL".to_owned()),
        Value::Boolean(value) => OracleValue::Exact(u8::from(*value).to_string()),
        Value::Int64(value) => OracleValue::Exact(value.to_string()),
        Value::UInt64(value) => OracleValue::Exact(value.to_string()),
        Value::Float64(value) => OracleValue::Float(value.get().to_string()),
        Value::Utf8(value) => OracleValue::Exact(value.clone()),
        Value::Binary(value) => OracleValue::Exact(String::from_utf8_lossy(value).into_owned()),
    }
}

#[allow(clippy::too_many_lines)]
fn oracle_cases() -> Vec<OracleCase> {
    let mut cases = Vec::with_capacity(EXPECTED_CASES);
    for value in 0..90 {
        cases.push(OracleCase {
            family: "arithmetic and predicates",
            sql: format!(
                "SELECT {value} + 7, {value} * 3, {value} % 7, \
                 {value} BETWEEN 10 AND 80, {value} IN (3, 17, 42, 88)"
            ),
            ordered: true,
        });
    }
    for value in 0..90 {
        cases.push(OracleCase {
            family: "strings",
            sql: format!(
                "SELECT CONCAT(LOWER('MiXeD'), '-', {value}), \
                 SUBSTRING('abcdef', 2, 3), TRIM('  pintail  '), \
                 REPLACE('a-b-c', '-', '_'), LEFT('abcdef', 3), \
                 RIGHT('abcdef', 2), LOCATE('tail', 'pintail'), \
                 CONVERT({value}, CHAR), CONVERT('MiXeD' USING utf8mb4)"
            ),
            ordered: true,
        });
    }
    for value in 0..90 {
        cases.push(OracleCase {
            family: "conditionals and nulls",
            sql: format!(
                "SELECT IF({value} % 2 = 0, 'even', 'odd'), \
                 CASE WHEN {value} < 25 THEN 'low' \
                      WHEN {value} < 75 THEN 'mid' ELSE 'high' END, \
                 IFNULL(NULL, {value}), COALESCE(NULL, NULL, {value} + 1), \
                 NULLIF({value} % 5, 0)"
            ),
            ordered: true,
        });
    }
    for value in 0..90 {
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
            ordered: true,
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
            ordered: true,
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
            ordered: true,
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
            ordered: true,
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
            ordered: true,
        });
    }
    for value in 0..25 {
        let threshold = value % 8 + 1;
        cases.push(OracleCase {
            family: "hash aggregate",
            sql: format!(
                "SELECT active, COUNT(*), SUM(score), AVG(score), \
                 COUNT(DISTINCT score), MIN(name), MAX(name), GROUP_CONCAT(name) \
                 FROM events WHERE id >= {threshold} \
                 GROUP BY active HAVING COUNT(*) >= 1 ORDER BY active"
            ),
            ordered: true,
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
            ordered: true,
        });
    }
    for value in 0..22 {
        let limit = value % 3 + 1;
        cases.push(OracleCase {
            family: "union all",
            sql: format!(
                "SELECT {value} AS value UNION ALL SELECT {} UNION ALL SELECT {} \
                 ORDER BY value LIMIT {limit}",
                value + 2,
                value + 1
            ),
            ordered: true,
        });
    }
    cases.extend(hand_written_cases());
    cases
}

#[allow(clippy::too_many_lines)]
fn hand_written_cases() -> Vec<OracleCase> {
    let ordered = |family, sql: &str| OracleCase {
        family,
        sql: sql.to_owned(),
        ordered: true,
    };
    let unordered = |family, sql: &str| OracleCase {
        family,
        sql: sql.to_owned(),
        ordered: false,
    };
    vec![
        unordered("hand-written distinct", "SELECT DISTINCT note FROM events"),
        unordered(
            "hand-written distinct",
            "SELECT DISTINCT active, note FROM events",
        ),
        ordered(
            "hand-written left join",
            "SELECT e.id, u.name FROM events e LEFT JOIN users u ON e.id = u.id \
             WHERE e.id >= 7 ORDER BY e.id",
        ),
        ordered(
            "hand-written left join",
            "SELECT e.id FROM events e LEFT JOIN users u ON e.id = u.id \
             WHERE u.id IS NULL ORDER BY e.id",
        ),
        ordered(
            "hand-written cross join",
            "SELECT e.id AS event_id, u.id AS user_id FROM events e CROSS JOIN users u \
             WHERE e.id >= 9 AND u.id <= 2 ORDER BY event_id, user_id",
        ),
        ordered(
            "hand-written using join",
            "SELECT id, e.name, u.name FROM events e JOIN users u USING (id) \
             WHERE id >= 3 ORDER BY id",
        ),
        ordered(
            "hand-written using join",
            "SELECT * FROM events e JOIN users u USING (id) ORDER BY id",
        ),
        ordered(
            "hand-written using join",
            "SELECT * FROM events e LEFT JOIN users u USING (id) ORDER BY id",
        ),
        ordered(
            "hand-written using join",
            "SELECT id, u.id FROM events e LEFT JOIN users u USING (id) \
             ORDER BY e.id",
        ),
        ordered(
            "hand-written using join",
            "SELECT * FROM events RIGHT JOIN users USING (id) ORDER BY id",
        ),
        ordered(
            "hand-written using join",
            "SELECT * FROM events e JOIN users u USING (id, name) ORDER BY id",
        ),
        ordered(
            "hand-written natural join",
            "SELECT * FROM events NATURAL JOIN users ORDER BY id",
        ),
        ordered(
            "hand-written natural join",
            "SELECT id, name FROM events NATURAL LEFT JOIN users ORDER BY id",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT e.id, (SELECT COUNT(*) FROM users u WHERE u.id = e.id) AS n \
             FROM events e ORDER BY e.id",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT e.id, (SELECT COUNT(*) FROM users u WHERE u.id = e.id AND u.id >= 3) AS n \
             FROM events e ORDER BY e.id",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT e.id, (SELECT SUM(u.id) FROM users u WHERE u.id = e.id) AS total \
             FROM events e ORDER BY e.id",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT e.id, (SELECT MIN(u.name) FROM users u WHERE u.id = e.id) AS lo \
             FROM events e WHERE e.active = TRUE ORDER BY e.id",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT e.id, (SELECT AVG(u.id) FROM users u WHERE u.id = e.id) AS mean \
             FROM events e ORDER BY e.id",
        ),
        ordered(
            "hand-written convert_tz",
            "SELECT CONVERT_TZ('2026-03-08 06:30:00','+00:00','+05:30'), \
             CONVERT_TZ('2026-03-08 06:30:00','+05:30','-08:00'), \
             CONVERT_TZ('2026-06-15 10:00:00.250','+00:00','+02:00')",
        ),
        ordered(
            "hand-written convert_tz",
            "SELECT CONVERT_TZ('2026-01-15 12:00:00','UTC','Asia/Kolkata'), \
             CONVERT_TZ('2026-01-15 12:00:00','Asia/Kolkata','UTC'), \
             CONVERT_TZ('2026-07-04 18:00:00','America/New_York','Europe/Paris')",
        ),
        ordered(
            "hand-written convert_tz",
            "SELECT CONVERT_TZ('2026-11-01 05:30:00','UTC','America/New_York'), \
             CONVERT_TZ('2026-11-01 01:30:00','America/New_York','UTC')",
        ),
        ordered(
            "hand-written convert_tz",
            "SELECT CONVERT_TZ('2026-06-15 10:00:00','Bad/Zone','UTC'), \
             CONVERT_TZ(NULL,'+00:00','+01:00'), \
             CONVERT_TZ('not a datetime','+00:00','+01:00')",
        ),
        ordered(
            "hand-written char and rand",
            "SELECT CHAR(77,121,83,81,76), CHAR(256), CHAR(77,NULL,121), \
             CHAR(id + 64) FROM events WHERE id <= 3 ORDER BY id",
        ),
        ordered(
            "hand-written char and rand",
            "SELECT RAND() >= 0 AND RAND() < 1, RAND() <> RAND() OR RAND() <> RAND()",
        ),
        unordered(
            "hand-written cross join",
            "SELECT e.name, u.name FROM events e, users u \
             WHERE e.id = 10 AND u.id >= 7",
        ),
        ordered(
            "hand-written null predicates",
            "SELECT id FROM events WHERE note IS NULL OR active = TRUE ORDER BY id",
        ),
        ordered(
            "hand-written null predicates",
            "SELECT id FROM events WHERE NOT (note = 'alpha') ORDER BY id",
        ),
        ordered(
            "hand-written null predicates",
            "SELECT id FROM events WHERE note = 'alpha' OR note IS NULL ORDER BY id",
        ),
        ordered(
            "hand-written null predicates",
            "SELECT id FROM events WHERE note <> 'alpha' AND note IS NOT NULL ORDER BY id",
        ),
        ordered(
            "hand-written logic",
            "SELECT TRUE OR NULL, FALSE AND NULL, NOT NULL, NULL XOR TRUE",
        ),
        ordered(
            "hand-written logic",
            "SELECT id, (id = 1 AND active) OR id = 3 FROM events \
             WHERE id <= 3 ORDER BY id",
        ),
        ordered(
            "hand-written strings",
            "SELECT UPPER('Pintáil'), LOWER('MiXeD'), LENGTH('Pintáil'), \
             CHAR_LENGTH('Pintáil')",
        ),
        ordered(
            "hand-written strings",
            "SELECT 'Pintail' LIKE 'pin%', 'Pintail' NOT LIKE '%sql', \
             'a_b' LIKE 'a_b'",
        ),
        ordered(
            "hand-written date and time",
            "SELECT HOUR('2024-02-29 12:34:56'), \
             MINUTE('2024-02-29 12:34:56'), SECOND('2024-02-29 12:34:56')",
        ),
        ordered(
            "hand-written date and time",
            "SELECT UNIX_TIMESTAMP(FROM_UNIXTIME(1704067200)), \
             FROM_UNIXTIME(UNIX_TIMESTAMP('2024-01-01 12:34:56'))",
        ),
        ordered(
            "hand-written date and time",
            "SELECT DATE(NOW()) = CURDATE(), LENGTH(NOW()), CHAR_LENGTH(CURDATE())",
        ),
        ordered(
            "hand-written date and time",
            "SELECT DATE_FORMAT('2024-02-29 12:34:56', '%c/%e/%Y %k:%i:%s'), \
             DATE('2024-02-29 12:34:56')",
        ),
        // Every DATE_FORMAT directive MySQL defines, adjudicated by MySQL
        // itself. The previous coverage used only %c %e %Y %k %i %s — exactly
        // the directives that were either mapped or coincided with chrono's
        // dialect — which is why %W returning a week number and %v returning
        // a formatted date went unnoticed. Two dates: a leap day mid-year and
        // a January 1st that belongs to the previous week-year.
        ordered(
            "hand-written date format directives",
            "SELECT DATE_FORMAT('2024-02-29 12:34:56', \
             '%a|%b|%c|%D|%d|%e|%H|%h|%I|%i|%j|%k|%l|%M|%m|%p|%r|%S|%s|%T|%W|%w|%Y|%y')",
        ),
        ordered(
            "hand-written date format week numbering",
            "SELECT DATE_FORMAT('2024-02-29 12:34:56', '%U|%u|%V|%v|%X|%x'), \
             DATE_FORMAT('2021-01-01 00:00:00', '%U|%u|%V|%v|%X|%x'), \
             DATE_FORMAT('2023-01-01 00:00:00', '%U|%u|%V|%v|%X|%x'), \
             DATE_FORMAT('2019-12-30 00:00:00', '%U|%u|%V|%v|%X|%x')",
        ),
        ordered(
            "hand-written date format edges",
            "SELECT DATE_FORMAT('2024-01-01 00:05:00', '%h %l %p %k %H'), \
             DATE_FORMAT('2024-01-11 12:00:00', '%D %p'), \
             DATE_FORMAT('2024-01-21 23:59:59', '%D %r'), \
             DATE_FORMAT('2024-02-29 12:34:56', '%q'), \
             DATE_FORMAT('2024-02-29 12:34:56', '100%%'), \
             DATE_FORMAT('2024-02-29 12:34:56', 'no directives')",
        ),
        // The aggregates added for BI parity. MySQL adjudicates the edge
        // semantics that are easy to assume wrongly: BIT_AND over an empty
        // group is the fold identity (all ones) rather than NULL, and the
        // sample forms divide by n-1 so a single row is NULL while the
        // population forms are 0.
        ordered(
            "hand-written statistical aggregates",
            "SELECT STDDEV(score), STDDEV_POP(score), STDDEV_SAMP(score), \
             VARIANCE(score), VAR_POP(score), VAR_SAMP(score), STD(score) FROM events",
        ),
        ordered(
            "hand-written statistical aggregates grouped",
            "SELECT active, STDDEV_POP(score), VAR_SAMP(score) FROM events \
             GROUP BY active ORDER BY active",
        ),
        ordered(
            "hand-written statistical aggregate single row",
            "SELECT STDDEV_POP(score), STDDEV_SAMP(score), VAR_POP(score), VAR_SAMP(score) \
             FROM events WHERE id = 1",
        ),
        ordered(
            "hand-written statistical aggregate empty group",
            "SELECT STDDEV_POP(score), STDDEV_SAMP(score), VARIANCE(score) \
             FROM events WHERE id > 1000",
        ),
        ordered(
            "hand-written bit aggregates",
            "SELECT BIT_AND(score), BIT_OR(score), BIT_XOR(score) FROM events",
        ),
        ordered(
            "hand-written bit aggregates grouped",
            "SELECT active, BIT_AND(id), BIT_OR(id), BIT_XOR(id) FROM events \
             GROUP BY active ORDER BY active",
        ),
        ordered(
            "hand-written bit aggregates empty group",
            "SELECT BIT_AND(score), BIT_OR(score), BIT_XOR(score) FROM events WHERE id > 1000",
        ),
        // ANY_VALUE is nondeterministic in MySQL, so it can only be compared
        // where the column is functionally dependent on the grouping key —
        // which is also the only shape clients emit it in.
        ordered(
            "hand-written any_value",
            "SELECT id, ANY_VALUE(name), ANY_VALUE(score) FROM events GROUP BY id ORDER BY id",
        ),
        // ANY_VALUE is not an aggregate: with no GROUP BY and no other
        // aggregate, the query is not aggregated, so an empty filter returns
        // no rows rather than one NULL row, and an unfiltered scan returns
        // every row rather than one.
        ordered(
            "hand-written any_value empty group",
            "SELECT ANY_VALUE(name) FROM events WHERE id > 1000",
        ),
        ordered(
            "hand-written any_value ungrouped",
            "SELECT ANY_VALUE(name) FROM events ORDER BY 1",
        ),
        ordered(
            "hand-written any_value ungrouped expression",
            "SELECT ANY_VALUE(score) + 1 FROM events WHERE id <= 3 ORDER BY 1",
        ),
        // CAST to a temporal target must convert, not relabel: DATE has to
        // drop the time, and an uninterpretable value is NULL rather than an
        // error. Before this, DATETIME reached neither the CHAR nor the INT
        // branch of the target table and rejected outright.
        ordered(
            "hand-written cast temporal",
            "SELECT CAST('2024-02-29 12:34:56' AS DATE), \
             CAST('2024-02-29' AS DATETIME), \
             CAST('2024-02-29 12:34:56' AS DATETIME), \
             CAST('2024-02-29 12:34:56.789' AS DATETIME(3))",
        ),
        ordered(
            "hand-written cast temporal invalid",
            "SELECT CAST('not-a-date' AS DATE), CAST('' AS DATE), \
             CAST('2024-13-45' AS DATE)",
        ),
        ordered(
            "hand-written cast targets",
            "SELECT CAST(42 AS CHAR), CAST('42' AS SIGNED), CAST('42' AS UNSIGNED), \
             CAST(-1 AS SIGNED), CAST('3.7' AS DECIMAL(10,2)), CAST(1 AS CHAR(4))",
        ),
        // JSON containment is asymmetric and recursive, which is the part
        // most likely to be subtly wrong: an array contains a bare scalar
        // when any element does, an object contains an object when every
        // candidate key matches, and scalars must be equal.
        ordered(
            "hand-written json contains",
            "SELECT JSON_CONTAINS('[1,2,3]', '2'), JSON_CONTAINS('[1,2,3]', '[1,3]'), \
             JSON_CONTAINS('[1,2,3]', '[1,4]'), JSON_CONTAINS('{\"a\":1,\"b\":2}', '{\"a\":1}'), \
             JSON_CONTAINS('{\"a\":1}', '{\"a\":2}'), JSON_CONTAINS('1', '1'), \
             JSON_CONTAINS('[[1,2]]', '[[1]]')",
        ),
        ordered(
            "hand-written json contains path",
            "SELECT JSON_CONTAINS_PATH('{\"a\":1,\"b\":2}', 'one', '$.a', '$.z'), \
             JSON_CONTAINS_PATH('{\"a\":1,\"b\":2}', 'all', '$.a', '$.z'), \
             JSON_CONTAINS_PATH('{\"a\":1,\"b\":2}', 'all', '$.a', '$.b')",
        ),
        ordered(
            "hand-written json length keys type",
            "SELECT JSON_LENGTH('[1,2,3]'), JSON_LENGTH('{\"a\":1,\"b\":2}'), \
             JSON_LENGTH('7'), JSON_LENGTH('{\"a\":[1,2,3]}', '$.a'), \
             JSON_KEYS('{\"b\":1,\"a\":2}'), JSON_KEYS('[1,2]'), \
             JSON_TYPE('[1]'), JSON_TYPE('{}'), JSON_TYPE('\"x\"'), \
             JSON_TYPE('1'), JSON_TYPE('1.5'), JSON_TYPE('true'), JSON_TYPE('null')",
        ),
        ordered(
            "hand-written json valid",
            "SELECT JSON_VALID('{\"a\":1}'), JSON_VALID('not json'), JSON_VALID('[1,2]'), \
             JSON_VALID('')",
        ),
        // SUBSTRING_INDEX counts from the right for a negative count and
        // returns the whole subject when the delimiter appears fewer times
        // than asked, which is what makes it usable for URL splitting.
        ordered(
            "hand-written substring_index",
            "SELECT SUBSTRING_INDEX('a/b/c', '/', 1), SUBSTRING_INDEX('a/b/c', '/', 2), \
             SUBSTRING_INDEX('a/b/c', '/', -1), SUBSTRING_INDEX('a/b/c', '/', -2), \
             SUBSTRING_INDEX('a/b/c', '/', 9), SUBSTRING_INDEX('a/b/c', '/', -9), \
             SUBSTRING_INDEX('a/b/c', '/', 0), SUBSTRING_INDEX('abc', '/', 1), \
             SUBSTRING_INDEX('a::b::c', '::', 2), SUBSTRING_INDEX('', '/', 1)",
        ),
        ordered(
            "hand-written conv",
            "SELECT CONV('a', 16, 2), CONV('6E', 18, 8), CONV(-17, 10, -18), \
             CONV('ff', 16, 10), CONV('1111', 2, 16), CONV('zz', 36, 10), \
             CONV('0', 10, 10), CONV('7fffffffffffffff', 16, 10)",
        ),
        ordered(
            "hand-written maketime",
            "SELECT MAKETIME(12, 15, 30), MAKETIME(0, 0, 0), MAKETIME(-1, 30, 0), \
             MAKETIME(1, 60, 0), MAKETIME(1, 0, 60)",
        ),
        // The offset window functions. LAST_VALUE is the case worth pinning:
        // under MySQL's default frame it reads the last row of the CURRENT
        // PEER GROUP, not of the partition, so with a unique ORDER BY key it
        // returns the current row's own value. Reading it as "last row of the
        // partition" is the common mistake and these cases would catch it.
        ordered(
            "hand-written lag lead",
            "SELECT id, LAG(score) OVER (ORDER BY id), LEAD(score) OVER (ORDER BY id) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written lag lead offset default",
            "SELECT id, LAG(score, 2, -1) OVER (ORDER BY id), \
             LEAD(score, 3, -1) OVER (ORDER BY id), \
             LAG(score, 0) OVER (ORDER BY id) FROM events ORDER BY id",
        ),
        ordered(
            "hand-written lag lead partitioned",
            "SELECT id, active, LAG(score) OVER (PARTITION BY active ORDER BY id), \
             LEAD(score) OVER (PARTITION BY active ORDER BY id) FROM events ORDER BY id",
        ),
        ordered(
            "hand-written ntile",
            "SELECT id, NTILE(3) OVER (ORDER BY id), NTILE(4) OVER (ORDER BY id), \
             NTILE(1) OVER (ORDER BY id), NTILE(20) OVER (ORDER BY id) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written first last value unique order",
            "SELECT id, FIRST_VALUE(score) OVER (ORDER BY id), \
             LAST_VALUE(score) OVER (ORDER BY id) FROM events ORDER BY id",
        ),
        ordered(
            "hand-written first last value peer groups",
            "SELECT id, active, FIRST_VALUE(score) OVER (ORDER BY active), \
             LAST_VALUE(score) OVER (ORDER BY active) FROM events ORDER BY id",
        ),
        ordered(
            "hand-written first last value no order",
            "SELECT id, FIRST_VALUE(score) OVER (PARTITION BY active), \
             LAST_VALUE(score) OVER (PARTITION BY active) FROM events ORDER BY id",
        ),
        // Explicit ROWS frames: running totals and moving windows.
        ordered(
            "hand-written window rows frames",
            "SELECT id, \
             SUM(score) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
             SUM(score) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW), \
             SUM(score) OVER (ORDER BY id ROWS 2 PRECEDING), \
             SUM(score) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written window rows frames unbounded",
            "SELECT id, \
             MAX(score) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
             COUNT(score) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
             SUM(score) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) \
             FROM events ORDER BY id",
        ),
        // The four divergences repaired this batch.
        ordered(
            "repaired trim whitespace class",
            "SELECT HEX(TRIM(CHAR(9))), HEX(TRIM(' a ')), HEX(TRIM(CONCAT(CHAR(9), 'a')))",
        ),
        // The wrapped value carries a newline at column 76, which the
        // comparison harness cannot represent — it parses MySQL's output one
        // line per row, so a value containing a newline splits into two. The
        // length and the substituted form pin the same behaviour without
        // tripping over that.
        ordered(
            "repaired base64 wrapping",
            "SELECT LENGTH(TO_BASE64(REPEAT('a', 58))), \
             REPLACE(TO_BASE64(REPEAT('a', 58)), CHAR(10), '|'), \
             LENGTH(TO_BASE64(REPEAT('a', 10))), TO_BASE64(REPEAT('a', 10))",
        ),
        ordered(
            "repaired sec_to_time fraction",
            "SELECT SEC_TO_TIME(1.5), SEC_TO_TIME(1), SEC_TO_TIME(90)",
        ),
        ordered(
            "repaired regexp unicode classes",
            "SELECT REGEXP_LIKE('\u{e9}', '[[:alpha:]]'), REGEXP_LIKE('a', '[[:alpha:]]'), \
             REGEXP_LIKE('1', '[[:alpha:]]'), REGEXP_LIKE('1', '[[:digit:]]')",
        ),
        ordered(
            "hand-written conditionals",
            "SELECT IF(NULL, 'yes', 'no'), COALESCE(NULL, 'first', 'second'), \
             NULLIF('Alpha', 'alpha')",
        ),
        ordered(
            "hand-written derived table",
            "SELECT d.active, COUNT(*) FROM \
             (SELECT id, active FROM events WHERE id >= 5) d \
             GROUP BY d.active ORDER BY d.active",
        ),
        ordered(
            "hand-written common table expression",
            "WITH missing AS (SELECT e.id FROM events e LEFT JOIN users u ON e.id = u.id \
             WHERE u.id IS NULL) SELECT id FROM missing ORDER BY id",
        ),
        ordered(
            "hand-written distinct aggregate",
            "SELECT COUNT(note), COUNT(DISTINCT note), GROUP_CONCAT(note) FROM events",
        ),
        ordered(
            "hand-written null membership",
            "SELECT id FROM events WHERE note NOT IN ('missing', NULL) ORDER BY id",
        ),
        ordered(
            "hand-written null membership",
            "SELECT id FROM events WHERE note IN ('alpha', NULL) ORDER BY id",
        ),
        ordered(
            "hand-written null membership",
            "SELECT 1 IN (1, NULL), 2 IN (1, NULL), 2 NOT IN (1, NULL), \
             NULL IN (1, 2)",
        ),
        ordered(
            "hand-written grouping",
            "SELECT note, COUNT(*), MIN(name), MAX(name) FROM events \
             GROUP BY note ORDER BY note",
        ),
        ordered(
            "hand-written collation",
            "SELECT MIN(note), MAX(note) FROM events",
        ),
        ordered(
            "hand-written left join aggregate",
            "SELECT u.id IS NULL, COUNT(*) FROM events e LEFT JOIN users u ON e.id = u.id \
             GROUP BY u.id IS NULL ORDER BY u.id IS NULL",
        ),
        ordered(
            "hand-written relational subquery",
            "SELECT id FROM events WHERE id NOT IN (SELECT id FROM users) ORDER BY id",
        ),
        ordered(
            "hand-written relational subquery",
            "SELECT (SELECT MAX(id) FROM users), \
             10 IN (SELECT id FROM users), 8 IN (SELECT id FROM users)",
        ),
        unordered(
            "hand-written union all",
            "SELECT 3 AS value UNION ALL SELECT 1 UNION ALL SELECT 2",
        ),
        ordered(
            "hand-written union all",
            "SELECT note FROM events WHERE id <= 2 UNION ALL \
             SELECT note FROM events WHERE id >= 9 ORDER BY note",
        ),
        unordered(
            "hand-written left join distinct",
            "SELECT DISTINCT u.name FROM events e LEFT JOIN users u ON e.id = u.id",
        ),
        ordered(
            "hand-written limit offset",
            "SELECT id, name FROM events ORDER BY id LIMIT 3 OFFSET 4",
        ),
        ordered(
            "hand-written reversed comparison",
            "SELECT id FROM events WHERE 8 <= id AND 10 > id ORDER BY id",
        ),
        ordered(
            "hand-written between",
            "SELECT id, id NOT BETWEEN 3 AND 7 FROM events \
             WHERE id IN (1, 3, 7, 10) ORDER BY id",
        ),
        ordered(
            "hand-written arithmetic",
            "SELECT -7 DIV 3, -7 % 3, 7 / 2, 7 DIV 2",
        ),
        ordered(
            "hand-written aggregate empty input",
            "SELECT COUNT(*), SUM(score), AVG(score), MIN(note), MAX(note) \
             FROM events WHERE id > 100",
        ),
        ordered(
            "hand-written scalar subquery",
            "SELECT id, (SELECT name FROM users WHERE id = 8) \
             FROM events WHERE id IN (1, 8) ORDER BY id",
        ),
        ordered(
            "hand-written case expression",
            "SELECT id, CASE WHEN note IS NULL THEN 'none' \
             WHEN note = 'alpha' THEN 'a' ELSE 'b' END \
             FROM events WHERE id >= 7 ORDER BY id",
        ),
        ordered(
            "hand-written exact mixed integers",
            "SELECT CAST(9007199254740993 AS UNSIGNED) > \
                    CAST(9007199254740992 AS SIGNED), \
                    CAST(-1 AS SIGNED) < CAST(0 AS UNSIGNED)",
        ),
        ordered(
            "hand-written mixed scalar join",
            "SELECT e.id AS event_id, u.id AS user_id \
             FROM events e INNER JOIN users u \
             ON CAST(e.id AS DOUBLE) = CAST(u.id AS CHAR) \
             ORDER BY event_id, user_id",
        ),
        ordered(
            "hand-written large mixed scalar join",
            "SELECT l.id, r.id FROM \
             (SELECT CAST(9007199254740993 AS UNSIGNED) AS id) l \
             INNER JOIN \
             (SELECT CAST(9007199254740993 AS CHAR) AS id) r \
             ON l.id = r.id",
        ),
        // Window functions (issue #4): rankings, aggregate windows with
        // MySQL default frames, windows nested in expressions, and windows
        // over grouped output — all differentially checked against MySQL.
        ordered(
            "window ranking",
            "SELECT id, ROW_NUMBER() OVER (ORDER BY score DESC) AS rn FROM events ORDER BY id",
        ),
        ordered(
            "window ranking",
            "SELECT id, RANK() OVER (ORDER BY note) AS r, DENSE_RANK() OVER (ORDER BY note) AS d \
             FROM events ORDER BY id",
        ),
        ordered(
            "window ranking",
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY active ORDER BY score DESC) AS rn \
             FROM events ORDER BY id",
        ),
        ordered(
            "window ranking",
            "SELECT id, RANK() OVER (PARTITION BY note ORDER BY id) AS r FROM events ORDER BY id",
        ),
        ordered(
            "window aggregate",
            "SELECT id, SUM(score) OVER (PARTITION BY active) AS t FROM events ORDER BY id",
        ),
        ordered(
            "window aggregate",
            "SELECT id, COUNT(*) OVER (PARTITION BY note) AS n FROM events ORDER BY id",
        ),
        ordered(
            "window aggregate",
            "SELECT id, AVG(score) OVER (PARTITION BY active) AS a FROM events ORDER BY id",
        ),
        ordered(
            "window aggregate",
            "SELECT id, MIN(score) OVER (PARTITION BY note) AS lo, \
             MAX(score) OVER (PARTITION BY note) AS hi FROM events ORDER BY id",
        ),
        ordered(
            "window running frame",
            "SELECT id, SUM(score) OVER (ORDER BY id) AS running FROM events ORDER BY id",
        ),
        ordered(
            "window running frame",
            "SELECT id, SUM(score) OVER (ORDER BY note) AS peers FROM events ORDER BY id",
        ),
        ordered(
            "window running frame",
            "SELECT id, COUNT(*) OVER (PARTITION BY active ORDER BY id) AS c \
             FROM events ORDER BY id",
        ),
        ordered(
            "window nested in expression",
            "SELECT id, ROUND(score * 100 / SUM(score) OVER (PARTITION BY active), 2) AS share \
             FROM events ORDER BY id",
        ),
        ordered(
            "window nested in expression",
            "SELECT id, CASE WHEN ROW_NUMBER() OVER (ORDER BY score DESC) <= 3 \
             THEN 'top' ELSE 'rest' END AS bucket FROM events ORDER BY id",
        ),
        ordered(
            "window nested in expression",
            "SELECT id, score - AVG(score) OVER (PARTITION BY active) AS deviation \
             FROM events ORDER BY id",
        ),
        ordered(
            "window over grouped output",
            "SELECT note, SUM(score) AS total, \
             SUM(SUM(score)) OVER () AS grand, \
             ROW_NUMBER() OVER (ORDER BY SUM(score) DESC, note) AS heaviest \
             FROM events GROUP BY note ORDER BY heaviest",
        ),
        ordered(
            "window over grouped output",
            "SELECT active, COUNT(*) AS n, \
             COUNT(*) * 100 / SUM(COUNT(*)) OVER () AS pct \
             FROM events GROUP BY active ORDER BY active",
        ),
        ordered(
            "window over grouped output",
            "SELECT note, MAX(score) AS best, \
             RANK() OVER (ORDER BY MAX(score) DESC) AS r \
             FROM events GROUP BY note HAVING COUNT(*) >= 1 ORDER BY r, note",
        ),
        ordered(
            "window in cte",
            "WITH seq AS (\
               SELECT id, active, score, \
               ROW_NUMBER() OVER (PARTITION BY active ORDER BY id) AS pos, \
               MIN(score) OVER (PARTITION BY active) AS floor_score \
             FROM events) \
             SELECT active, COUNT(*) AS n, SUM(CASE WHEN pos > 1 THEN 1 ELSE 0 END) AS repeats, \
             MIN(floor_score) AS lo FROM seq GROUP BY active ORDER BY active",
        ),
        ordered(
            "window in cte",
            "WITH ranked AS (\
               SELECT id, note, score, \
               ROW_NUMBER() OVER (PARTITION BY note ORDER BY score DESC) AS rn, \
               COUNT(*) OVER (PARTITION BY note) AS n \
             FROM events) \
             SELECT id, note, rn, n FROM ranked WHERE rn <= 2 ORDER BY id",
        ),
        ordered(
            "window multiple in one query",
            "SELECT id, \
             ROW_NUMBER() OVER (ORDER BY score) AS by_score, \
             ROW_NUMBER() OVER (ORDER BY id DESC) AS by_id, \
             SUM(score) OVER (PARTITION BY active) AS group_total \
             FROM events ORDER BY id",
        ),
        ordered(
            "decimal division",
            "SELECT id, score / 7, score / 3, id / score FROM events ORDER BY id",
        ),
        ordered(
            "decimal division",
            "SELECT active, SUM(CASE WHEN score > 40 THEN 1 ELSE 0 END) / COUNT(*) \
             FROM events GROUP BY active ORDER BY active",
        ),
        ordered(
            "decimal division",
            "SELECT id, score * 100 / SUM(score) OVER () FROM events ORDER BY id",
        ),
        ordered(
            "decimal division",
            // Chained division (1/3/3) is documented-imprecise: MySQL keeps
            // unrounded internal digits between the two divisions
            // (docs/limitations.md); single divisions are exact.
            "SELECT score / 0, 100 / 7, 10 / 4 FROM events WHERE id = 1",
        ),
        ordered(
            "json",
            r#"SELECT JSON_EXTRACT('{"a": {"b": [10, 20]}, "c": "x"}', '$.a.b[1]'), JSON_EXTRACT('{"c": "x"}', '$.c'), JSON_EXTRACT('{"c": "x"}', '$.missing'), JSON_UNQUOTE(JSON_EXTRACT('{"c": "x"}', '$.c'))"#,
        ),
        ordered(
            "json",
            r#"SELECT JSON_EXTRACT('[1, 2, 3]', '$[2]'), JSON_UNQUOTE(JSON_EXTRACT('{"b": "two"}', '$.b')), JSON_EXTRACT('{"a": {"deep key": true}}', '$.a."deep key"')"#,
        ),
        ordered(
            "json",
            "SELECT id, JSON_OBJECT('name', name, 'score', score, 'note', note) \
             FROM events ORDER BY id",
        ),
        ordered(
            "json",
            "SELECT id, JSON_ARRAY(id, name, active, note, score + 5) \
             FROM events ORDER BY id",
        ),
        ordered(
            "json",
            "SELECT JSON_OBJECT('bb', 1, 'a', 2, 'a', 3), JSON_OBJECT(), JSON_ARRAY(), \
             JSON_ARRAY(NULL, 'x')",
        ),
        ordered(
            "json aggregate",
            "SELECT id, JSON_ARRAYAGG(score) FROM events GROUP BY id ORDER BY id",
        ),
        unordered(
            "json aggregate",
            "SELECT active, JSON_ARRAYAGG(note) FROM events GROUP BY active",
        ),
        ordered(
            "json aggregate",
            "SELECT JSON_ARRAYAGG(name) FROM events WHERE id > 100",
        ),
        ordered("json aggregate", "SELECT JSON_ARRAYAGG(note) FROM events"),
        ordered(
            "json",
            r#"SELECT JSON_EXTRACT('{"a": {"b": [10, 20], "c": "x"}}', '$.a'), JSON_EXTRACT('[{"k": 1}, {"k": 2}]', '$[1]')"#,
        ),
        ordered(
            "right join",
            "SELECT u.id, u.name, e.name FROM events e RIGHT JOIN users u ON e.id = u.id \
             ORDER BY u.id",
        ),
        ordered(
            "right join",
            "SELECT e.id, u.name FROM users u RIGHT JOIN events e ON e.id = u.id \
             WHERE u.id IS NULL ORDER BY e.id",
        ),
        unordered(
            "set operations",
            "SELECT id FROM events WHERE id <= 6 INTERSECT SELECT id FROM events WHERE id >= 4",
        ),
        unordered(
            "set operations",
            "SELECT id FROM events WHERE id <= 6 EXCEPT SELECT id FROM events WHERE id >= 4",
        ),
        unordered(
            "set operations",
            "SELECT note FROM events INTERSECT SELECT note FROM events WHERE active = 1",
        ),
        unordered(
            "set operations",
            "SELECT note FROM events EXCEPT SELECT note FROM events WHERE active = 1",
        ),
        unordered(
            "set operations",
            "SELECT active FROM events INTERSECT SELECT active FROM events WHERE id <= 3",
        ),
        unordered(
            "set operations",
            "SELECT active FROM events INTERSECT ALL SELECT active FROM events WHERE id <= 3",
        ),
        unordered(
            "set operations",
            "SELECT active FROM events EXCEPT ALL SELECT active FROM events WHERE id <= 3",
        ),
        unordered(
            "set operations",
            "SELECT id % 3 FROM events INTERSECT ALL SELECT id % 4 FROM events WHERE id <= 8",
        ),
        unordered(
            "set operations",
            "SELECT id % 3 FROM events EXCEPT ALL SELECT id % 4 FROM events WHERE id <= 8",
        ),
        unordered(
            "union mixed numeric",
            "SELECT id FROM events UNION SELECT id FROM users",
        ),
        unordered(
            "union mixed numeric",
            "SELECT score - 100 FROM events UNION ALL SELECT id FROM events WHERE id <= 3",
        ),
        unordered(
            "union mixed numeric",
            "SELECT CAST(score AS DECIMAL(7,2)) FROM events WHERE id <= 5 \
             UNION SELECT score FROM events",
        ),
        unordered(
            "union mixed numeric",
            "SELECT id FROM events INTERSECT SELECT id FROM users",
        ),
        ordered(
            "regexp",
            "SELECT id, name REGEXP '^event-0[1-5]$', name NOT REGEXP '0[89]', \
             note REGEXP 'alpha' FROM events ORDER BY id",
        ),
        ordered(
            "regexp",
            "SELECT id, REGEXP_LIKE(name, 'EVENT'), REGEXP_SUBSTR(name, '[0-9]+'), \
             REGEXP_INSTR(name, '-'), REGEXP_REPLACE(name, '-0', '#') \
             FROM events ORDER BY id",
        ),
        unordered(
            "union distinct",
            "SELECT note FROM events UNION SELECT note FROM events",
        ),
        unordered(
            "union distinct",
            "SELECT id FROM events WHERE id <= 3 UNION DISTINCT SELECT id FROM events WHERE id <= 5",
        ),
        ordered(
            "union distinct",
            "SELECT name FROM users WHERE id <= 2 \
             UNION ALL SELECT name FROM users WHERE id <= 2 \
             UNION SELECT name FROM users WHERE id = 3 ORDER BY name",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id, e.name FROM events e \
             WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = e.id) ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id FROM events e \
             WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.id = e.id) ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id, e.score FROM events e \
             WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = e.id) AND e.score > 30 \
             ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT u.id, u.name FROM users u \
             WHERE EXISTS (SELECT 1 FROM events e WHERE e.id = u.id AND e.active = 1) \
             ORDER BY u.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT u.id FROM users u \
             WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.id = u.id AND e.score >= 50) \
             ORDER BY u.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT u.id, u.name FROM users u \
             WHERE EXISTS (SELECT 1 FROM events e \
             WHERE e.active = 0 AND e.id = u.id AND e.note IS NOT NULL) \
             ORDER BY u.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id FROM events e \
             WHERE EXISTS (SELECT 1 FROM users u WHERE u.name <> 'user-05' AND e.id = u.id) \
             ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id, e.name FROM events e \
             WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = e.id AND u.id = e.score DIV 10) \
             ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT e.id FROM events e \
             WHERE NOT EXISTS (SELECT 1 FROM users u \
             WHERE u.id = e.id AND u.id = e.score DIV 10 AND u.name <> 'user-03') \
             ORDER BY e.id",
        ),
        ordered(
            "in subqueries",
            "SELECT u.id, u.name FROM users u \
             WHERE u.id IN (SELECT e.id FROM events e WHERE e.score = u.id * 10) \
             ORDER BY u.id",
        ),
        ordered(
            "in subqueries",
            "SELECT u.id FROM users u \
             WHERE u.id IN (SELECT e.id FROM events e \
             WHERE e.score = u.id * 10 AND e.active = 1) ORDER BY u.id",
        ),
        ordered(
            "in subqueries",
            "SELECT u.id FROM users u \
             WHERE u.id NOT IN (SELECT e.id FROM events e \
             WHERE e.score = u.id * 10 AND e.active = 1) ORDER BY u.id",
        ),
        ordered(
            "recursive cte",
            "WITH RECURSIVE seq (n) AS (\
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 10) \
             SELECT n FROM seq ORDER BY n",
        ),
        unordered(
            "recursive cte",
            "WITH RECURSIVE r (n) AS (\
             SELECT 1 UNION SELECT n * 2 % 7 FROM r) SELECT n FROM r",
        ),
        ordered(
            "recursive cte",
            "WITH RECURSIVE chain (id) AS (\
             SELECT id FROM events WHERE id = 1 \
             UNION ALL SELECT e.id FROM events e JOIN chain c ON e.id = c.id + 1 \
             WHERE e.id <= 5) \
             SELECT c.id, e.name FROM chain c JOIN events e ON e.id = c.id ORDER BY c.id",
        ),
        ordered(
            "recursive cte",
            "WITH RECURSIVE t (n, s) AS (\
             SELECT 1, CAST('x' AS CHAR(20)) \
             UNION ALL SELECT n + 1, CONCAT(s, 'y') FROM t WHERE n < 5) \
             SELECT n, s FROM t ORDER BY n",
        ),
        ordered(
            "decimal round",
            "SELECT ROUND(CAST(322.905 AS DECIMAL(9,3)), 2), \
             ROUND(CAST(-322.905 AS DECIMAL(9,3)), 2), \
             ROUND(CAST(2.5 AS DECIMAL(4,1))), ROUND(CAST(-2.5 AS DECIMAL(4,1)))",
        ),
        ordered(
            "decimal round",
            "SELECT ROUND(CAST(1234.567 AS DECIMAL(10,3)), -2), \
             ROUND(CAST(9.99 AS DECIMAL(4,2)), 1), \
             ROUND(CAST(1.005 AS DECIMAL(6,3)), 2), \
             ROUND(CAST(322.905 AS DECIMAL(9,3)), 5)",
        ),
        unordered(
            "decimal round",
            "SELECT active, ROUND(AVG(score), 2), ROUND(AVG(id) / 7, 2) \
             FROM events GROUP BY active",
        ),
        ordered(
            "decimal round",
            "SELECT CEIL(CAST(2.5 AS DECIMAL(4,1))), CEIL(CAST(-2.5 AS DECIMAL(4,1))), \
             FLOOR(CAST(2.5 AS DECIMAL(4,1))), FLOOR(CAST(-2.5 AS DECIMAL(4,1))), \
             CEIL(CAST(3.0 AS DECIMAL(4,1))), FLOOR(CAST(-3.0 AS DECIMAL(4,1)))",
        ),
        ordered(
            "decimal round",
            "SELECT TRUNCATE(CAST(322.905 AS DECIMAL(9,3)), 2), \
             TRUNCATE(CAST(-322.905 AS DECIMAL(9,3)), 2), \
             TRUNCATE(CAST(1234.567 AS DECIMAL(10,3)), -2), \
             TRUNCATE(CAST(9.99 AS DECIMAL(4,2)), 5), TRUNCATE(CAST(1.005 AS DECIMAL(6,3)), 2)",
        ),
        ordered(
            "div precedence",
            "SELECT id, id DIV 2 + 1, id DIV 3 IS NULL, score DIV 10 AND active \
             FROM events ORDER BY id",
        ),
        ordered(
            "div precedence",
            "SELECT id FROM events WHERE id = score DIV 10 AND active = 1 ORDER BY id",
        ),
        ordered(
            "multi-key join",
            "SELECT e.id, u.name FROM events e \
             JOIN users u ON u.id = e.id AND u.id = e.score DIV 10 ORDER BY e.id",
        ),
        ordered(
            "exists subqueries",
            "SELECT id, name FROM events \
             WHERE EXISTS (SELECT 1 FROM users WHERE users.id > 6) ORDER BY id",
        ),
        ordered(
            "exists subqueries",
            "SELECT id FROM events \
             WHERE NOT EXISTS (SELECT 1 FROM users WHERE users.id > 100) ORDER BY id",
        ),
        ordered(
            "exists subqueries",
            "SELECT EXISTS (SELECT 1 FROM users WHERE name = 'user-03'), \
             EXISTS (SELECT 1 FROM users WHERE name = 'nobody'), \
             NOT EXISTS (SELECT 1 FROM events WHERE score > 90)",
        ),
        ordered(
            "datetime calendar",
            "SELECT QUARTER('2024-05-15'), DAYOFWEEK('2024-05-15'), WEEKDAY('2024-05-15'), \
             DAYOFYEAR('2024-05-15'), WEEK('2024-01-01'), WEEK('2024-01-07'), \
             WEEK('2024-12-31'), WEEKOFYEAR('2024-01-01'), WEEK('2024-06-15', 3)",
        ),
        ordered(
            "datetime calendar",
            "SELECT YEARWEEK('2024-01-01'), YEARWEEK('2023-01-01'), YEARWEEK('2024-06-15'), \
             LAST_DAY('2024-02-10'), LAST_DAY('2023-02-10'), DAYNAME('2024-05-15'), \
             MONTHNAME('2024-05-15')",
        ),
        ordered(
            "datetime calendar",
            "SELECT TO_DAYS('1970-01-01'), TO_DAYS('2024-05-15'), FROM_DAYS(719528), \
             TIME_TO_SEC('01:01:01'), SEC_TO_TIME(3661), SEC_TO_TIME(0 - 3661), \
             SEC_TO_TIME(9999999)",
        ),
        ordered(
            "datetime calendar",
            "SELECT MAKEDATE(2024, 60), MAKEDATE(2024, 0), MAKEDATE(2023, 365), \
             EXTRACT(QUARTER FROM '2024-05-15'), EXTRACT(WEEK FROM '2024-01-07'), \
             TIMESTAMPADD(DAY, 10, '2024-05-15'), TIMESTAMPADD(HOUR, 0 - 5, '2024-05-15 03:00:00')",
        ),
        ordered(
            "datetime calendar",
            "SELECT STR_TO_DATE('15,5,2024', '%d,%m,%Y'), \
             STR_TO_DATE('2024-05-15 10:20:30', '%Y-%m-%d %H:%i:%s'), \
             STR_TO_DATE('nope', '%Y-%m-%d')",
        ),
        ordered(
            "group concat",
            "SELECT active, GROUP_CONCAT(name ORDER BY id DESC SEPARATOR '|') \
             FROM events GROUP BY active ORDER BY active",
        ),
        ordered(
            "group concat",
            "SELECT GROUP_CONCAT(DISTINCT note ORDER BY note), \
             GROUP_CONCAT(score ORDER BY score DESC SEPARATOR ';') FROM events",
        ),
        ordered(
            "group concat",
            // MySQL truncates at group_concat_max_len (default 1024 bytes).
            "SELECT active, LENGTH(GROUP_CONCAT(REPEAT(name, 40) ORDER BY id)) \
             FROM events GROUP BY active ORDER BY active",
        ),
        ordered(
            "numeric scalars",
            "SELECT id, ABS(CAST(id AS SIGNED) - 5), SIGN(CAST(id AS SIGNED) - 5), \
             ABS(0 - score / 7) FROM events ORDER BY id",
        ),
        ordered(
            "numeric scalars",
            "SELECT id, POWER(id, 2), ROUND(SQRT(score), 6), ROUND(EXP(id / 100), 6) \
             FROM events ORDER BY id",
        ),
        ordered(
            "numeric scalars",
            "SELECT ROUND(LN(score), 6), ROUND(LOG(2, score), 6), ROUND(LOG2(score), 6), \
             ROUND(LOG10(score), 6), LN(0), SQRT(0 - 1), LOG(1, 5) FROM events WHERE id = 4",
        ),
        ordered(
            "numeric scalars",
            "SELECT id, TRUNCATE(score / 7, 2), MOD(score, 7), score % 7, MOD(score, 0) \
             FROM events ORDER BY id",
        ),
        ordered(
            "numeric scalars",
            "SELECT id, GREATEST(id, score, 25), LEAST(id, score, 25), \
             GREATEST(score / 7, id), LEAST(name, note), GREATEST('10', '9') \
             FROM events ORDER BY id",
        ),
        ordered(
            "numeric scalars",
            "SELECT id, IFNULL(note, 'none'), IFNULL(NULL, id) FROM events ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, CONCAT_WS('-', name, note, NULL), CONCAT_WS(NULL, name, note) \
             FROM events ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, REVERSE(name), REPEAT(note, 2), SPACE(3), LPAD(id, 6, '0'), \
             RPAD(name, 12, '.') FROM events ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, INSTR(name, 'event'), INSTR(name, 'zzz'), FIND_IN_SET('b', 'a,b,c'), \
             FIND_IN_SET('d', 'a,b,c'), FIND_IN_SET(note, 'Alpha,beta') \
             FROM events ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, ASCII(name), ORD(name), HEX(name), HEX(id), UNHEX('414243'), \
             UNHEX('zz') FROM events WHERE id <= 3 ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, ELT(2, 'a', 'b', 'c'), ELT(9, 'a'), FIELD('b', 'a', 'b'), \
             FIELD('z', 'a', 'b'), FORMAT(1234567.8915, 2), FORMAT(score * 1000.4, 0) \
             FROM events ORDER BY id",
        ),
        ordered(
            "string scalars",
            "SELECT id, TO_BASE64(name), FROM_BASE64(TO_BASE64(name)), FROM_BASE64('!!') \
             FROM events ORDER BY id",
        ),
        ordered(
            "decimal arithmetic",
            "SELECT CAST(12.50 AS DECIMAL(10, 2)) + 1, \
             CAST(12.50 AS DECIMAL(10, 2)) - CAST(0.75 AS DECIMAL(10, 2)), \
             CAST(1.25 AS DECIMAL(10, 2)) * 3, \
             CAST(1.25 AS DECIMAL(10, 2)) * CAST(0.5 AS DECIMAL(10, 1)), \
             CAST(99999999.99 AS DECIMAL(10, 2)) + CAST(0.01 AS DECIMAL(10, 2))",
        ),
        ordered(
            "decimal arithmetic",
            "SELECT CAST(12.567 AS DECIMAL(10, 2)), CAST(12.565 AS DECIMAL(10, 2)), \
             CAST(0 - 12.565 AS DECIMAL(10, 2)), CAST(score AS DECIMAL(10, 3)), \
             CAST('88.4499' AS DECIMAL(6, 2)) FROM events WHERE id = 1",
        ),
        ordered(
            "decimal ordering",
            // Lexical text ordering would put 8.5714 above 14.2857.
            "SELECT id, score / 7 AS share FROM events ORDER BY share DESC, id",
        ),
        ordered(
            "decimal ordering",
            "SELECT name, SUM(score) AS total, \
             ROW_NUMBER() OVER (ORDER BY SUM(score) / 7 DESC) AS heaviest \
             FROM events GROUP BY name ORDER BY name",
        ),
        ordered(
            "decimal average",
            "SELECT AVG(score), AVG(id), AVG(active) FROM events",
        ),
        ordered(
            "decimal average",
            "SELECT active, AVG(score), COUNT(*) FROM events \
             GROUP BY active ORDER BY active",
        ),
        ordered(
            "datetime helpers",
            "SELECT TIMESTAMPDIFF(SECOND, '2024-01-01 00:00:00', '2024-01-01 00:05:30'), \
             TIMESTAMPDIFF(MINUTE, '2024-01-01 00:00:00', '2024-01-02 03:04:00'), \
             TIMESTAMPDIFF(HOUR, '2024-01-02 03:00:00', '2024-01-01 00:00:00'), \
             TIMESTAMPDIFF(DAY, '2024-01-01 12:00:00', '2024-03-01 11:59:59')",
        ),
        ordered(
            "datetime helpers",
            "SELECT TIMESTAMPDIFF(MONTH, '2020-01-31', '2020-02-29'), \
             TIMESTAMPDIFF(MONTH, '2020-01-31', '2020-03-01'), \
             TIMESTAMPDIFF(MONTH, '2020-03-01', '2020-01-31'), \
             TIMESTAMPDIFF(YEAR, '2020-02-29', '2024-02-28'), \
             TIMESTAMPDIFF(YEAR, '2020-02-29', '2024-02-29')",
        ),
        ordered(
            "datetime helpers",
            "SELECT CEIL(7 / 2), FLOOR(7 / 2), CEIL(-7 / 2), FLOOR(-7 / 2), \
             CEIL(10 * 0.95), CEIL(4 / 2)",
        ),
        ordered(
            "datetime helpers",
            "SELECT '2024-01-31' + INTERVAL 1 DAY, '2024-01-31' + INTERVAL 1 MONTH, \
             '2024-03-31' - INTERVAL 1 MONTH, '2024-02-29' + INTERVAL 1 YEAR, \
             '2024-01-01 23:30:00' + INTERVAL 45 MINUTE",
        ),
        ordered(
            "datetime helpers",
            "SELECT id, CEIL(score / 3) AS c, FLOOR(score / 3) AS f FROM events ORDER BY id",
        ),
    ]
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
    .map_err(|error| error.to_string())?
    .with_key_columns([1])
    .map_err(|error| error.to_string())?;
    let users = TableEntry::new(
        USERS_ID,
        "users",
        users_schema,
        TableStatistics::with_row_count(8),
    )
    .map_err(|error| error.to_string())?
    .with_key_columns([1])
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
            Column::new(5, "note", DataType::Utf8, true),
        ],
    )
    .map_err(|error| error.to_string())
}

fn users_schema() -> Result<TableSchema, String> {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
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
            match id {
                1 | 7 => Value::Utf8("Alpha".to_owned()),
                2 | 8 => Value::Utf8("alpha".to_owned()),
                4 | 10 => Value::Utf8("Beta".to_owned()),
                5 => Value::Utf8("beta".to_owned()),
                3 | 6 | 9 => Value::Null,
                _ => unreachable!("oracle event IDs are 1 through 10"),
            },
        ],
        id,
        false,
    )
}

fn user_row(id: u64) -> StoredRow {
    let id = i64::try_from(id).expect("small seed id");
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("non-empty user key"),
        vec![Value::Int64(id), Value::Utf8(format!("user-{id:02}"))],
        u64::try_from(id).expect("positive oracle event ID"),
        false,
    )
}
