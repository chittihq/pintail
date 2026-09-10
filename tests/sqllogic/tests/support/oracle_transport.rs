use super::{MysqlContainer, OracleCase, OracleValue, checked_output};
use mysql_async::{Conn, OptsBuilder, Row, Value, consts::ColumnType, prelude::Queryable};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write as _, path::Path, process::Command};

pub const DEFAULT_MODE: &str = "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION";

pub fn pinned_image(requested: &str) -> Result<String, String> {
    let inspect = || {
        checked_output(
            Command::new("docker").args([
                "image",
                "inspect",
                requested,
                "--format",
                "{{index .RepoDigests 0}}",
            ]),
            "resolve oracle image digest",
        )
    };
    let output = if let Ok(output) = inspect() {
        output
    } else {
        checked_output(
            Command::new("docker").args(["pull", requested]),
            "pull oracle image",
        )?;
        inspect()?
    };
    let digest = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    if !digest.contains("@sha256:") {
        return Err("oracle image has no immutable repository digest".into());
    }
    Ok(digest.trim().to_owned())
}

fn endpoint(mysql: &MysqlContainer) -> Result<(String, u16), String> {
    let output = checked_output(
        Command::new("docker").args(["port", &mysql.name, "3306/tcp"]),
        "find oracle port",
    )?;
    let ports = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let port = ports
        .lines()
        .next()
        .and_then(|s| s.rsplit(':').next())
        .ok_or("missing oracle port")?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    let endpoint = std::env::var("DOCKER_HOST").unwrap_or_else(|_| {
        Command::new("docker")
            .args([
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    });
    let host = if let Some(target) = endpoint.strip_prefix("ssh://") {
        let target = target
            .rsplit('@')
            .next()
            .unwrap_or(target)
            .trim_end_matches('/');
        let output = checked_output(
            Command::new("ssh").args(["-G", target]),
            "resolve Docker host",
        )?;
        String::from_utf8(output.stdout)
            .map_err(|e| e.to_string())?
            .lines()
            .find_map(|s| s.strip_prefix("hostname "))
            .ok_or("SSH configuration has no hostname")?
            .to_owned()
    } else {
        "127.0.0.1".to_owned()
    };
    Ok((host, port))
}

type CaseResult = Result<Vec<Vec<OracleValue>>, String>;

pub fn execute_all(
    mysql: &MysqlContainer,
    cases: &[OracleCase],
) -> Result<Vec<CaseResult>, String> {
    let (host, port) = endpoint(mysql)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let opts = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some("root"))
            .pass(Some(mysql.password.clone()))
            .db_name(Some("app"));
        let mut conn = Conn::new(opts).await.map_err(|e| e.to_string())?;
        conn.query_drop("SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci")
            .await
            .map_err(|e| e.to_string())?;
        conn.query_drop("SET time_zone='+00:00', SESSION max_execution_time=5000")
            .await
            .map_err(|e| e.to_string())?;
        let mut results = Vec::with_capacity(cases.len());
        for (index, case) in cases.iter().enumerate() {
            let mode = if case.sql_mode.is_empty() {
                DEFAULT_MODE
            } else {
                case.sql_mode
            };
            conn.query_drop(format!("SET sql_mode='{mode}'"))
                .await
                .map_err(|e| e.to_string())?;
            // A family whose shapes MySQL's default optimizer answers wrongly
            // runs under the switch that makes it answer the statement as written.
            let switch = super::mysql_optimizer_switch(case.family);
            if let Some(switch) = switch {
                conn.query_drop(format!("SET SESSION optimizer_switch='{switch}'"))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let rows: Result<Vec<Row>, String> = conn.query(&case.sql).await.map_err(|e| {
                format!(
                    "MySQL rejected case {index} ({}) `{}`: {e}",
                    case.family, case.sql
                )
            });
            if switch.is_some() {
                conn.query_drop("SET SESSION optimizer_switch='default'")
                    .await
                    .map_err(|e| e.to_string())?;
            }
            results.push(rows.and_then(|rows| rows.iter().map(row).collect::<Result<Vec<_>, _>>()));
        }
        conn.disconnect().await.map_err(|e| e.to_string())?;
        Ok(results)
    })
}

fn row(row: &Row) -> Result<Vec<OracleValue>, String> {
    (0..row.len())
        .map(|i| {
            let value = row.as_ref(i).ok_or("missing result cell")?;
            let column = &row.columns_ref()[i];
            let approximate = matches!(
                column.column_type(),
                ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE
            );
            let bytes = match value {
                Value::NULL => return Ok(OracleValue::Null),
                Value::Bytes(bytes) => bytes.clone(),
                Value::Int(n) => n.to_string().into_bytes(),
                Value::UInt(n) => n.to_string().into_bytes(),
                Value::Float(n) => n.to_string().into_bytes(),
                Value::Double(n) => n.to_string().into_bytes(),
                Value::Date(y, m, d, h, min, sec, micros) => {
                    let base = if column.column_type() == ColumnType::MYSQL_TYPE_DATE {
                        format!("{y:04}-{m:02}-{d:02}")
                    } else {
                        format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{sec:02}")
                    };
                    temporal(base, *micros, column.decimals()).into_bytes()
                }
                Value::Time(negative, days, h, m, sec, micros) => temporal(
                    format!(
                        "{}{:02}:{m:02}:{sec:02}",
                        if *negative { "-" } else { "" },
                        days * 24 + u32::from(*h)
                    ),
                    *micros,
                    column.decimals(),
                )
                .into_bytes(),
            };
            if approximate {
                Ok(OracleValue::Float(
                    String::from_utf8(bytes).map_err(|e| e.to_string())?,
                ))
            } else {
                match String::from_utf8(bytes) {
                    Ok(s) => Ok(OracleValue::Exact(s)),
                    Err(e) => Ok(OracleValue::Binary(e.into_bytes())),
                }
            }
        })
        .collect()
}

