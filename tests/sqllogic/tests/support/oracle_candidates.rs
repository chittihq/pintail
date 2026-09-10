use super::*;
use sqlparser::ast::{Expr, Query, SetExpr, Statement, Visit, Visitor};
use std::ops::ControlFlow;

struct ReadOnly {
    relations: BTreeSet<String>,
    queries: usize,
    references: usize,
    expressions: usize,
}
impl Visitor for ReadOnly {
    type Break = String;
    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<String> {
        if matches!(statement, Statement::Query(_)) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break("only SELECT statements are candidates".into())
        }
    }
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<String> {
        self.queries += 1;
        if self.queries > 4 || query.limit_clause.is_some() {
            return ControlFlow::Break("query nesting or LIMIT requires manual review".into());
        }
        if !query.locks.is_empty() {
            return ControlFlow::Break("locking reads are not candidates".into());
        }
        if let Some(with) = &query.with {
            if with.recursive {
                return ControlFlow::Break("recursive candidates are not bounded".into());
            }
            for cte in &with.cte_tables {
                self.relations.insert(cte.alias.name.value.to_lowercase());
            }
        }
        if let SetExpr::Select(select) = query.body.as_ref()
            && select.into.is_some()
        {
            return ControlFlow::Break("SELECT INTO is not read-only".into());
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, relation: &sqlparser::ast::ObjectName) -> ControlFlow<String> {
        self.references += 1;
        if self.references > 3 {
            return ControlFlow::Break("too many relation references".into());
        }
        if self
            .relations
            .contains(&relation.to_string().replace('`', "").to_lowercase())
        {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break("candidate references a relation outside synthetic fixtures".into())
        }
    }
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<String> {
        self.expressions += 1;
        if self.expressions > 160 {
            return ControlFlow::Break("expression budget exceeded".into());
        }
        if let Expr::Function(function) = expr {
            let name = function.name.to_string().to_uppercase();
            let allowed = "ABS ROUND TRUNCATE CEIL CEILING FLOOR SIGN MOD POWER POW SQRT GREATEST LEAST COALESCE IFNULL NULLIF IF COUNT SUM AVG MIN MAX RANK DENSE_RANK LOWER UPPER LCASE UCASE LENGTH CHAR_LENGTH CHARACTER_LENGTH SUBSTRING SUBSTR LEFT RIGHT TRIM LTRIM RTRIM CONCAT CONCAT_WS LOCATE INSTR REPLACE REVERSE LPAD RPAD HEX UNHEX OCT BIN DATE YEAR MONTH DAY DAYOFMONTH DAYOFYEAR HOUR MINUTE SECOND DATE_ADD DATE_SUB TIMESTAMPADD TIMESTAMPDIFF DATEDIFF LAST_DAY DATE_FORMAT TIME_TO_SEC SEC_TO_TIME ADDTIME SUBTIME TIMEDIFF JSON_EXTRACT JSON_UNQUOTE JSON_TYPE JSON_LENGTH JSON_CONTAINS JSON_CONTAINS_PATH JSON_OBJECT JSON_ARRAY JSON_KEYS JSON_VALID CAST CONVERT EXTRACT";
            if !allowed.split_whitespace().any(|a| a == name) {
                return ControlFlow::Break(format!(
                    "function {name} is outside the candidate allowlist"
                ));
            }
        }
        ControlFlow::Continue(())
    }
}

