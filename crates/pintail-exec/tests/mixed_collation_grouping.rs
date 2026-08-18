//! Grouping keys of two text collations is answerable: grouping never
//! compares one key column AGAINST another, so each key folds under its own
//! rules - exactly as a sort orders each key by its own collation. Reported
//! by a customer whose staging schema groups a `general_ci` column next to a
//! `0900_ai_ci` one; Pintail refused the query outright.
//!
//! The fixture makes the two collations' rules pull apart: `general_ci`
//! PAD SPACE folds `'red'` and `'red '` together, while `0900_ai_ci` is
//! NO PAD and keeps `'ann'` and `'ann '` distinct - in the same query.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const GENERAL_CI: &str = "utf8mb4_general_ci";
const AI_CI: &str = "utf8mb4_0900_ai_ci";

/// (id, ci, ai, school, n). Folds: `ci` under `general_ci` gives red x3 and
/// blue x2; `ai` under `0900_ai_ci` gives Ann x2, 'ann ' x1, Bob x2.
const ROWS: [(u64, &str, &str, u64, i64); 5] = [
    (1, "red", "Ann", 1, 10),
    (2, "RED", "ann", 1, 5),
    (3, "red ", "ann ", 1, 1),
    (4, "blue", "Bob", 2, 2),
    (5, "BLUE", "bob", 2, 4),
];

fn items_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "ci", DataType::Utf8, true).with_collation(Some(GENERAL_CI.to_owned())),
            Column::new(3, "ai", DataType::Utf8, true).with_collation(Some(AI_CI.to_owned())),
            Column::new(4, "school", DataType::UInt64, false),
            Column::new(5, "n", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn schools_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, true)
                .with_collation(Some(GENERAL_CI.to_owned())),
        ],
    )
    .expect("schema")
}

fn item_row(id: u64, ci: &str, ai: &str, school: u64, n: i64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(ci.to_owned()),
            Value::Utf8(ai.to_owned()),
            Value::UInt64(school),
            Value::Int64(n),
        ],
        id,
        false,
    )
}

fn school_row(id: u64, label: &str) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(label.to_owned())],
        id,
        false,
    )
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let items_dir = tempfile::tempdir().expect("temporary items table");
    let mut items = TableStore::open(items_dir.path(), items_schema(), StoreOptions::default())
        .expect("open items");
    items
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, ci, ai, school, n)| item_row(*id, ci, ai, *school, *n))
                .collect(),
        )
        .expect("bulk items");
    let schools_dir = tempfile::tempdir().expect("temporary schools table");
    let mut schools = TableStore::open(
        schools_dir.path(),
        schools_schema(),
        StoreOptions::default(),
    )
    .expect("open schools");
    schools
        .bulk_ingest_snapshot(vec![school_row(1, "North"), school_row(2, "SOUTH")])
        .expect("bulk schools");

    let items_snapshot = items.snapshot();
    let schools_snapshot = schools.snapshot();
    let database_id = DatabaseId::new(15);
    let items_id = TableId::new(17);
    let schools_id = TableId::new(18);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                items_id,
                "items",
                items_schema(),
                TableStatistics::with_row_count(ROWS.len() as u64),
            )
            .expect("items entry"),
            TableEntry::new(
                schools_id,
                "schools",
                schools_schema(),
                TableStatistics::with_row_count(2),
            )
            .expect("schools entry"),
        ],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, items_id, &items_snapshot),
        (database_id, schools_id, &schools_snapshot),
    ])
    .expect("provider");

    let statement = parse_statement(sql).expect("parse query");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind query");
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
        let columns = batch.columns().len();
        for row in batch.selection().selected_rows() {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value");
                values.push(render(&value));
            }
            rows.push(values);
        }
    }
    rows
}

#[test]
fn v1_grouping_a_general_ci_key_next_to_an_ai_ci_key_answers() {
    // The customer's blocker shape. Each key folds by its own collation:
    // general_ci merges red/RED/'red ' and blue/BLUE; 0900_ai_ci merges
    // Ann/ann but keeps 'ann ' its own group (NO PAD).
    let rows = run("SELECT ci, ai, COUNT(*) AS c FROM items GROUP BY ci, ai ORDER BY ci, ai");
    assert_eq!(
        rows,
        [
            ["blue", "Bob", "2"],
            ["red", "Ann", "2"],
            ["red ", "ann ", "1"]
        ]
    );
}