fn temporal(mut base: String, micros: u32, precision: u8) -> String {
    if precision > 0 && precision <= 6 {
        base.push('.');
        base.push_str(&format!("{micros:06}")[..usize::from(precision)]);
    }
    base
}

pub fn hash(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(result, "{byte:02x}").expect("write hash");
    }
    result
}

pub fn case_id(case: &OracleCase) -> String {
    hash(
        format!(
            "fixture-v1\0{}\0{}\0{}",
            case.sql_mode, case.ordered, case.sql
        )
        .as_bytes(),
    )
}

fn inventory(cases: &[OracleCase]) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1, "expectedCases": cases.len(),
        "fixtureSha256": hash(format!("{}{}", super::FIXTURE_SQL, super::oracle_boundaries::SQL).as_bytes()),
        "sourceSha256": hash(include_bytes!("../mysql_oracle.rs")),
        "sourceFiles": { "tests/sqllogic/tests/support/oracle_boundaries.rs": hash(include_bytes!("oracle_boundaries.rs")), "tests/sqllogic/tests/support/oracle_reviewed_cases.json": hash(include_bytes!("oracle_reviewed_cases.json")), "tests/sqllogic/tests/support/oracle_seed_cases.json": hash(include_bytes!("oracle_seed_cases.json")) },
        "fixtureSQL": format!("{}{}", super::FIXTURE_SQL, super::oracle_boundaries::SQL),
        "cases": cases.iter().map(|c| serde_json::json!({
            "id": case_id(c), "family": c.family, "sql": c.sql, "ordered": c.ordered,
            "fixture": "fixture-v1", "sqlMode": if c.sql_mode.is_empty() { DEFAULT_MODE } else { c.sql_mode },
            "timeZone": "+00:00", "collation": "utf8mb4_0900_ai_ci",
            "ledger": "docs/mysql-parity/ledger.json",
        })).collect::<Vec<_>>()
    })
}

fn write(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())
}

pub fn export_inventory(cases: &[OracleCase]) -> Result<(), String> {
    if let Ok(path) = std::env::var("PINTAIL_ORACLE_INVENTORY") {
        write(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(path),
            &inventory(cases),
        )?;
    }
    Ok(())
}

pub fn write_outcomes(
    mysql: &MysqlContainer,
    cases: &[OracleCase],
    outcomes: &[serde_json::Value],
) -> Result<(), String> {
    let path = std::env::var("PINTAIL_ORACLE_RUN_REPORT").unwrap_or_else(|_| {
        format!(
            "{}/../../validate-out/oracle-outcomes.json",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let mut families = BTreeMap::<&str, [usize; 2]>::new();
    for (case, outcome) in cases.iter().zip(outcomes) {
        families.entry(case.family).or_default()[usize::from(outcome["status"] != "PASS")] += 1;
    }
    let mut report = inventory(cases);
    report["provenance"] = provenance(mysql)?;
    report["outcomes"] = serde_json::json!(outcomes);
    report["familiesPassFail"] = serde_json::json!(families);
    report["image"] = serde_json::json!(mysql.image);
    report["version"] = serde_json::json!(mysql.query_batch("SELECT VERSION();")?.trim());
    report["verdict"] = serde_json::json!(if outcomes.iter().all(|o| o["status"] == "PASS") {
        "PASS"
    } else {
        "FAIL"
    });
    report["commit"] = serde_json::json!(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    );
    write(Path::new(&path), &report)
}

pub fn provenance(mysql: &MysqlContainer) -> Result<serde_json::Value, String> {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    Ok(serde_json::json!({
        "image": mysql.image,
        "version": mysql.query_batch("SELECT VERSION();")?.trim(),
        "session": {"sqlMode": DEFAULT_MODE, "timeZone": "+00:00", "collation": "utf8mb4_0900_ai_ci"},
        "fixtureSha256": hash(format!("{}{}", super::FIXTURE_SQL, super::oracle_boundaries::SQL).as_bytes()),
        "commit": git(&["rev-parse", "HEAD"]), "dirty": git(&["status", "--porcelain"]),
        "unixSeconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs()
    }))
}

pub fn execute(
    mysql: &MysqlContainer,
    cases: &[OracleCase],
) -> Result<Vec<Vec<Vec<OracleValue>>>, String> {
    execute_all(mysql, cases)?.into_iter().collect()
}