pub fn validate(sql: &str) -> Result<(), String> {
    if sql.to_uppercase().contains("INTO")
        || sql.len() > 4096
        || sql.contains(';')
        || sql.contains('@')
        || sql.contains(":=")
    {
        return Err("candidate must be one bounded statement without variables".into());
    }
    if sql.to_uppercase().contains("ROWS ") {
        let digest = oracle_transport::hash(sql.as_bytes());
        let reviewed = std::env::var("PINTAIL_REVIEWED_CANDIDATE_HASHES").unwrap_or_default();
        if !reviewed.split(',').any(|hash| hash == digest) {
            return Err("ROWS window requires explicit unique-order review".into());
        }
    }
    let statement = parse_statement(sql).map_err(|e| e.to_string())?;
    let mut visitor = ReadOnly {
        queries: 0,
        references: 0,
        expressions: 0,
        relations: ["bounds", "events", "orders", "users"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    match statement.visit(&mut visitor) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(e) => Err(e),
    }
}

pub fn reductions(sql: &str) -> Vec<String> {
    trusted_reductions(sql)
        .into_iter()
        .filter(|s| validate(s).is_ok())
        .collect()
}

pub fn trusted_reductions(sql: &str) -> Vec<String> {
    let Ok(Statement::Query(query)) = parse_statement(sql) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    if let SetExpr::Select(select) = query.body.as_ref() {
        if select.projection.len() > 1 {
            for index in 0..select.projection.len() {
                let mut candidate = query.clone();
                if let SetExpr::Select(select) = candidate.body.as_mut() {
                    select.projection.remove(index);
                }
                result.push(candidate.to_string());
            }
        }
        if select.selection.is_some() {
            let mut candidate = query.clone();
            if let SetExpr::Select(select) = candidate.body.as_mut() {
                select.selection = None;
            }
            result.push(candidate.to_string());
        }
    }
    result.into_iter().filter(|s| s.len() < sql.len()).collect()
}

#[allow(clippy::too_many_lines)]
pub fn run() -> Result<(), String> {
    let path = std::env::var("PINTAIL_CANDIDATES_PATH")
        .map_err(|_| "PINTAIL_CANDIDATES_PATH is required for the explicit candidate sweep")?;
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let fixture_hash =
        oracle_transport::hash(format!("{FIXTURE_SQL}{}", oracle_boundaries::SQL).as_bytes());
    if document["fixtureSha256"].as_str() != Some(fixture_hash.as_str()) {
        return Err("candidate fixture hash does not match executing fixtures".into());
    }
    let candidates = document["cases"]
        .as_array()
        .ok_or("candidate array missing")?;
    if candidates.len() > 100 {
        return Err("candidate sweep is capped at 100 proposals".into());
    }
    let mysql = MysqlContainer::start()?;
    mysql.query_batch(FIXTURE_SQL)?;
    mysql.query_batch(oracle_boundaries::SQL)?;
    let mut outcomes = Vec::new();
    let mut seen = oracle_cases()
        .iter()
        .map(|c| c.sql.clone())
        .collect::<BTreeSet<_>>();
    for candidate in candidates {
        let sql = candidate["sql"]
            .as_str()
            .ok_or("candidate SQL is not a string")?;
        if let Err(error) = validate(sql) {
            outcomes.push(serde_json::json!({"sql":sql,"status":"REJECTED","error":error}));
            continue;
        }
        if !seen.insert(sql.to_owned()) {
            outcomes.push(serde_json::json!({"sql":sql,"status":"DUPLICATE"}));
            continue;
        }
        let case = OracleCase {
            sql_mode: "",
            family: "generated-candidate",
            sql: sql.into(),
            ordered: false,
        };
        let expected = match execute_mysql_cases(&mysql, std::slice::from_ref(&case)) {
            Ok(rows) => rows,
            Err(error) => {
                outcomes
                    .push(serde_json::json!({"sql":sql,"status":"MYSQL_REJECTED","error":error}));
                continue;
            }
        };
        let actual = bounded_execute(sql);
        let pass = actual
            .as_ref()
            .is_ok_and(|r| oracle_rows_equal(r, &expected[0], false));
        let mut minimized = sql.to_owned();
        if !pass {
            for _ in 0..8 {
                let mut reduced = false;
                for sql in reductions(&minimized).into_iter().take(8) {
                    let candidate = OracleCase {
                        sql_mode: "",
                        family: "reduction",
                        sql: sql.clone(),
                        ordered: false,
                    };
                    let Ok(expected) = execute_mysql_cases(&mysql, &[candidate]) else {
                        continue;
                    };
                    let probe = bounded_execute(&sql);
                    // Keep value mismatches as value mismatches, and execution errors as errors.
                    let same_class = match (&probe, &actual) {
                        (Err(a), Err(b)) => a == b,
                        (Ok(_), Ok(_)) => true,
                        _ => false,
                    };
                    if same_class
                        && !probe
                            .as_ref()
                            .is_ok_and(|r| oracle_rows_equal(r, &expected[0], false))
                    {
                        minimized = sql;
                        reduced = true;
                        break;
                    }
                }
                if !reduced {
                    break;
                }
            }
        }
        outcomes.push(serde_json::json!({"sql":sql,"family":candidate["family"],"rationale":candidate["rationale"],"status":if pass {"PASS"} else {"FAIL"},"expected":expected[0],"actual":actual.as_ref().ok(),"error":actual.as_ref().err(),"minimizedSQL":minimized}));
    }
    let report = serde_json::json!({"source":document["source"],"provenance":oracle_transport::provenance(&mysql)?,"outcomes":outcomes});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../validate-out/candidates-validated.json");
    std::fs::write(
        path,
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let accepted = outcomes
        .iter()
        .filter(|o| o["status"] == "PASS" || o["status"] == "FAIL")
        .count();
    println!(
        "candidate sweep: {accepted}/{} MySQL-valid queries; see validate-out/candidates-validated.json",
        candidates.len()
    );
    if outcomes.iter().any(|o| o["status"] == "FAIL") {
        return Err("valid candidates expose parity failures".into());
    }
    Ok(())
}

pub fn worker() -> Result<(), String> {
    let sql = std::env::var("PINTAIL_CANDIDATE_SQL").map_err(|e| e.to_string())?;
    let mode = std::env::var("PINTAIL_CANDIDATE_MODE").unwrap_or_default();
    let result = pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(&mode), || {
        fixture_execute(&sql)
    });
    let output = std::env::var("PINTAIL_CANDIDATE_RESULT").map_err(|e| e.to_string())?;
    std::fs::write(
        output,
        serde_json::to_vec(&result).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn fixture_execute(sql: &str) -> Result<Vec<Vec<OracleValue>>, String> {
    let directories = (0..4)
        .map(|_| tempfile::tempdir().expect("fixture directory"))
        .collect::<Vec<_>>();
    let schemas = [
        events_schema()?,
        users_schema()?,
        orders_schema()?,
        oracle_boundaries::schema(),
    ];
    let seeds = [
        (1..=10).map(event_row).collect(),
        (1..=8).map(user_row).collect(),
        order_rows(),
        oracle_boundaries::rows(),
    ];
    let mut stores = Vec::new();
    for ((directory, schema), rows) in directories.iter().zip(&schemas).zip(seeds) {
        let mut store = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .map_err(|e| e.to_string())?;
        store.ingest(rows).map_err(|e| e.to_string())?;
        stores.push(store);
    }
    let snapshots = stores.iter().map(TableStore::snapshot).collect::<Vec<_>>();
    let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
        (
            DATABASE_ID,
            TableId::new(u64::try_from(i + 1).expect("table id")),
            s,
        )
    }))
    .map_err(|e| e.to_string())?;
    let catalog = catalog(schemas[0].clone(), schemas[1].clone(), schemas[2].clone())?;
    execute_pintail(sql, &catalog, &provider)
}

