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
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const EVENTS_ID: TableId = TableId::new(1);
const USERS_ID: TableId = TableId::new(2);
const ORDERS_ID: TableId = TableId::new(3);
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
/// Generated parametric loops + hand-written edges + typed multi-table diversify cases.
/// Prefer `bun run scripts/oracle-coverage.ts` over this count when judging diversity.
const EXPECTED_CASES: usize = 1070;
/// orders.status declaration order - deliberately disagrees with the
/// alphabetical order at every adjacent pair.
const ENUM_LABELS: [&str; 5] = ["pending", "processing", "shipped", "delivered", "cancelled"];

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

#[test]
fn oracle_case_inventory_matches_the_declared_gate() {
    assert_eq!(oracle_cases().len(), EXPECTED_CASES);
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
           note VARCHAR(32) NULL,\
           tag VARCHAR(24) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         CREATE TABLE users (\
           id BIGINT PRIMARY KEY,\
           name VARCHAR(32) NOT NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         CREATE TABLE orders (\
           id BIGINT UNSIGNED PRIMARY KEY,\
           user_id BIGINT NOT NULL,\
           total DECIMAL(12,2) NOT NULL,\
           placed_at DATETIME NOT NULL,\
           status ENUM('pending','processing','shipped','delivered','cancelled') NOT NULL,\
           meta JSON NULL\
         ) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;\
         INSERT INTO events VALUES\
           (1,'event-01',10,0,'Alpha','red'),(2,'event-02',20,1,'alpha','RED'),\
           (3,'event-03',30,0,NULL,'red '),(4,'event-04',40,1,'Beta','blue'),\
           (5,'event-05',50,0,'beta','BLUE'),(6,'event-06',60,1,NULL,'blue'),\
           (7,'event-07',70,0,'Alpha','Green'),(8,'event-08',80,1,'alpha','green'),\
           (9,'event-09',90,0,NULL,'RED'),(10,'event-10',100,1,'Beta','Blue');\
         INSERT INTO users VALUES\
           (1,'user-01'),(2,'user-02'),(3,'user-03'),(4,'user-04'),\
           (5,'user-05'),(6,'user-06'),(7,'user-07'),(8,'user-08');\
         INSERT INTO orders VALUES\
           (1,1,10.50,'2024-01-15 10:00:00','shipped','{\"tags\":[\"premium\"],\"score\":1.5,\"items\":[1,2,3,4]}'),\
           (2,1,198.82,'2024-02-29 12:34:56','shipped','{\"tags\":[\"bulk\"],\"score\":2.0,\"items\":[1]}'),\
           (3,2,0.01,'2024-03-01 00:00:00','pending',NULL),\
           (4,2,99999999.99,'2024-03-08 06:30:00','cancelled','{\"tags\":[],\"score\":0,\"items\":[]}'),\
           (5,3,12.35,'2024-06-15 18:00:00','shipped','{\"tags\":[\"premium\",\"rush\"],\"score\":9.9,\"items\":[1,2,3,4,5]}'),\
           (6,3,50.00,'2024-07-04 09:15:00','pending','{\"tags\":[\"gift\"],\"score\":3,\"items\":[7,8]}'),\
           (7,4,7.00,'2024-11-01 01:30:00','shipped',NULL),\
           (8,5,100.00,'2025-01-01 00:00:00','delivered','{\"tags\":[\"premium\"],\"score\":1,\"items\":[1,2]}'),\
           (9,9,25.25,'2025-02-01 12:00:00','pending','{\"tags\":[\"orphan\"],\"score\":0.5,\"items\":[9]}'),\
           (10,1,0.00,'2025-02-28 23:59:59','cancelled',NULL),\
           (11,6,33.33,'2025-03-01 08:00:00','shipped','{\"tags\":[\"a\"],\"score\":4.25,\"items\":[1,2,3]}'),\
           (12,7,64.00,'2025-04-01 16:45:00','delivered','{\"tags\":[\"premium\"],\"score\":8,\"items\":[2,4,6,8]}');",
    )?;

    let events_directory =
        tempfile::tempdir().map_err(|error| format!("events tempdir: {error}"))?;
    let users_directory = tempfile::tempdir().map_err(|error| format!("users tempdir: {error}"))?;
    let orders_directory =
        tempfile::tempdir().map_err(|error| format!("orders tempdir: {error}"))?;
    let events_schema = events_schema()?;
    let users_schema = users_schema()?;
    let orders_schema = orders_schema()?;
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
    let mut orders = TableStore::open(
        orders_directory.path(),
        orders_schema.clone(),
        StoreOptions::default(),
    )
    .map_err(|error| format!("open orders: {error}"))?;
    events
        .ingest((1..=10).map(event_row).collect())
        .map_err(|error| format!("ingest events: {error}"))?;
    users
        .ingest((1..=8).map(user_row).collect())
        .map_err(|error| format!("ingest users: {error}"))?;
    orders
        .ingest(order_rows())
        .map_err(|error| format!("ingest orders: {error}"))?;
    let events_snapshot = events.snapshot();
    let users_snapshot = users.snapshot();
    let orders_snapshot = orders.snapshot();
    let catalog = catalog(events_schema, users_schema, orders_schema)?;
    let provider = SnapshotScanProvider::new([
        (DATABASE_ID, EVENTS_ID, &events_snapshot),
        (DATABASE_ID, USERS_ID, &users_snapshot),
        (DATABASE_ID, ORDERS_ID, &orders_snapshot),
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
    let physical = PhysicalPlanner::plan(logical, Collation::default())
        .map_err(|error| format!("physical plan: {error}"))?;
    let mut execution = Execution::start(physical, provider, MEMORY_LIMIT, Collation::default())
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
        // The oracle compares what MySQL displays, and MySQL displays an
        // ENUM as its label.
        Value::Utf8(value) | Value::Enum { label: value, .. } => OracleValue::Exact(value.clone()),
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
    // ENUM semantics, generated over every label and operator: MySQL sorts
    // an ENUM by its declared ordinal but COMPARES it - ranges, BETWEEN,
    // MIN/MAX - as its label string. The declaration order and the
    // alphabetical order disagree everywhere here, so a path applying the
    // wrong rule cannot pass by luck (found live by the e2e gate; the
    // oracle's status column was plain VARCHAR and never saw it).
    for label in ENUM_LABELS {
        for op in [">", ">=", "<", "<=", "=", "<>"] {
            cases.push(OracleCase {
                family: "enum semantics",
                sql: format!(
                    "SELECT COUNT(*), MIN(status), MAX(status) FROM orders \
                     WHERE status {op} '{label}'"
                ),
                ordered: true,
            });
        }
    }
    for limit in 1..=12 {
        cases.push(OracleCase {
            family: "enum semantics",
            sql: format!("SELECT id, status FROM orders ORDER BY status, id LIMIT {limit}"),
            ordered: true,
        });
        cases.push(OracleCase {
            family: "enum semantics",
            sql: format!("SELECT id, status FROM orders ORDER BY status DESC, id LIMIT {limit}"),
            ordered: true,
        });
    }
    for low in ENUM_LABELS {
        for high in ENUM_LABELS {
            cases.push(OracleCase {
                family: "enum semantics",
                sql: format!(
                    "SELECT COUNT(*) FROM orders WHERE status BETWEEN '{low}' AND '{high}'"
                ),
                ordered: true,
            });
        }
    }
    for sql in [
        "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY COUNT(*) DESC, status",
        "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY status",
        "SELECT DISTINCT status FROM orders ORDER BY status",
        "SELECT DISTINCT status FROM orders ORDER BY status DESC",
        "SELECT COUNT(*) FROM orders WHERE status IN ('pending', 'delivered')",
        "SELECT COUNT(*) FROM orders WHERE status NOT IN ('shipped')",
    ] {
        cases.push(OracleCase {
            family: "enum semantics",
            sql: sql.to_owned(),
            ordered: true,
        });
    }
    // Mixed-collation grouping: events.note is general_ci next to the
    // orders columns' 0900_ai_ci, so these fold each key by its own rules.
    // Projections stay spelling-independent (counts only): the reported
    // spelling of a case-insensitively equal group follows scan order,
    // which differs between engines by design (documented gap #10).
    for floor in 1..=12 {
        cases.push(OracleCase {
            family: "mixed collation grouping",
            sql: format!(
                "SELECT COUNT(*) FROM (SELECT tag, status FROM events e \
                 JOIN orders o ON o.user_id = e.id WHERE o.id >= {floor} \
                 GROUP BY tag, status) g"
            ),
            ordered: true,
        });
    }
    for sql in [
        "SELECT COUNT(DISTINCT tag) FROM events",
        "SELECT COUNT(*) FROM (SELECT tag FROM events GROUP BY tag) g",
        "SELECT COUNT(*) FROM (SELECT tag, status, COUNT(*) AS c FROM events e \
         JOIN orders o ON o.user_id = e.id GROUP BY tag, status HAVING COUNT(*) > 1) g",
        "SELECT COUNT(DISTINCT tag), COUNT(DISTINCT status) FROM events e \
         JOIN orders o ON o.user_id = e.id",
    ] {
        cases.push(OracleCase {
            family: "mixed collation grouping",
            sql: sql.to_owned(),
            ordered: true,
        });
    }
    // The twelve binder-callable names the coverage report shows no oracle
    // SQL ever exercised - mostly aliases, which is exactly where a rename
    // slips through untested. Over typed columns, not constants, and the
    // two time-of-day names in shapes whose OUTPUT is deterministic.
    for sql in [
        "SELECT id, LEFT(name, 5), RIGHT(name, 2), SUBSTR(name, 3, 4) FROM events ORDER BY id",
        "SELECT id, LCASE(note), UCASE(note), CHARACTER_LENGTH(name) FROM events ORDER BY id",
        "SELECT id, CEILING(total), POW(user_id, 2), CEILING(total / 7) FROM orders ORDER BY id",
        "SELECT id, DAYOFMONTH(placed_at), CHAR(65, 66, 67) FROM orders ORDER BY id",
        "SELECT CURRENT_TIME() >= '00:00:00', CURTIME() <= '24:00:00'",
        "SELECT id, LEFT(note, 2), RIGHT(note, 1) FROM events ORDER BY id",
        "SELECT LCASE(LEFT(status, 4)), COUNT(*) FROM orders GROUP BY LCASE(LEFT(status, 4)) \
         ORDER BY LCASE(LEFT(status, 4))",
        "SELECT id, SUBSTR(name, CHARACTER_LENGTH(name) - 1) FROM users ORDER BY id",
        "SELECT CEILING(AVG(total)), POW(COUNT(*), 2) FROM orders",
        "SELECT id, UCASE(SUBSTR(tag, 1, 3)), CHARACTER_LENGTH(tag) FROM events ORDER BY id",
    ] {
        cases.push(OracleCase {
            family: "alias functions",
            sql: sql.to_owned(),
            ordered: true,
        });
    }
    // Window functions over the typed tables: no oracle family covered
    // them at all, while the e2e corpus leans on them.
    for sql in [
        "SELECT id, ROW_NUMBER() OVER (ORDER BY total, id) AS r FROM orders ORDER BY r",
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY placed_at, id) AS r \
         FROM orders ORDER BY id",
        "SELECT id, status, ROW_NUMBER() OVER (ORDER BY status, id) AS r FROM orders ORDER BY r",
        "SELECT id, SUM(total) OVER (PARTITION BY user_id) AS s FROM orders ORDER BY id",
        "SELECT id, COUNT(*) OVER (PARTITION BY status) AS c FROM orders ORDER BY id",
        "SELECT id, AVG(score) OVER (PARTITION BY active) AS a FROM events ORDER BY id",
        "SELECT id, MIN(placed_at) OVER (PARTITION BY user_id) AS first_order \
         FROM orders ORDER BY id",
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY active ORDER BY score DESC, id) AS r \
         FROM events ORDER BY id",
    ] {
        cases.push(OracleCase {
            family: "window functions",
            sql: sql.to_owned(),
            ordered: true,
        });
    }
    // Typed multi-table diversify: several types and features interacting
    // in one statement, which is what the coverage report asks to grow -
    // JSON next to ENUM next to DECIMAL, correlated subqueries over joins,
    // mixed-collation text riding through aggregation.
    for sql in [
        "SELECT o.id, o.status, e.tag, o.total FROM orders o \
         JOIN events e ON e.id = o.user_id \
         WHERE o.status >= 'pending' AND o.total > 5.00 ORDER BY o.id",
        "SELECT u.id, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id \
         AND o.status <> 'cancelled') AS live_orders FROM users u ORDER BY u.id",
        "SELECT o.status, COUNT(DISTINCT e.tag), SUM(o.total) FROM orders o \
         JOIN events e ON e.id = o.user_id GROUP BY o.status ORDER BY o.status",
        "SELECT o.id, JSON_EXTRACT(o.meta, '$.score'), o.status FROM orders o \
         WHERE o.meta IS NOT NULL AND o.status IN ('shipped', 'delivered') ORDER BY o.id",
        "SELECT e.tag, MIN(o.placed_at), MAX(o.total) FROM events e \
         JOIN orders o ON o.user_id = e.id GROUP BY e.tag \
         ORDER BY MIN(o.placed_at), MAX(o.total)",
        "SELECT e.name, MIN(o.placed_at), MAX(o.total) FROM events e \
         JOIN orders o ON o.user_id = e.id GROUP BY e.name \
         ORDER BY MIN(o.placed_at), MAX(o.total)",
        "SELECT o.id, o.total, CASE WHEN o.status = 'cancelled' THEN 0.00 ELSE o.total END \
         FROM orders o WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = o.user_id) \
         ORDER BY o.id",
        "SELECT DATE(o.placed_at), COUNT(*), SUM(o.total) FROM orders o \
         WHERE o.status BETWEEN 'delivered' AND 'shipped' \
         GROUP BY DATE(o.placed_at) ORDER BY DATE(o.placed_at)",
        "SELECT u.name, COALESCE((SELECT MAX(o.total) FROM orders o \
         WHERE o.user_id = u.id), 0) FROM users u ORDER BY u.id",
        "SELECT u.name, COUNT(*) FROM orders o JOIN users u ON u.id = o.user_id \
         GROUP BY u.name ORDER BY u.name",
    ] {
        cases.push(OracleCase {
            family: "typed diversify",
            sql: sql.to_owned(),
            ordered: true,
        });
    }
    // Row-constructor IN, the natural predicate for composite-key tables
    // (a customer needed it to address one; MySQL n=2, Pintail rejected).
    // Desugars to OR-of-AND equalities in the binder; these pin the
    // semantics across types, NOT IN, misses, and single-column tuples.
    for floor in 0..6 {
        cases.push(OracleCase {
            family: "row constructor IN",
            sql: format!(
                "SELECT COUNT(*) FROM orders WHERE (user_id, status) IN \
                 (({floor}, 'shipped'), ({}, 'pending'), (9, 'nope'))",
                floor + 1
            ),
            ordered: true,
        });
    }
    for sql in [
        "SELECT id, name FROM events WHERE (id, tag) IN ((1, 'red'), (4, 'blue'), (7, 'zzz')) \
         ORDER BY id",
        "SELECT COUNT(*) FROM events WHERE (id, tag) NOT IN ((1, 'red'), (2, 'RED'))",
        "SELECT COUNT(*) FROM orders WHERE (id, user_id, status) IN \
         ((1, 1, 'shipped'), (4, 2, 'cancelled'), (4, 2, 'shipped'))",
        "SELECT COUNT(*) FROM users WHERE (id) IN ((1), (3), (99))",
        "SELECT o.id FROM orders o JOIN users u ON u.id = o.user_id \
         WHERE (o.user_id, o.status) IN ((1, 'shipped'), (3, 'pending')) ORDER BY o.id",
        "SELECT COUNT(*) FROM events WHERE (id, note) IN ((1, 'Alpha'), (3, 'missing'))",
    ] {
        cases.push(OracleCase {
            family: "row constructor IN",
            sql: sql.to_owned(),
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
        // JSON string results collate utf8mb4_bin in MySQL - case-SENSITIVE
        // comparison and grouping, losing only to a real column's collation.
        // The customer's repro plus the coercibility ladder, measured live.
        ordered(
            "json bin collation",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"A\\\"}','$.k')) = JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"a\\\"}','$.k'))",
        ),
        ordered(
            "json bin collation",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"A\\\"}','$.k')) = 'a'",
        ),
        ordered(
            "json bin collation",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"A\\\"}','$.k')) = 'A'",
        ),
        unordered(
            "json bin collation",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(CONCAT('{\\\"k\\\":\\\"', note, '\\\"}'), '$.k')), COUNT(*) \
             FROM events WHERE note IS NOT NULL \
             GROUP BY JSON_UNQUOTE(JSON_EXTRACT(CONCAT('{\\\"k\\\":\\\"', note, '\\\"}'), '$.k'))",
        ),
        unordered(
            "json bin collation",
            "SELECT DISTINCT JSON_UNQUOTE(JSON_EXTRACT(CONCAT('{\\\"k\\\":\\\"', note, '\\\"}'), '$.k')) \
             FROM events WHERE note IS NOT NULL",
        ),
        ordered(
            "json bin collation",
            "SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' = 'premium'",
        ),
        ordered(
            "json bin collation",
            "SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' = 'PREMIUM'",
        ),
        ordered(
            "json bin collation",
            "SELECT id, meta->>'$.tags[0]' FROM orders ORDER BY meta->>'$.tags[0]', id",
        ),
        ordered(
            "json bin collation",
            "SELECT UPPER(JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"alpha\\\"}','$.k'))) = 'ALPHA'",
        ),
        ordered(
            "json bin collation",
            "SELECT id, JSON_TYPE(meta) = 'object' FROM orders ORDER BY id",
        ),
        ordered(
            "json bin collation",
            "SELECT COUNT(*) FROM events WHERE JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"alpha\\\"}','$.k')) = note",
        ),
        ordered(
            "json bin collation",
            "SELECT COUNT(*) FROM events WHERE JSON_UNQUOTE(JSON_EXTRACT('{\\\"k\\\":\\\"alpha\\\"}','$.k')) = tag",
        ),
        ordered(
            "like default escape",
            "SELECT 'a%b' LIKE 'a\\%b', 'axb' LIKE 'a\\%b', 'a_b' LIKE 'a\\_b', \
                    'axb' LIKE 'a\\_b', 'a%b' LIKE 'a!%b' ESCAPE '!', 'C:\\\\dir' LIKE 'C:\\\\\\\\%'",
        ),
        ordered(
            "binary operator",
            "SELECT BINARY 'A' = BINARY 'a', BINARY 'a' = BINARY 'a', \
                    'a' = BINARY 'A', BINARY 'abc' < BINARY 'abd', BINARY 'a' = BINARY 'a '",
        ),
        ordered(
            "binary operator",
            "SELECT COUNT(*) FROM events WHERE BINARY note = 'Alpha'",
        ),
        ordered(
            "binary operator",
            "SELECT COUNT(*) FROM events WHERE BINARY note = 'alpha'",
        ),
        ordered(
            "null-only derived",
            "SELECT k, COUNT(*) FROM (SELECT NULL AS k) d GROUP BY k",
        ),
        ordered(
            "null-only derived",
            "SELECT COUNT(k), COUNT(*) FROM (SELECT NULL AS k UNION ALL SELECT NULL) d",
        ),
        ordered(
            "rejected constructs",
            "SELECT CAST(-1 AS UNSIGNED), CAST(1 AS UNSIGNED), CAST('7' AS UNSIGNED)",
        ),
        ordered(
            "rejected constructs",
            "SELECT TRIM(BOTH 'x' FROM 'xxaxx'), TRIM(LEADING 'x' FROM 'xxaxx'), \
                    TRIM(TRAILING 'x' FROM 'xxaxx'), TRIM('ab' FROM 'ababzab')",
        ),
        ordered(
            "rejected constructs",
            "SELECT NULL <=> NULL, 1 <=> NULL, NULL <=> 1, 1 <=> 1, 1 <=> 2, 'a' <=> 'A'",
        ),
        ordered(
            "rejected constructs",
            "SELECT COUNT(*) FROM events WHERE note <=> NULL",
        ),
        ordered(
            "rejected constructs",
            "SELECT 'A' = 'a' COLLATE utf8mb4_bin, 'A' = 'a' COLLATE utf8mb4_general_ci, \
                    'a' = 'a ' COLLATE utf8mb4_general_ci, 'a' = 'a ' COLLATE utf8mb4_0900_ai_ci",
        ),
        unordered(
            "rejected constructs",
            "SELECT note COLLATE utf8mb4_bin AS k, COUNT(*) FROM events \
             WHERE note IS NOT NULL GROUP BY note COLLATE utf8mb4_bin",
        ),
        ordered(
            "json bin collation",
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(CONCAT('{\\\"k\\\":\\\"', note, '\\\"}'), '$.k')) AS t, COUNT(*) \
             FROM events WHERE note IS NOT NULL \
             GROUP BY JSON_UNQUOTE(JSON_EXTRACT(CONCAT('{\\\"k\\\":\\\"', note, '\\\"}'), '$.k')) ORDER BY t",
        ),
        // Coercion matrix: mixed types in comparison, the class where one
        // BINARY operand silently numeric-coerced everything equal.
        ordered(
            "coercion matrix",
            "SELECT '1' = 1, '1.0' = 1, ' 1' = 1, '1x' = 1, 'x' = 0, \
                    1.0 = 1, '0.5' = 0.5, TRUE = 1, TRUE = '1', FALSE = ''",
        ),
        ordered(
            "coercion matrix",
            "SELECT 1 < '2', '10' < 9, 'abc' = 0, '' = 0, NULL = '', \
                    '2025-01-01' = DATE('2025-01-01')",
        ),
        // Prefix operators against every comparison shape: the precedence
        // class that broke BINARY and the JSON arrows.
        ordered(
            "prefix precedence",
            "SELECT BINARY 'a' LIKE 'A', BINARY 'a' LIKE 'a', \
                    BINARY 'a' IS NULL, BINARY NULL IS NULL, \
                    NOT 'a' = 'a', NOT 1 > 2, -1 < 0",
        ),
        ordered(
            "prefix precedence",
            "SELECT COUNT(*) FROM events WHERE BINARY note LIKE 'Alpha'",
        ),
        ordered(
            "prefix precedence",
            "SELECT COUNT(*) FROM events WHERE BINARY note BETWEEN 'A' AND 'B'",
        ),
        // PAD SPACE across contexts: '=' folds trailing spaces per collation,
        // LIKE never does, IN follows '='.
        ordered(
            "pad space matrix",
            "SELECT 'a' = 'a ' COLLATE utf8mb4_general_ci, 'a' LIKE 'a ', \
                    'a ' LIKE 'a', 'a' IN ('a ' COLLATE utf8mb4_general_ci), \
                    'a' IN ('a ', 'b')",
        ),
        unordered(
            "pad space matrix",
            "SELECT tag, COUNT(*) FROM events GROUP BY tag",
        ),
        // LIKE escape corners: escaped escape, escape at pattern end,
        // underscore, and a custom escape that frees the backslash.
        ordered(
            "like escape corners",
            "SELECT '50%' LIKE '50\\%', '50x' LIKE '50\\%', 'a\\\\b' LIKE 'a\\\\\\\\b', \
                    '_' LIKE '\\_', 'x' LIKE '\\_', 'a\\\\' LIKE 'a%', \
                    'a%' LIKE 'a|%' ESCAPE '|', 'a\\\\b' LIKE 'a\\\\b' ESCAPE '|'",
        ),
        // Null-safe equality across type shapes and against columns.
        ordered(
            "null-safe matrix",
            "SELECT 0 <=> 0, 0 <=> NULL, '' <=> '', 'a' <=> 'A', \
                    NULL <=> '', 1.5 <=> 1.5, DATE('2025-01-01') <=> DATE('2025-01-01')",
        ),
        ordered(
            "null-safe matrix",
            "SELECT COUNT(*) FROM events WHERE NOT (note <=> NULL)",
        ),
        // COLLATE in every comparing position.
        ordered(
            "collate positions",
            "SELECT COUNT(*) FROM events WHERE note = 'ALPHA' COLLATE utf8mb4_bin",
        ),
        ordered(
            "collate positions",
            "SELECT note FROM events WHERE note IS NOT NULL \
             ORDER BY note COLLATE utf8mb4_bin, id",
        ),
        unordered(
            "collate positions",
            "SELECT DISTINCT note COLLATE utf8mb4_bin FROM events WHERE note IS NOT NULL",
        ),
        // Unsigned wrap family: strings, decimals, negatives, SIGNED back.
        ordered(
            "unsigned wrap",
            "SELECT CAST(-2 AS UNSIGNED), CAST('-3' AS UNSIGNED) + 0, \
                    CAST(18446744073709551615 AS UNSIGNED), \
                    CAST(CAST(-1 AS UNSIGNED) AS SIGNED)",
        ),
        // JSON-to-JSON comparison: MySQL's type ladder, numbers numeric
        // across integer/double spellings, objects equal whatever the member
        // order. Unequal-object ORDERING is unspecified in MySQL, so no case
        // depends on it.
        ordered(
            "json comparison",
            "SELECT JSON_EXTRACT('{\"a\":1}','$.a') = JSON_EXTRACT('{\"b\":1.0}','$.b'), \
                    JSON_EXTRACT('{\"a\":1,\"b\":2}','$') = JSON_EXTRACT('{\"b\":2,\"a\":1}','$'), \
                    JSON_EXTRACT('[1,2]','$') = JSON_EXTRACT('[1,2]','$'), \
                    JSON_EXTRACT('[1,2]','$') < JSON_EXTRACT('[1,3]','$')",
        ),
        ordered(
            "json comparison",
            "SELECT JSON_EXTRACT('[9]','$') > JSON_EXTRACT('{\"n\":99}','$.n'), \
                    JSON_EXTRACT('true','$') > JSON_EXTRACT('[9]','$'), \
                    JSON_EXTRACT('null','$') < JSON_EXTRACT('0','$'), \
                    JSON_EXTRACT('\"a\"','$') > JSON_EXTRACT('9','$'), \
                    JSON_EXTRACT('false','$') < JSON_EXTRACT('true','$')",
        ),
        ordered(
            "json comparison",
            "SELECT COUNT(*) FROM events \
             WHERE JSON_EXTRACT(CONCAT('{\"n\":', id, '}'), '$.n') \
                   IN (JSON_EXTRACT('[1,3]','$[0]'), JSON_EXTRACT('[1,3]','$[1]'))",
        ),
        ordered(
            "json comparison",
            "SELECT id FROM events \
             ORDER BY JSON_EXTRACT(CONCAT('{\"n\":', id * 7 % 5, '}'), '$.n'), id",
        ),
        unordered(
            "json comparison",
            "SELECT JSON_EXTRACT(CONCAT('{\"n\":', id % 3, '}'), '$.n') AS k, COUNT(*) \
             FROM events GROUP BY JSON_EXTRACT(CONCAT('{\"n\":', id % 3, '}'), '$.n')",
        ),
        ordered(
            "json comparison",
            "SELECT COUNT(DISTINCT JSON_EXTRACT(CONCAT('{\"n\":', id % 3, '}'), '$.n')) \
             FROM events",
        ),
        // JSON path wildcards, recursive descent, ranges, last-relative
        // indexes, and the non-array autowrap rules.
        ordered(
            "json path wildcards",
            "SELECT JSON_EXTRACT('{\"b\":2,\"aa\":1}','$.*'), \
                    JSON_EXTRACT('[1,2,3]','$[*]'), \
                    JSON_EXTRACT('{\"a\":{\"b\":1},\"c\":{\"b\":2}}','$**.b'), \
                    JSON_EXTRACT('[1,2,3,4]','$[1 to 2]'), \
                    JSON_EXTRACT('[1,2,3,4]','$[last-2 to last]')",
        ),
        ordered(
            "json path wildcards",
            "SELECT JSON_EXTRACT('[1,2,3]','$[last]'), \
                    JSON_EXTRACT('[1,2,3]','$[last-1]'), \
                    JSON_EXTRACT('3','$[0]'), \
                    JSON_EXTRACT('{\"a\":1}','$[*]'), \
                    JSON_EXTRACT('{}','$.*') IS NULL",
        ),
        ordered(
            "json path wildcards",
            "SELECT JSON_CONTAINS_PATH('{\"a\":{\"b\":1}}','one','$**.b'), \
                    JSON_CONTAINS_PATH('{\"a\":{\"b\":1}}','all','$**.b','$.a.*'), \
                    JSON_CONTAINS_PATH('{\"a\":1}','one','$**.zz'), \
                    JSON_EXTRACT('{\"a\":[{\"b\":1},{\"b\":2}]}','$.a[*].b')",
        ),
        // The JSON modification family and the remaining predicates.
        ordered(
            "json modification",
            "SELECT JSON_SET('{\"a\":1}','$.b',2), \
                    JSON_SET('{\"a\":1}','$.a','x'), \
                    JSON_INSERT('{\"a\":1}','$.a',9,'$.b',2), \
                    JSON_REPLACE('{\"a\":1}','$.a',9,'$.b',2), \
                    JSON_REMOVE('{\"a\":1,\"b\":2}','$.b'), \
                    JSON_REMOVE('[1,2,3]','$[1]'), \
                    JSON_SET('[1,2]','$[5]',3), \
                    JSON_SET('1','$[1]',2)",
        ),
        ordered(
            "json modification",
            "SELECT JSON_MERGE_PATCH('{\"a\":1,\"b\":2}','{\"b\":null,\"c\":3}'), \
                    JSON_MERGE_PATCH('{\"a\":{\"x\":1}}','{\"a\":{\"y\":2}}'), \
                    JSON_MERGE_PATCH('{\"a\":1}','[1]'), \
                    JSON_SET('{}','$.s','[1]'), \
                    JSON_SET('{}','$.j',JSON_EXTRACT('[1]','$'))",
        ),
        ordered(
            "json modification",
            "SELECT JSON_DEPTH('{}'), JSON_DEPTH('[1,[2,3]]'), \
                    JSON_QUOTE('a\"b'), JSON_PRETTY('{\"b\":[1,{}],\"a\":2}'), \
                    JSON_OVERLAPS('[1,2]','[2,9]'), JSON_OVERLAPS('[1,2]','[8,9]'), \
                    JSON_OVERLAPS('{\"a\":1,\"b\":2}','{\"a\":9,\"b\":2}'), \
                    1 MEMBER OF('[1.0, 2]'), 'x' MEMBER OF('[\"x\"]'), \
                    3 MEMBER OF('[1,2]')",
        ),
        // Hash and network scalar batch (UUID is volatile and excluded:
        // nothing byte-exact can hold for it).
        ordered(
            "hash and net scalars",
            "SELECT SHA1(''), SHA1('abc'), SHA2('abc', 256), SHA2('abc', 224), \
                    SHA2('abc', 384), SHA2('abc', 512), SHA2('abc', 0), \
                    SHA2('abc', 7), MD5('abc'), CRC32('MySQL'), CRC32('')",
        ),
        ordered(
            "hash and net scalars",
            "SELECT BIN(12), BIN(-1), BIN(0), OCT(64), OCT(-1), \
                    INET_ATON('10.0.5.9'), INET_ATON('255.255.255.255'), \
                    INET_ATON('10.0.5.256'), INET_ATON('1.2.3'), \
                    INET_NTOA(167773449), INET_NTOA(0), INET_NTOA(4294967296)",
        ),
        ordered(
            "hash and net scalars",
            "SELECT SHA1(note), CRC32(note) FROM events WHERE id = 1",
        ),
        // Composite EXTRACT units: concatenated decimal per MySQL.
        ordered(
            "extract composite units",
            "SELECT EXTRACT(YEAR_MONTH FROM '2025-07-21 10:40:50'), \
                    EXTRACT(DAY_HOUR FROM '2025-07-21 10:40:50'), \
                    EXTRACT(DAY_MINUTE FROM '2025-07-21 10:40:50'), \
                    EXTRACT(DAY_SECOND FROM '2025-07-21 10:40:50'), \
                    EXTRACT(HOUR_MINUTE FROM '2025-07-21 10:40:50'), \
                    EXTRACT(HOUR_SECOND FROM '2025-07-21 10:40:50'), \
                    EXTRACT(MINUTE_SECOND FROM '2025-07-21 10:40:50'), \
                    EXTRACT(YEAR_MONTH FROM '2025-01-05'), \
                    EXTRACT(MINUTE_SECOND FROM '00:00:07')",
        ),
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
            "hand-written parenthesized inner join",
            "SELECT e.id, e.name, u.name FROM \
             (events e INNER JOIN users u ON e.id = u.id) ORDER BY e.id",
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
            "hand-written scalar subquery unique lookup",
            "SELECT e.id, (SELECT u.name FROM users u WHERE u.id = e.id) AS user_name \
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
            "hand-written hashes",
            "SELECT MD5(''), MD5('abc'), MD5(42), MD5(NULL)",
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
             CAST('2024-02-29 12:34:56.789' AS DATETIME(3)), \
             CAST('12:34:56.7896' AS TIME(3)), \
             CAST('-12:34:56.123456' AS TIME(6)), \
             CAST('1 02:03:04' AS TIME), CAST('1112' AS TIME), \
             CAST('2026-08-06 07:08:09.987654' AS TIME(3)), \
             CAST('850:00:00' AS TIME)",
        ),
        ordered(
            "hand-written cast temporal invalid",
            "SELECT CAST('not-a-date' AS DATE), CAST('' AS DATE), \
             CAST('2024-13-45' AS DATE)",
        ),
        ordered(
            "hand-written cast targets",
            "SELECT CAST(42 AS CHAR), CAST('42' AS SIGNED), CAST('42' AS UNSIGNED), \
             CAST(-1 AS SIGNED), CAST('3.7' AS DECIMAL(10,2)), CAST(1 AS CHAR(4)), \
             CAST('{\"aa\":1,\"b\":[true,null]}' AS JSON), \
             JSON_TYPE(CAST('[1,2]' AS JSON)), \
             CAST(0 AS YEAR), CAST('0' AS YEAR), CAST(69 AS YEAR), \
             CAST(70 AS YEAR), CAST(1944.5 AS YEAR), CAST(2156 AS YEAR), \
             CAST(CAST('2024-02-29' AS DATE) AS YEAR), CAST('11:35:00' AS YEAR), \
             CAST('1979aaa' AS YEAR), CAST('not-a-year' AS YEAR)",
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
        // A named window must resolve to exactly what the inline form means.
        // An explicit RANGE frame with offsetless bounds differs from ROWS
        // only in treating CURRENT ROW as the whole peer group. The tied
        // ORDER BY key is what distinguishes the two readings.
        ordered(
            "hand-written window range frames",
            "SELECT id, \
             SUM(score) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
             SUM(score) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING \
             AND UNBOUNDED FOLLOWING) FROM events ORDER BY id",
        ),
        ordered(
            "hand-written window range peer groups",
            "SELECT id, active, \
             SUM(score) OVER (ORDER BY active RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
             SUM(score) OVER (ORDER BY active RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written window numeric range offsets",
            "SELECT id, score, \
             SUM(score) OVER (ORDER BY score RANGE BETWEEN 10 PRECEDING AND CURRENT ROW), \
             COUNT(*) OVER (ORDER BY score DESC RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING), \
             SUM(score) OVER (ORDER BY score RANGE BETWEEN 0.5 PRECEDING AND CURRENT ROW) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written window temporal range offsets",
            "SELECT id, COUNT(*) OVER (ORDER BY \
             CAST(CONCAT('2024-01-', LPAD(id, 2, '0')) AS DATE) \
             RANGE BETWEEN INTERVAL 2 DAY PRECEDING AND CURRENT ROW) \
             FROM events ORDER BY id",
        ),
        ordered(
            "hand-written named windows",
            "SELECT id, SUM(score) OVER w, ROW_NUMBER() OVER w FROM events \
             WINDOW w AS (ORDER BY id) ORDER BY id",
        ),
        ordered(
            "hand-written named windows with frame",
            "SELECT id, SUM(score) OVER w FROM events \
             WINDOW w AS (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) ORDER BY id",
        ),
        ordered(
            "hand-written named windows partitioned",
            "SELECT id, active, SUM(score) OVER w, LAG(score) OVER w FROM events \
             WINDOW w AS (PARTITION BY active ORDER BY id) ORDER BY id",
        ),
        ordered(
            "hand-written named window extension",
            "SELECT id, active, SUM(score) OVER (w ORDER BY id \
             ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM events \
             WINDOW w AS (PARTITION BY active) ORDER BY id",
        ),
        ordered(
            "hand-written chained named windows",
            "SELECT id, active, SUM(score) OVER rolling FROM events \
             WINDOW base AS (PARTITION BY active), \
             ordered AS (base ORDER BY id), \
             rolling AS (ordered ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
             ORDER BY id",
        ),
        // The isolation that found this: CHAR and CONCAT both agreed with
        // MySQL, so the decoder's whitespace set was the only suspect left.
        ordered(
            "repaired from_base64 whitespace set",
            "SELECT HEX(CHAR(11)), HEX(CONCAT('YQ==', CHAR(11))), \
             HEX(FROM_BASE64('YQ==')), HEX(FROM_BASE64(CONCAT('YQ==', CHAR(11)))), \
             HEX(FROM_BASE64('YQ== ')), HEX(FROM_BASE64(CONCAT('YQ==', CHAR(9))))",
        ),
        ordered(
            "repaired exact literal rounding",
            "SELECT ROUND(1.005, 2), ROUND(25E-1), ROUND(2.5), ROUND(-2.5), \
             ROUND(1.005E0, 2), CEIL(1.2), FLOOR(-1.2), 1 + 2.5, \
             ROUND(12.345, 2), TRUNCATE(1.999, 2)",
        ),
        // Defects a robustness review found in this batch, each adjudicated
        // rather than assumed. The two inferred claims are here too: CONV's
        // saturation on overflow, and JSON_KEYS's key ordering.
        ordered(
            "reviewed conv edges",
            "SELECT CONV('10000000000000000', 16, 10), CONV('ff', 16, 10), \
             CONV('zz', 36, 10), CONV('-17', 10, -18), CONV('1', 1, 10), CONV('1', 37, 10)",
        ),
        ordered(
            "reviewed maketime fraction",
            "SELECT MAKETIME(12, 15, 30.5), MAKETIME(12, 15, 30), MAKETIME(1, 60, 0)",
        ),
        // JSON_SEARCH: MySQL answers NULL for no match, a bare path for one
        // hit under 'one', and an array once several match under 'all'.
        ordered(
            "json search",
            "SELECT JSON_SEARCH('{\"a\":\"x\",\"b\":\"y\"}', 'one', 'x'), \
             JSON_SEARCH('{\"a\":\"x\",\"b\":\"x\"}', 'all', 'x'), \
             JSON_SEARCH('{\"a\":\"x\"}', 'one', 'zzz'), \
             JSON_SEARCH('[\"abc\",\"abd\"]', 'all', 'ab%'), \
             JSON_SEARCH('{\"a\":{\"b\":\"deep\"}}', 'one', 'deep'), \
             JSON_SEARCH('{\"a\":1}', 'one', '1')",
        ),
        ordered(
            "json value",
            "SELECT JSON_VALUE('{\"a\":\"x\"}', '$.a'), \
             JSON_VALUE('{\"a\":42}', '$.a'), \
             JSON_VALUE('{\"a\":42}', '$.zz'), \
             JSON_VALUE('{\"a\":\"7\"}', '$.a' RETURNING SIGNED), \
             JSON_VALUE('{\"a\":[1,2]}', '$.a[1]')",
        ),
        ordered(
            "json objectagg",
            "SELECT JSON_OBJECTAGG(name, score) FROM events",
        ),
        ordered(
            "json objectagg grouped",
            "SELECT active, JSON_OBJECTAGG(name, score), JSON_ARRAYAGG(score) FROM events \
             GROUP BY active ORDER BY active",
        ),
        ordered(
            "reviewed json keys ordering",
            "SELECT JSON_KEYS('{\"aa\":1,\"b\":2}'), JSON_KEYS('{\"b\":1,\"aa\":2}'), \
             JSON_KEYS('{\"c\":1,\"a\":2,\"bb\":3}')",
        ),
        ordered(
            "reviewed frame on extremes",
            "SELECT id, \
             LAST_VALUE(score) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING \
             AND UNBOUNDED FOLLOWING), \
             FIRST_VALUE(score) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
             FROM events ORDER BY id",
        ),
        ordered(
            "reviewed named window in arguments",
            "SELECT id, COALESCE(LAG(score) OVER w, 0) FROM events \
             WINDOW w AS (ORDER BY id) ORDER BY id",
        ),
        ordered(
            "reviewed grouped statistical aggregates",
            "SELECT active, STDDEV_POP(score), VAR_POP(score), BIT_OR(id), ANY_VALUE(active) \
             FROM events GROUP BY active ORDER BY active",
        ),
        ordered(
            "repaired binary case sensitivity",
            "SELECT LOWER(CAST('ABC' AS BINARY)), UPPER(CAST('abc' AS BINARY)), \
             INSTR(CAST('A' AS BINARY), 'a'), LOCATE('a', CAST('A' AS BINARY)), \
             INSTR('A', 'a'), LOCATE('a', 'A'), INSTR(CAST('Aa' AS BINARY), 'a')",
        ),
        ordered(
            "repaired regexp unicode classes",
            "SELECT REGEXP_LIKE('\u{e9}', '[[:alpha:]]'), REGEXP_LIKE('a', '[[:alpha:]]'), \
             REGEXP_LIKE('1', '[[:alpha:]]'), REGEXP_LIKE('1', '[[:digit:]]')",
        ),
        ordered(
            "hand-written conditionals",
            "SELECT IF(NULL, 'yes', 'no'), COALESCE(NULL, 'first', 'second'), \
             NULLIF('Alpha', 'alpha'), \
             IF(1, CAST('1.25' AS DECIMAL(3,2)), 0), \
             CASE WHEN 0 THEN 0 ELSE CAST('2.50' AS DECIMAL(3,2)) END, \
             IFNULL(NULL, CAST('3.75' AS DECIMAL(3,2))), \
             COALESCE(NULL, CAST('4.50' AS DECIMAL(3,2)), 0), \
             NULLIF(CAST('9007199254740993' AS DECIMAL(16,0)), 9007199254740992), \
             NULLIF(CAST('9007199254740993' AS DECIMAL(16,0)), 9007199254740993)",
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
            "SELECT COUNT(note), COUNT(DISTINCT note), COUNT(DISTINCT active, note), \
             GROUP_CONCAT(note), \
             GROUP_CONCAT(id, ':', name ORDER BY id SEPARATOR '|') FROM events",
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
            "SELECT MIN(note), MAX(note), 'a' = 'a ', \
             'a' COLLATE utf8mb4_0900_ai_ci = 'A' FROM events",
        ),
        ordered(
            "collation unicode equality",
            "SELECT 'A' = 'a', 'É' = 'e', 'é' = 'e\u{301}', 'ß' = 'ss'",
        ),
        ordered(
            "collation locale-sensitive equality",
            "SELECT 'İ' = 'i', 'ı' = 'i', 'Æ' = 'ae', 'Œ' = 'oe'",
        ),
        ordered(
            "collation supplementary equality and order",
            "SELECT '😀' < '😁', '🪿' = '🪿', '𝔄' = 'A'",
        ),
        ordered(
            "collation no-pad and binary",
            "SELECT 'a' = 'a ', CAST('A' AS BINARY) = CAST('a' AS BINARY), \
             CAST('é' AS BINARY) = CAST('e' AS BINARY)",
        ),
        ordered(
            "collation explicit profile",
            "SELECT 'É' COLLATE utf8mb4_0900_ai_ci = \
             'e' COLLATE utf8mb4_0900_ai_ci",
        ),
        ordered(
            "collation grouping",
            "SELECT value, COUNT(*) FROM \
             (SELECT 'É' AS value UNION ALL SELECT 'e' UNION ALL SELECT 'z') c \
             GROUP BY value ORDER BY value",
        ),
        ordered(
            "collation distinct",
            "SELECT COUNT(DISTINCT value) FROM \
             (SELECT 'É' AS value UNION ALL SELECT 'e' UNION ALL SELECT 'z') c",
        ),
        ordered(
            "collation join",
            "SELECT COUNT(*) FROM (SELECT 'É' AS value) l \
             JOIN (SELECT 'e' AS value) r ON l.value = r.value",
        ),
        ordered(
            "collation membership",
            "SELECT 'É' IN ('e', 'z'), 'ß' IN ('ss'), 'x' IN ('y')",
        ),
        ordered(
            "collation pattern matching",
            "SELECT 'Éclair' LIKE 'e%', LOCATE('SS', 'straße'), \
             'straße' LIKE 'stra_e'",
        ),
        ordered(
            "collation extrema",
            "SELECT MIN(value), MAX(value) FROM \
             (SELECT 'Zulu' AS value UNION ALL SELECT 'alpha') c",
        ),
        ordered(
            "collation ordering",
            "SELECT value FROM \
             (SELECT 'éclair' AS value UNION ALL SELECT 'Elephant' UNION ALL SELECT 'zebra') c \
             ORDER BY value",
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
            "SELECT score / 0, 100 / 7, 10 / 4 FROM events WHERE id = 1",
        ),
        ordered(
            "decimal division intermediates",
            "SELECT (14620 / 9432456) / (24250 / 9432456), \
             (1 / 3) * 3, 1 / 3 / 3",
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
        ordered(
            "bushy outer join",
            "SELECT e.id, u.name, marker.name FROM events e \
             LEFT JOIN (users u LEFT JOIN \
               (SELECT 1 AS id, 'flag' AS name) marker ON marker.id = u.id) \
             ON u.id = e.id ORDER BY e.id",
        ),
        ordered(
            "join predicate subquery",
            "SELECT e.id, u.name FROM events e LEFT JOIN users u \
             ON u.id = e.id AND EXISTS \
                (SELECT 1 FROM users probe WHERE probe.id > u.id) \
             ORDER BY e.id",
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
        ordered(
            "set operation scoping",
            "SELECT 1 AS n UNION SELECT 1 UNION ALL SELECT 1 ORDER BY n",
        ),
        ordered(
            "set operation scoping",
            "(SELECT 2 AS n ORDER BY n DESC LIMIT 1) \
             UNION ALL SELECT 9 AS n ORDER BY n",
        ),
        ordered(
            "set operation precedence",
            "SELECT 1 AS n EXCEPT SELECT 1 UNION ALL SELECT 2 ORDER BY n",
        ),
        ordered(
            "set operation precedence",
            "SELECT 1 AS n UNION ALL SELECT 2 INTERSECT SELECT 2 ORDER BY n",
        ),
        ordered(
            "set operation scoping",
            "SELECT 1 AS n UNION ALL (SELECT 2 UNION SELECT 2) ORDER BY n",
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
        ordered(
            "regexp match type",
            "SELECT REGEXP_LIKE('Abc', 'abc', 'c'), REGEXP_LIKE('Abc', 'abc', 'ci'), \
             REGEXP_LIKE(CONCAT('a', CONVERT(CHAR(10) USING utf8mb4), 'b'), '^b$', 'm'), \
             REGEXP_LIKE(CONCAT('a', CONVERT(CHAR(10) USING utf8mb4), 'b'), 'a.b', 'n')",
        ),
        ordered(
            "json extract multiple paths",
            "SELECT JSON_EXTRACT('{\"a\":1,\"b\":\"x\"}', '$.a', '$.b')",
        ),
        ordered(
            "json typed constructor input",
            "SELECT JSON_OBJECT('json', JSON_EXTRACT('{\"x\":1}', '$'), \
             'text', '{\"x\":1}'), JSON_ARRAY(JSON_EXTRACT('{\"x\":1}', '$'), '{\"x\":1}')",
        ),
        ordered(
            "json typed aggregate input",
            "SELECT id, JSON_ARRAYAGG(JSON_EXTRACT('{\"x\":1}', '$')) \
             FROM events GROUP BY id ORDER BY id",
        ),
        ordered(
            "json null distinction",
            "SELECT JSON_EXTRACT('null', '$'), JSON_EXTRACT(NULL, '$'), \
             JSON_ARRAY(JSON_EXTRACT('null', '$'), NULL)",
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
            "dependent correlated subqueries",
            "SELECT e.id, (SELECT u.name FROM users u WHERE u.id >= e.id \
             ORDER BY u.id LIMIT 1) FROM events e ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id FROM events e WHERE EXISTS \
             (SELECT 1 FROM users u WHERE u.id > e.id) ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id FROM events e WHERE e.id + 1 IN \
             (SELECT u.id FROM users u WHERE u.id > e.id) ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, COUNT(*) FROM events e GROUP BY e.id HAVING EXISTS \
             (SELECT 1 FROM users u WHERE u.id > e.id) ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, (WITH candidates AS (SELECT id, name FROM users) \
             SELECT name FROM candidates WHERE candidates.id >= e.id \
             ORDER BY candidates.id LIMIT 1) FROM events e ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, (SELECT (SELECT u2.name FROM users u2 \
             WHERE u2.id >= u1.id AND u2.id >= e.id ORDER BY u2.id LIMIT 1) \
             FROM users u1 WHERE u1.id = e.id) FROM events e ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, (SELECT (SELECT e.name FROM users e WHERE e.id = u.id) \
             FROM users u WHERE u.id = e.id) FROM events e ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, IF(e.id > 0, 'chosen', \
             (SELECT u.name FROM users u WHERE u.id >= e.id)) \
             FROM events e ORDER BY e.id",
        ),
        ordered(
            "dependent correlated subqueries",
            "SELECT e.id, COALESCE('chosen', \
             (SELECT u.name FROM users u WHERE u.id >= e.id)) \
             FROM events e ORDER BY e.id",
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
            "datetime week modes",
            "SELECT WEEK('2008-02-20', 0), WEEK('2008-02-20', 1), \
             WEEK('2008-02-20', 2), WEEK('2008-02-20', 3), \
             WEEK('2008-02-20', 4), WEEK('2008-02-20', 5), \
             WEEK('2008-02-20', 6), WEEK('2008-02-20', 7)",
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
            "decimal exact comparison",
            "SELECT CAST('9007199254740993' AS DECIMAL(16,0)) > \
             CAST('9007199254740992' AS DECIMAL(16,0)), \
             CAST('1.00' AS DECIMAL(3,2)) = CAST('1.0' AS DECIMAL(2,1)), \
             CAST('9007199254740993' AS DECIMAL(16,0)) > 9007199254740992",
        ),
        ordered(
            "decimal exact set and modulo",
            "SELECT CAST('9007199254740993' AS DECIMAL(16,0)) \
                 IN (9007199254740992), \
             CAST('9007199254740993' AS DECIMAL(16,0)) % 2, \
             CAST('12.50' AS DECIMAL(4,2)) % CAST('0.70' AS DECIMAL(3,2)), \
             CAST('12.50' AS DECIMAL(4,2)) % 0",
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
            "decimal mixed precision boundaries",
            "SELECT CAST('999.99' AS DECIMAL(5,2)) + CAST('0.01' AS DECIMAL(3,2)), \
             CAST('-5.50' AS DECIMAL(3,2)) + CAST('2.125' AS DECIMAL(4,3)), \
             CAST('-12.34' AS DECIMAL(4,2)) * CAST('1.50' AS DECIMAL(3,2)), \
             CAST('12.34' AS DECIMAL(5,2)) / CAST('1.234' AS DECIMAL(4,3))",
        ),
        ordered(
            "decimal exact grouping distinct and extremes",
            "SELECT CAST(active AS DECIMAL(20,0)) + \
                    CAST('9007199254740992' AS DECIMAL(20,0)) AS amount, \
                    COUNT(*), MIN(CAST(id AS DECIMAL(20,0)) + \
                    CAST('9007199254740992' AS DECIMAL(20,0))), \
                    MAX(CAST(id AS DECIMAL(20,0)) + \
                    CAST('9007199254740992' AS DECIMAL(20,0))) \
             FROM events GROUP BY amount ORDER BY amount",
        ),
        ordered(
            "decimal exact distinct ordering",
            "SELECT DISTINCT CAST(active AS DECIMAL(20,0)) + \
                    CAST('9007199254740992' AS DECIMAL(20,0)) AS amount \
             FROM events ORDER BY amount",
        ),
        ordered(
            "decimal mixed comparison and join keys",
            "SELECT e.id, u.id, \
                    CAST('9007199254740993' AS DECIMAL(16,0)) = \
                        CAST('9007199254740993' AS CHAR), \
                    CAST('9007199254740993' AS DECIMAL(16,0)) = 9007199254740992e0 \
             FROM events e JOIN users u ON \
                  CAST(e.id AS DECIMAL(20,0)) + \
                      CAST('9007199254740992' AS DECIMAL(20,0)) = \
                  CAST(u.id AS DECIMAL(21,1)) + \
                      CAST('9007199254740992.0' AS DECIMAL(21,1)) \
             ORDER BY e.id",
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
    .into_iter()
    .chain(diversify_cases())
    .collect()
}

/// Typed multi-table cases on the `orders` seed — DECIMAL / DATETIME / JSON
/// columns and joins against `users`. These raise template entropy without
/// another parametric 0..90 loop.
#[allow(clippy::too_many_lines)]
fn diversify_cases() -> Vec<OracleCase> {
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
        ordered(
            "diversify decimal column aggregates",
            "SELECT COUNT(*), SUM(total), AVG(total), MIN(total), MAX(total), \
             ROUND(AVG(total), 2) FROM orders",
        ),
        ordered(
            "diversify decimal column aggregates",
            "SELECT status, COUNT(*), SUM(total), ROUND(AVG(total), 2) \
             FROM orders GROUP BY status HAVING COUNT(*) >= 1 ORDER BY status",
        ),
        ordered(
            "diversify decimal column math",
            "SELECT id, total, total * 2, total + 0.50, ROUND(total / 3, 4), \
             TRUNCATE(total, 1) FROM orders WHERE id <= 6 ORDER BY id",
        ),
        ordered(
            "diversify decimal beyond float64",
            "SELECT id, total FROM orders \
             WHERE total = CAST('99999999.99' AS DECIMAL(12,2)) ORDER BY id",
        ),
        ordered(
            "diversify decimal join key",
            "SELECT o.id, u.name, o.total FROM orders o \
             JOIN users u ON o.user_id = u.id \
             WHERE o.total > CAST('20.00' AS DECIMAL(12,2)) \
             ORDER BY o.total DESC, o.id",
        ),
        ordered(
            "diversify outer join nullability",
            "SELECT u.id, u.name, COUNT(o.id) AS n, \
             COALESCE(SUM(o.total), 0) AS spend \
             FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             GROUP BY u.id, u.name ORDER BY spend DESC, u.id",
        ),
        ordered(
            "diversify outer join unmatched orders",
            "SELECT o.id, o.user_id, u.name FROM orders o \
             LEFT JOIN users u ON u.id = o.user_id \
             WHERE u.id IS NULL ORDER BY o.id",
        ),
        ordered(
            "diversify temporal column",
            "SELECT id, YEAR(placed_at), MONTH(placed_at), DAY(placed_at), \
             DATE(placed_at), DATE_FORMAT(placed_at, '%Y-%m-%d %H:%i:%s') \
             FROM orders ORDER BY id",
        ),
        ordered(
            "diversify temporal bucketing",
            "SELECT YEAR(placed_at) AS yr, MONTH(placed_at) AS mo, COUNT(*), SUM(total) \
             FROM orders GROUP BY yr, mo ORDER BY yr, mo",
        ),
        ordered(
            "diversify convert_tz on column",
            "SELECT id, CONVERT_TZ(placed_at, '+00:00', '+05:30'), \
             CONVERT_TZ(placed_at, 'UTC', 'America/New_York') \
             FROM orders WHERE id IN (1, 2, 7) ORDER BY id",
        ),
        ordered(
            "diversify json column extract",
            "SELECT id, JSON_EXTRACT(meta, '$.score'), JSON_UNQUOTE(JSON_EXTRACT(meta, '$.tags[0]')), \
             meta -> '$.score', meta ->> '$.tags[0]' \
             FROM orders WHERE meta IS NOT NULL ORDER BY id",
        ),
        ordered(
            "diversify json contains and length",
            "SELECT id, JSON_CONTAINS(meta, '\"premium\"', '$.tags'), \
             JSON_LENGTH(meta, '$.items'), JSON_TYPE(JSON_EXTRACT(meta, '$.score')) \
             FROM orders WHERE meta IS NOT NULL ORDER BY id",
        ),
        ordered(
            "diversify json null rows",
            "SELECT id, meta IS NULL, COALESCE(JSON_LENGTH(meta), -1) \
             FROM orders ORDER BY id",
        ),
        ordered(
            "diversify window on decimal",
            "SELECT id, status, total, \
             ROUND(SUM(total) OVER (PARTITION BY status ORDER BY id), 2) AS running, \
             ROW_NUMBER() OVER (PARTITION BY status ORDER BY total DESC, id) AS rank_in_status \
             FROM orders ORDER BY status, rank_in_status, id",
        ),
        ordered(
            "diversify correlated scalar on orders",
            "SELECT u.id, u.name, \
             (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS n, \
             (SELECT COALESCE(SUM(o.total), 0) FROM orders o WHERE o.user_id = u.id) AS spend \
             FROM users u ORDER BY u.id",
        ),
        ordered(
            "diversify correlated exists",
            "SELECT u.id, u.name FROM users u \
             WHERE EXISTS (SELECT 1 FROM orders o \
                           WHERE o.user_id = u.id AND o.status = 'shipped') \
             ORDER BY u.id",
        ),
        ordered(
            "diversify in subquery",
            "SELECT id, name FROM users \
             WHERE id IN (SELECT user_id FROM orders WHERE total > 50) \
             ORDER BY id",
        ),
        ordered(
            "diversify set ops with columns",
            "SELECT user_id AS id FROM orders WHERE status = 'shipped' \
             UNION \
             SELECT id FROM users WHERE id <= 3 \
             ORDER BY id",
        ),
        ordered(
            "diversify set ops except",
            "SELECT id FROM users \
             EXCEPT \
             SELECT user_id FROM orders \
             ORDER BY id",
        ),
        ordered(
            "diversify three table join",
            "SELECT o.id, u.name, e.name, o.total \
             FROM orders o \
             JOIN users u ON o.user_id = u.id \
             JOIN events e ON e.id = u.id \
             WHERE o.status <> 'cancelled' \
             ORDER BY o.id",
        ),
        ordered(
            "diversify case on status and decimal",
            "SELECT CASE WHEN total >= 100 THEN 'high' \
                         WHEN total >= 20 THEN 'mid' ELSE 'low' END AS bucket, \
                    COUNT(*), SUM(total) \
             FROM orders GROUP BY bucket ORDER BY bucket",
        ),
        ordered(
            "diversify group_concat statuses",
            "SELECT user_id, GROUP_CONCAT(status ORDER BY id) AS statuses, \
             COUNT(*) AS n FROM orders GROUP BY user_id \
             HAVING COUNT(*) >= 2 ORDER BY user_id",
        ),
        ordered(
            "diversify filter null json and temporal range",
            "SELECT id, total, placed_at FROM orders \
             WHERE meta IS NULL AND placed_at >= '2024-07-01' \
             ORDER BY id",
        ),
        ordered(
            "diversify distinct status total pairs",
            "SELECT DISTINCT status, ROUND(total, 0) AS whole \
             FROM orders ORDER BY status, whole",
        ),
        ordered(
            "diversify decimal average by user",
            "SELECT user_id, AVG(total), COUNT(*) FROM orders \
             GROUP BY user_id ORDER BY user_id",
        ),
        ordered(
            "diversify date_add on column",
            "SELECT id, placed_at, DATE_ADD(placed_at, INTERVAL 1 DAY), \
             DATE_SUB(placed_at, INTERVAL 1 MONTH) \
             FROM orders WHERE id <= 4 ORDER BY id",
        ),
        ordered(
            "diversify like and string on status",
            "SELECT id, UPPER(status), status LIKE '%ship%' \
             FROM orders ORDER BY id",
        ),
        ordered(
            "diversify multi-key style equality join",
            "SELECT o.id, u.id, o.status, u.name \
             FROM orders o JOIN users u \
               ON o.user_id = u.id AND u.name = CONCAT('user-0', u.id) \
             WHERE o.id <= 5 ORDER BY o.id",
        ),
        unordered(
            "diversify distinct user ids with orders",
            "SELECT DISTINCT user_id FROM orders",
        ),
        ordered(
            "diversify having on decimal sum",
            "SELECT user_id, SUM(total) AS spend FROM orders \
             GROUP BY user_id HAVING SUM(total) >= 50 ORDER BY spend DESC, user_id",
        ),
        ordered(
            "diversify order by decimal column",
            "SELECT id, total FROM orders ORDER BY total DESC, id LIMIT 5",
        ),
        ordered(
            "diversify coalesce meta length",
            "SELECT id, COALESCE(JSON_LENGTH(meta, '$.tags'), 0) AS tag_n \
             FROM orders ORDER BY id",
        ),
        ordered(
            "diversify between on decimal",
            "SELECT id, total FROM orders \
             WHERE total BETWEEN 10 AND 100 ORDER BY id",
        ),
        ordered(
            "diversify not in statuses",
            "SELECT id, status FROM orders \
             WHERE status NOT IN ('cancelled', 'pending') ORDER BY id",
        ),
        ordered(
            "diversify derived table on orders",
            "SELECT status, AVG(total) AS avg_total FROM ( \
               SELECT status, total FROM orders WHERE total > 0 \
             ) d GROUP BY status ORDER BY status",
        ),
        ordered(
            "diversify cte spend",
            "WITH spend AS ( \
               SELECT user_id, SUM(total) AS lifetime FROM orders GROUP BY user_id \
             ) \
             SELECT u.id, u.name, ROUND(s.lifetime, 2) AS lifetime \
             FROM users u JOIN spend s ON s.user_id = u.id \
             ORDER BY lifetime DESC, u.id",
        ),
        ordered(
            "diversify lag lead on totals",
            "SELECT id, total, \
             LAG(total, 1) OVER (ORDER BY id) AS prev_total, \
             LEAD(total, 1) OVER (ORDER BY id) AS next_total \
             FROM orders WHERE user_id = 1 ORDER BY id",
        ),
        ordered(
            "diversify json object constructor with column",
            "SELECT id, JSON_OBJECT('id', id, 'total', total, 'status', status) \
             FROM orders WHERE id <= 3 ORDER BY id",
        ),
        ordered(
            "diversify unsigned order id filter",
            "SELECT id, user_id, total FROM orders WHERE id >= 10 ORDER BY id",
        ),
        ordered(
            "diversify min max datetime",
            "SELECT MIN(placed_at), MAX(placed_at), COUNT(*) FROM orders",
        ),
    ]
}

#[test]
fn documented_rejects_stay_explicit() {
    let events_schema = events_schema().expect("events schema");
    let users_schema = users_schema().expect("users schema");
    let orders_schema = orders_schema().expect("orders schema");
    let events_directory = tempfile::tempdir().expect("events dir");
    let users_directory = tempfile::tempdir().expect("users dir");
    let orders_directory = tempfile::tempdir().expect("orders dir");
    let mut events = TableStore::open(
        events_directory.path(),
        events_schema.clone(),
        StoreOptions::default(),
    )
    .expect("open events");
    let mut users = TableStore::open(
        users_directory.path(),
        users_schema.clone(),
        StoreOptions::default(),
    )
    .expect("open users");
    let mut orders = TableStore::open(
        orders_directory.path(),
        orders_schema.clone(),
        StoreOptions::default(),
    )
    .expect("open orders");
    events
        .ingest((1..=10).map(event_row).collect())
        .expect("ingest events");
    users
        .ingest((1..=8).map(user_row).collect())
        .expect("ingest users");
    orders.ingest(order_rows()).expect("ingest orders");
    let events_snapshot = events.snapshot();
    let users_snapshot = users.snapshot();
    let orders_snapshot = orders.snapshot();
    let catalog = catalog(events_schema, users_schema, orders_schema).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (DATABASE_ID, EVENTS_ID, &events_snapshot),
        (DATABASE_ID, USERS_ID, &users_snapshot),
        (DATABASE_ID, ORDERS_ID, &orders_snapshot),
    ])
    .expect("provider");

    for (family, sql, needle) in reject_cases() {
        let error = match execute_pintail(sql, &catalog, &provider) {
            Ok(rows) => panic!(
                "{family}: expected reject for `{sql}`, got {} row(s)",
                rows.len()
            ),
            Err(error) => error,
        };
        let lower = error.to_ascii_lowercase();
        let ok = needle.is_empty()
            || needle
                .split('|')
                .any(|part| lower.contains(&part.to_ascii_lowercase()));
        assert!(
            ok,
            "{family}: error `{error}` missing needle `{needle}` for `{sql}`"
        );
    }
}

/// Shapes that must fail closed (explicit error), never return a plausible
/// wrong result. Needles are matched case-insensitively on the error string.
fn reject_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "reject non-equality join",
            "SELECT e.id, u.id FROM events e JOIN users u ON e.id > u.id",
            "join|equality|unsupported|bind",
        ),
        (
            "reject json table",
            "SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt",
            "unsupported|json_table|parse|bind|table",
        ),
        (
            "reject soundex",
            "SELECT SOUNDEX(name) FROM users",
            "unsupported",
        ),
        (
            "reject recursive with aggregate",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT SUM(n) FROM t) SELECT * FROM t",
            "recursive|unsupported|unknown|aggregate",
        ),
        (
            "reject json mixed compare",
            "SELECT id FROM orders WHERE meta = 'premium'",
            "json|=|binary|invalid",
        ),
        (
            "reject json arithmetic",
            "SELECT meta + 1 FROM orders WHERE meta IS NOT NULL",
            "json|\\+|binary|invalid",
        ),
        (
            "reject unknown collate",
            "SELECT name FROM users ORDER BY name COLLATE latin1_swedish_ci",
            "collat",
        ),
        (
            "reject trig function",
            "SELECT SIN(score) FROM events WHERE id = 1",
            "unsupported",
        ),
        (
            "reject full text match",
            "SELECT id FROM events WHERE MATCH(name) AGAINST ('event')",
            "unsupported|match|parse|bind",
        ),
        (
            "reject update statement",
            "UPDATE orders SET total = 0 WHERE id = 1",
            "unsupported|update|statement|parse|bind",
        ),
        (
            "reject compound year_month interval",
            "SELECT DATE_ADD(placed_at, INTERVAL '1-2' YEAR_MONTH) FROM orders",
            "unsupported|interval|year_month|parse|bind",
        ),
        (
            "reject aliased parenthesized join group",
            "SELECT * FROM (events e JOIN users u ON e.id = u.id) AS j",
            "unsupported|alias|join|bind|parse",
        ),
    ]
}