#[test]
fn v2_the_key_order_reversed_answers_the_same_groups() {
    let rows = run("SELECT ai, ci, COUNT(*) AS c FROM items GROUP BY ai, ci ORDER BY ai, ci");
    assert_eq!(
        rows,
        [
            ["Ann", "red", "2"],
            ["ann ", "red ", "1"],
            ["Bob", "blue", "2"]
        ]
    );
}

#[test]
fn v3_an_aggregate_over_mixed_keys_sums_within_each_fold() {
    let rows = run("SELECT ci, ai, SUM(n) AS total FROM items GROUP BY ci, ai ORDER BY ci, ai");
    assert_eq!(
        rows,
        [
            ["blue", "Bob", "6"],
            ["red", "Ann", "15"],
            ["red ", "ann ", "1"],
        ]
    );
}

#[test]
fn v4_grouping_the_general_ci_key_alone_still_pads_and_folds_case() {
    let rows = run("SELECT ci, COUNT(*) AS c FROM items GROUP BY ci ORDER BY ci");
    assert_eq!(rows, [["blue", "2"], ["red", "3"]]);
}

#[test]
fn v5_grouping_the_ai_ci_key_alone_keeps_no_pad_semantics() {
    let rows = run("SELECT ai, COUNT(*) AS c FROM items GROUP BY ai ORDER BY ai");
    assert_eq!(rows, [["Ann", "2"], ["ann ", "1"], ["Bob", "2"]]);
}

#[test]
fn v6_a_numeric_key_rides_along_with_the_mixed_pair() {
    let rows = run("SELECT school, ci, ai, COUNT(*) AS c FROM items \
         GROUP BY school, ci, ai ORDER BY school, ci, ai");
    assert_eq!(
        rows,
        [
            ["1", "red", "Ann", "2"],
            ["1", "red ", "ann ", "1"],
            ["2", "blue", "Bob", "2"],
        ]
    );
}

#[test]
fn v7_the_customer_shape_join_then_group_by_both_sides_names() {
    // JOIN School then GROUP BY s.sectionName, sc.schoolName, verbatim in
    // miniature: one key from each table, different collations.
    let rows = run("SELECT s.label, i.ai, COUNT(*) AS c FROM items i \
         JOIN schools s ON s.id = i.school \
         GROUP BY s.label, i.ai ORDER BY s.label, i.ai");
    assert_eq!(
        rows,
        [
            ["North", "Ann", "2"],
            ["North", "ann ", "1"],
            ["SOUTH", "Bob", "2"],
        ]
    );
}

#[test]
fn v8_distinct_over_the_mixed_pair_folds_each_side_by_its_own_rules() {
    let rows = run("SELECT DISTINCT ci, ai FROM items ORDER BY ci, ai");
    assert_eq!(rows, [["blue", "Bob"], ["red", "Ann"], ["red ", "ann "]]);
}

#[test]
fn v9_having_filters_the_mixed_grouping() {
    let rows = run("SELECT ci, ai, COUNT(*) AS c FROM items GROUP BY ci, ai \
         HAVING COUNT(*) > 1 ORDER BY ci, ai");
    assert_eq!(rows, [["blue", "Bob", "2"], ["red", "Ann", "2"]]);
}

#[test]
fn v10_each_collation_keeps_its_own_pad_rule_in_one_query() {
    // The sharpest split: the SAME query must PAD-fold the general_ci key
    // ('red' absorbs 'red ') while NOT pad-folding the 0900 key ('ann '
    // stays distinct from 'ann'). A single shared collation gets one of
    // the two wrong, whichever it picks.
    let rows = run("SELECT COUNT(DISTINCT ci) AS ci_folds, COUNT(*) AS total FROM items");
    assert_eq!(rows, [["2", "5"]]);
    let rows = run("SELECT COUNT(DISTINCT ai) AS ai_folds FROM items");
    assert_eq!(rows, [["3"]]);
}