pub fn bounded_execute(sql: &str) -> Result<Vec<Vec<OracleValue>>, String> {
    bounded_execute_mode(sql, "")
}

fn bounded_execute_mode(sql: &str, mode: &str) -> Result<Vec<Vec<OracleValue>>, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let output = directory.path().join("result.json");
    let mut child = Command::new("sh")
        .args([
            "-c",
            "ulimit -v 1048576; ulimit -f 4096; exec \"$@\"",
            "candidate-worker",
        ])
        .arg(std::env::current_exe().map_err(|e| e.to_string())?)
        .args([
            "--exact",
            "validates_generated_candidates_worker",
            "--ignored",
        ])
        // Two workers means two: the scan pool otherwise sizes itself to the
        // host's cores, and on a many-core host its threads and their malloc
        // arenas outgrow the 1 GiB address-space ceiling before any query
        // runs, failing thread creation with EAGAIN.
        .env("RAYON_NUM_THREADS", "2")
        .env("PINTAIL_SCAN_THREADS", "2")
        .env("MALLOC_ARENA_MAX", "2")
        .env("PINTAIL_CANDIDATE_SQL", sql)
        .env("PINTAIL_CANDIDATE_MODE", mode)
        .env("PINTAIL_CANDIDATE_RESULT", &output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                return Err("candidate worker failed".into());
            }
            let data = std::fs::read(output).map_err(|e| e.to_string())?;
            if data.len() > 2_000_000 {
                return Err("candidate result exceeds 2 MB".into());
            }
            return serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("candidate exceeded five-second process deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn minimize(
    mysql: &MysqlContainer,
    sql: &str,
    mode: &'static str,
    actual: &Result<Vec<Vec<OracleValue>>, String>,
) -> String {
    let mut minimized = sql.to_owned();
    for _ in 0..8 {
        let mut reduced = false;
        for sql in trusted_reductions(&minimized).into_iter().take(8) {
            let case = OracleCase {
                sql_mode: mode,
                family: "seed-reduction",
                sql: sql.clone(),
                ordered: true,
            };
            let Ok(expected) = execute_mysql_cases(mysql, &[case]) else {
                continue;
            };
            let probe = bounded_execute_mode(&sql, mode);
            let same_error = match (&probe, actual) {
                (Err(a), Err(b)) => a == b,
                (Ok(_), Ok(_)) => true,
                _ => false,
            };
            if same_error
                && !probe
                    .as_ref()
                    .is_ok_and(|r| oracle_rows_equal(r, &expected[0], true))
            {
                minimized = sql;
                reduced = true;
                break;
            }
        }
        if !reduced {
            break;
        }
    }
    minimized
}
