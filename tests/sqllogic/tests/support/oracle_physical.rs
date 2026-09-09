use super::*;

fn execute(
    sql: &str,
    catalog: &CatalogSnapshot,
    provider: &SnapshotScanProvider<'_>,
    limit: usize,
) -> Result<
    (
        Vec<Vec<OracleValue>>,
        pintail_exec::spill::QuerySpillMetrics,
    ),
    String,
> {
    let bound = Binder::new(catalog, Some("app"))
        .bind(&parse_statement(sql).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .map_err(|e| e.to_string())?;
    let mut execution =
        Execution::start(plan, provider, limit, Collation::default()).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(|e| e.to_string())? {
        for index in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|c| canonical_value(c.value(index).expect("result cell")))
                    .collect(),
            );
        }
    }
    Ok((rows, execution.spill_metrics()))
}

#[allow(clippy::too_many_lines)]
pub fn run() -> Result<(), String> {
    let mysql = MysqlContainer::start()?;
    mysql.query_batch(oracle_boundaries::SQL)?;
    let mut counts = BTreeMap::<&str, usize>::new();
    let cases = oracle_boundaries::cases()
        .into_iter()
        .filter(|c| {
            if !c.sql.contains("FROM bounds") || c.sql.contains("FROM orders") {
                return false;
            }
            let count = counts.entry(c.family).or_default();
            *count += 1;
            *count <= 6
        })
        .collect::<Vec<_>>();
    let expected = execute_mysql_cases(&mysql, &cases)?;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let options = StoreOptions {
        compaction_fan_in: 2,
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), oracle_boundaries::schema(), options)
        .map_err(|e| e.to_string())?;
    table
        .ingest(oracle_boundaries::rows())
        .map_err(|e| e.to_string())?;
    let entry = TableEntry::new(
        TableId::new(4),
        "bounds",
        oracle_boundaries::schema(),
        TableStatistics::with_row_count(8),
    )
    .map_err(|e| e.to_string())?
    .with_key_columns([1])
    .map_err(|e| e.to_string())?;
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DATABASE_ID, "app", [entry]).map_err(|e| e.to_string())?
    ])
    .map_err(|e| e.to_string())?;
    let mut outcomes = Vec::new();
    for phase in ["memtable", "persisted", "mixed", "compacted", "reopened"] {
        match phase {
            "persisted" => {
                table.flush().map_err(|e| e.to_string())?;
            }
            "mixed" => {
                let row = oracle_boundaries::rows().remove(0);
                table
                    .ingest(vec![StoredRow::new(
                        row.key().clone(),
                        row.values().to_vec(),
                        100,
                        false,
                    )])
                    .map_err(|e| e.to_string())?;
            }
            "compacted" => {
                table.flush().map_err(|e| e.to_string())?;
                let compacted = table.compact().map_err(|e| e.to_string())?;
                outcomes.push(serde_json::json!({ "phase": phase, "operation": "compaction", "inputSegments": compacted.input_segments(), "status": if compacted.input_segments() >= 2 { "PASS" } else { "FAIL" } }));
            }
            "reopened" => {
                drop(table);
                table = TableStore::open(directory.path(), oracle_boundaries::schema(), options)
                    .map_err(|e| e.to_string())?;
            }
            _ => {}
        }
        let snapshot = table.snapshot();
        let provider = SnapshotScanProvider::new([(DATABASE_ID, TableId::new(4), &snapshot)])
            .map_err(|e| e.to_string())?;
        for (case, expected) in cases.iter().zip(&expected) {
            let actual = execute(&case.sql, &catalog, &provider, MEMORY_LIMIT);
            let pass = actual
                .as_ref()
                .is_ok_and(|(rows, _)| oracle_rows_equal(rows, expected, case.ordered));
            outcomes.push(serde_json::json!({ "phase": phase, "id": oracle_transport::case_id(case), "sql": case.sql,
                "status": if pass { "PASS" } else { "FAIL" }, "expected": expected, "actual": actual.as_ref().ok().map(|(r, _)| r), "error": actual.as_ref().err() }));
        }
    }
    stress(&mysql, &mut outcomes)?;
    let failures = outcomes.iter().filter(|o| o["status"] != "PASS").count();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../validate-out/oracle-physical.json");
    std::fs::create_dir_all(path.parent().expect("report parent")).map_err(|e| e.to_string())?;
    std::fs::write(path, serde_json::to_string_pretty(&serde_json::json!({ "provenance": oracle_transport::provenance(&mysql)?, "outcomes": outcomes, "failures": failures })).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if failures > 0 {
        return Err(format!(
            "{failures} physical replay checks failed; see validate-out/oracle-physical.json"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn stress(mysql: &MysqlContainer, outcomes: &mut Vec<serde_json::Value>) -> Result<(), String> {
    const ROWS: u64 = 120_000;
    mysql.query_batch("CREATE TABLE digits (n INT PRIMARY KEY); INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9); CREATE TABLE spill_rows (id BIGINT UNSIGNED PRIMARY KEY, k BIGINT NOT NULL, payload VARCHAR(256) NOT NULL, amount DECIMAL(12,3) NOT NULL); INSERT INTO spill_rows SELECT n, MOD(n,15000), CONCAT(LPAD(n,6,'0'),REPEAT('x',234)), MOD(n,1000)/1000 FROM (SELECT a.n+10*b.n+100*c.n+1000*d.n+10000*e.n+100000*f.n AS n FROM digits a CROSS JOIN digits b CROSS JOIN digits c CROSS JOIN digits d CROSS JOIN digits e CROSS JOIN digits f) t WHERE n<120000;")?;
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "k", DataType::Int64, false),
            Column::new(3, "payload", DataType::Utf8, false),
            Column::new(
                4,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 3,
                },
                false,
            ),
        ],
    )
    .map_err(|e| e.to_string())?;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
        .map_err(|e| e.to_string())?;
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(10_000) {
        table
            .ingest(
                chunk
                    .iter()
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                            vec![
                                Value::UInt64(*id),
                                Value::Int64(i64::try_from(id % 15_000).expect("group")),
                                Value::Utf8(format!("{id:06}{}", "x".repeat(234))),
                                Value::Utf8(format!("0.{:03}", id % 1000)),
                            ],
                            *id,
                            false,
                        )
                    })
                    .collect(),
            )
            .map_err(|e| e.to_string())?;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "spill_rows",
        schema,
        TableStatistics::with_row_count(ROWS),
    )
    .map_err(|e| e.to_string())?
    .with_key_columns([1])
    .map_err(|e| e.to_string())?;
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DATABASE_ID, "app", [entry]).map_err(|e| e.to_string())?
    ])
    .map_err(|e| e.to_string())?;
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DATABASE_ID, TableId::new(1), &snapshot)])
        .map_err(|e| e.to_string())?;
    let cases = [
        ("aggregate", "SELECT k,COUNT(*),SUM(amount),MIN(payload),COUNT(DISTINCT payload) FROM spill_rows GROUP BY k ORDER BY k"),
        ("sort", "SELECT id FROM spill_rows ORDER BY payload DESC,id"),
        ("join", "SELECT a.k,COUNT(*),SUM(a.amount) FROM spill_rows a JOIN spill_rows b ON a.id=b.id GROUP BY a.k ORDER BY a.k"),
        ("window", "SELECT id,ROW_NUMBER() OVER (PARTITION BY k ORDER BY id) FROM spill_rows ORDER BY id"),
    ].map(|(family,sql)| OracleCase {sql_mode:"",family,sql:sql.into(),ordered:true});
    let expected = execute_mysql_cases(mysql, &cases)?;
    let mut spilled = false;
    for limit in [12 * 1024 * 1024, 24 * 1024 * 1024, 256 * 1024 * 1024] {
        for (case, expected) in cases.iter().zip(&expected) {
            let actual = execute(&case.sql, &catalog, &provider, limit);
            let pass = actual
                .as_ref()
                .is_ok_and(|(r, _)| oracle_rows_equal(r, expected, true));
            let spill = actual.as_ref().ok().map(|(_, s)| s.files);
            spilled |= spill.is_some_and(|n| n > 0);
            outcomes.push(serde_json::json!({"phase":"memory-ceilings","family":case.family,"sql":case.sql,"limit":limit,"status":if pass {"PASS"} else {"FAIL"},"spillFiles":spill,"error":actual.as_ref().err(),"expectedRows":expected.len(),"actualRows":actual.as_ref().ok().map(|(r,_)| r.len())}));
        }
    }
    outcomes.push(serde_json::json!({"phase":"memory-ceilings","operation":"observed-spill","status":if spilled {"PASS"} else {"FAIL"}}));
    Ok(())
}
