//! One comparison between columns of two collations resolves the way `MySQL`
//! resolves it, rather than being refused wholesale.
//!
//! Columns share one coercibility rung, so the charset decides first - the
//! wider charset wins, whatever collation the narrower side carries - and
//! within one charset a binary collation beats a case-insensitive one. Two
//! non-binary collations of one charset stay `MySQL`'s "illegal mix" error.
//! Every expected answer here was read from `MySQL` 8.4 over the same rows.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// (id, g, b, ai, m3, m3b, l1) under `general_ci`, `utf8mb4_bin`, `0900_ai_ci`,
/// `utf8mb3_general_ci`, `utf8mb3_bin` and `latin1_swedish_ci`.
const ROWS: [(u64, [&str; 6]); 3] = [
    (1, ["a", "a", "A", "A", "A", "A"]),
    (2, ["b ", "B", "b", "b", "b", "b"]),
    (3, ["c", "c", "c ", "C ", "c", "C"]),
];

const COLLATIONS: [&str; 6] = [
    "utf8mb4_general_ci",
    "utf8mb4_bin",
    "utf8mb4_0900_ai_ci",
    "utf8mb3_general_ci",
    "utf8mb3_bin",
    "latin1_swedish_ci",
];

fn schema() -> TableSchema {
    let mut columns = vec![Column::new(1, "id", DataType::UInt64, false)];
    for (index, (name, collation)) in ["g", "b", "ai", "m3", "m3b", "l1"]
        .into_iter()
        .zip(COLLATIONS)
        .enumerate()
    {
        let id = u32::try_from(index).expect("column index") + 2;
        columns.push(
            Column::new(id, name, DataType::Utf8, true).with_collation(Some(collation.to_owned())),
        );
    }
    TableSchema::new(1, columns).expect("schema")
}

fn row(id: u64, texts: [&str; 6]) -> StoredRow {
    let mut values = vec![Value::UInt64(id)];
    values.extend(texts.iter().map(|text| Value::Utf8((*text).to_owned())));
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) => text.clone(),
        Value::UInt64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(sql: &str) -> Result<Vec<String>, String> {
    let dir = tempfile::tempdir().expect("temporary table");
    let mut store =
        TableStore::open(dir.path(), schema(), StoreOptions::default()).expect("open table");
    store
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, texts)| row(*id, *texts)).collect())
        .expect("bulk rows");
    let snapshot = store.snapshot();
    let database_id = DatabaseId::new(21);
    let table_id = TableId::new(22);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [TableEntry::new(
            table_id,
            "m",
            schema(),
            TableStatistics::with_row_count(ROWS.len() as u64),
        )
        .expect("table entry")],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");

    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| error.to_string())?;
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        for selected in batch.selection().selected_rows() {
            let values: Vec<String> = (0..batch.columns().len())
                .map(|column| {
                    render(
                        batch
                            .column(column)
                            .and_then(|column| column.value(selected))
                            .expect("selected value"),
                    )
                })
                .collect();
            rows.push(values.join("-"));
        }
    }
    Ok(rows)
}

fn ids(sql: &str) -> Vec<String> {
    run(sql).unwrap_or_else(|error| panic!("{sql} must bind: {error}"))
}

#[test]
fn a_binary_collation_beats_a_case_insensitive_one_in_its_charset() {
    assert_eq!(ids("SELECT id FROM m WHERE g = b ORDER BY id"), ["1", "3"]);
    assert_eq!(ids("SELECT id FROM m WHERE b = ai ORDER BY id"), ["3"]);
}

#[test]
fn the_wider_charset_wins_whatever_the_narrower_side_carries() {
    assert_eq!(
        ids("SELECT id FROM m WHERE ai = m3 ORDER BY id"),
        ["1", "2", "3"]
    );
    // utf8mb3's binary collation loses to utf8mb4's general_ci: the charset
    // decides before the binary rule is consulted.
    assert_eq!(
        ids("SELECT id FROM m WHERE g = m3b ORDER BY id"),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids("SELECT id FROM m WHERE ai = l1 ORDER BY id"),
        ["1", "2"]
    );
    assert_eq!(
        ids("SELECT id FROM m WHERE m3 = l1 ORDER BY id"),
        ["1", "2", "3"]
    );
}

#[test]
fn join_keys_of_two_collations_hash_under_the_resolved_one() {
    assert_eq!(
        ids("SELECT x.id, y.id FROM m x JOIN m y ON x.g = y.b ORDER BY x.id, y.id"),
        ["1-1", "3-3"]
    );
    assert_eq!(
        ids("SELECT x.id, y.id FROM m x JOIN m y ON x.ai = y.m3b ORDER BY x.id, y.id"),
        ["1-1", "2-2"]
    );
}

#[test]
fn an_in_subquery_compares_under_both_sides_together() {
    assert_eq!(
        ids("SELECT id FROM m WHERE g IN (SELECT b FROM m) ORDER BY id"),
        ["1", "3"]
    );
}

#[test]
fn two_case_insensitive_collations_of_one_charset_stay_refused() {
    let error = run("SELECT id FROM m WHERE g = ai").expect_err("an illegal mix must refuse");
    assert!(error.contains("llegal mix of collations"), "{error}");
    let error = run("SELECT g FROM m UNION SELECT ai FROM m").expect_err("a union of the two");
    assert!(
        error.contains("llegal mix of collations for operation 'UNION'"),
        "{error}"
    );
}