fn catalog(
    events_schema: TableSchema,
    users_schema: TableSchema,
    orders_schema: TableSchema,
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
    let orders = TableEntry::new(
        ORDERS_ID,
        "orders",
        orders_schema,
        TableStatistics::with_row_count(12),
    )
    .map_err(|error| error.to_string())?
    .with_key_columns([1])
    .map_err(|error| error.to_string())?;
    let database = DatabaseEntry::new(DATABASE_ID, "app", [events, users, orders])
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
            Column::new(6, "tag", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_general_ci".to_owned())),
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

fn orders_schema() -> Result<TableSchema, String> {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::Int64, false),
            Column::new(
                3,
                "total",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
            Column::new(4, "placed_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(5, "status", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned()))
                .with_enum_labels(Some(
                    ["pending", "processing", "shipped", "delivered", "cancelled"]
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                )),
            Column::new(6, "meta", DataType::Json, true),
        ],
    )
    .map_err(|error| error.to_string())
}

fn order_rows() -> Vec<StoredRow> {
    // Canonical carriers must match the MySQL INSERT seed above.
    [
        order_row(
            1,
            1,
            "10.50",
            "2024-01-15 10:00:00",
            "shipped",
            Some(r#"{"tags":["premium"],"score":1.5,"items":[1,2,3,4]}"#),
        ),
        order_row(
            2,
            1,
            "198.82",
            "2024-02-29 12:34:56",
            "shipped",
            Some(r#"{"tags":["bulk"],"score":2.0,"items":[1]}"#),
        ),
        order_row(3, 2, "0.01", "2024-03-01 00:00:00", "pending", None),
        order_row(
            4,
            2,
            "99999999.99",
            "2024-03-08 06:30:00",
            "cancelled",
            Some(r#"{"tags":[],"score":0,"items":[]}"#),
        ),
        order_row(
            5,
            3,
            "12.35",
            "2024-06-15 18:00:00",
            "shipped",
            Some(r#"{"tags":["premium","rush"],"score":9.9,"items":[1,2,3,4,5]}"#),
        ),
        order_row(
            6,
            3,
            "50.00",
            "2024-07-04 09:15:00",
            "pending",
            Some(r#"{"tags":["gift"],"score":3,"items":[7,8]}"#),
        ),
        order_row(7, 4, "7.00", "2024-11-01 01:30:00", "shipped", None),
        order_row(
            8,
            5,
            "100.00",
            "2025-01-01 00:00:00",
            "delivered",
            Some(r#"{"tags":["premium"],"score":1,"items":[1,2]}"#),
        ),
        order_row(
            9,
            9,
            "25.25",
            "2025-02-01 12:00:00",
            "pending",
            Some(r#"{"tags":["orphan"],"score":0.5,"items":[9]}"#),
        ),
        order_row(10, 1, "0.00", "2025-02-28 23:59:59", "cancelled", None),
        order_row(
            11,
            6,
            "33.33",
            "2025-03-01 08:00:00",
            "shipped",
            Some(r#"{"tags":["a"],"score":4.25,"items":[1,2,3]}"#),
        ),
        order_row(
            12,
            7,
            "64.00",
            "2025-04-01 16:45:00",
            "delivered",
            Some(r#"{"tags":["premium"],"score":8,"items":[2,4,6,8]}"#),
        ),
    ]
    .into()
}

fn order_row(
    id: u64,
    user_id: i64,
    total: &str,
    placed_at: &str,
    status: &str,
    meta: Option<&str>,
) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("non-empty order key"),
        vec![
            Value::UInt64(id),
            Value::Int64(user_id),
            Value::Utf8(total.to_owned()),
            Value::Utf8(placed_at.to_owned()),
            Value::Utf8(status.to_owned()),
            match meta {
                Some(document) => Value::Utf8(document.to_owned()),
                None => Value::Null,
            },
        ],
        id,
        false,
    )
}

fn event_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("non-empty event key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("event-{id:02}")),
            Value::Int64(i64::try_from(id * 10).expect("small seed score")),
            Value::Boolean(id.is_multiple_of(2)),
            match id {
                1 | 7 => Value::Utf8("Alpha".to_owned()),
                2 | 8 => Value::Utf8("alpha".to_owned()),
                4 | 10 => Value::Utf8("Beta".to_owned()),
                5 => Value::Utf8("beta".to_owned()),
                3 | 6 | 9 => Value::Null,
                _ => unreachable!("oracle event IDs are 1 through 10"),
            },
            Value::Utf8(
                match id {
                    1 => "red",
                    2 | 9 => "RED",
                    3 => "red ",
                    4 | 6 => "blue",
                    5 => "BLUE",
                    7 => "Green",
                    8 => "green",
                    10 => "Blue",
                    _ => unreachable!("oracle event IDs are 1 through 10"),
                }
                .to_owned(),
            ),
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
