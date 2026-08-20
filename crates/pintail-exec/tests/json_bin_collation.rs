//! JSON string results collate `utf8mb4_bin` (measured against `MySQL` 8.4):
//! grouping and comparing them is case-sensitive, losing only to a real
//! column's collation. This is the in-process repro for the oracle family -
//! and for the case-703 regression, where the bin-collated filter died with
//! "bound expression has an invalid physical type".

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: [(u64, Option<&str>); 5] = [
    (1, Some(r#"{"tags":["premium"],"score":1}"#)),
    (2, Some(r#"{"tags":["PREMIUM"],"score":2}"#)),
    (3, Some(r#"{"tags":["premium"],"score":3}"#)),
    (4, None),
    (5, Some(r#"{"tags":[],"score":0}"#)),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "meta", DataType::Json, true),
        ],
    )
    .expect("schema")
}

fn row(id: u64, meta: Option<&str>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            meta.map_or(Value::Null, |text| Value::Utf8(text.to_owned())),
        ],
        id,
        false,
    )
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, meta)| row(*id, *meta)).collect())
        .expect("bulk snapshot");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(15);
    let table_id = TableId::new(17);
    let entry = TableEntry::new(
        table_id,
        "orders",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
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
                values.push(match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Boolean(flag) => if flag { "1" } else { "0" }.to_owned(),
                    Value::Utf8(text) | Value::Enum { label: text, .. } => text,
                    Value::UInt64(number) => number.to_string(),
                    Value::Int64(number) => number.to_string(),
                    other => format!("{other:?}"),
                });
            }
            rows.push(values);
        }
    }
    rows
}

#[test]
fn a_bin_collated_json_filter_executes() {
    // The case-703 regression: this exact shape died in execution once the
    // comparison's collation resolved to utf8mb4_bin.
    let rows = run("SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' = 'premium'");
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn json_grouping_is_case_sensitive() {
    // Ordered by count, not by the alias: ordering by an alias of a
    // bin-collated group key is a known gap (the GroupKey reference is
    // opaque to collation resolution) tracked separately; what THIS test
    // pins is the case-sensitive grouping itself.
    let rows = run("SELECT meta->>'$.tags[0]' AS t, COUNT(*) FROM orders \
         WHERE meta IS NOT NULL AND meta->>'$.tags[0]' IS NOT NULL \
         GROUP BY meta->>'$.tags[0]' ORDER BY COUNT(*), t");
    assert_eq!(
        rows,
        vec![
            vec!["PREMIUM".to_owned(), "1".to_owned()],
            vec!["premium".to_owned(), "2".to_owned()],
        ]
    );
}

#[test]
fn json_literal_comparison_is_case_sensitive() {
    let rows = run(
        "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\"k\":\"A\"}','$.k')) = 'a', \
                JSON_UNQUOTE(JSON_EXTRACT('{\"k\":\"A\"}','$.k')) = 'A'",
    );
    assert_eq!(rows, vec![vec!["0".to_owned(), "1".to_owned()]]);
}

#[test]
fn probe_projection_only() {
    let rows = run("SELECT meta->>'$.tags[0]' FROM orders ORDER BY id");
    assert_eq!(rows.len(), 5);
}

#[test]
fn probe_filter_without_json() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE id > 2");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn probe_explicit_unquote_filter() {
    let rows = run(
        "SELECT COUNT(*) FROM orders WHERE JSON_UNQUOTE(JSON_EXTRACT(meta,'$.tags[0]')) = 'premium'",
    );
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn probe_is_not_null_filter() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE meta->>'$.tags[0]' IS NOT NULL");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn probe_extract_no_unquote_filter() {
    let rows = run("SELECT COUNT(*) FROM orders WHERE JSON_EXTRACT(meta,'$.tags[0]') IS NOT NULL");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn a_null_only_derived_table_groups() {
    // An untyped NULL projection is compatible with any derived column
    // type; this shape was an internal 'invalid physical plan' error.
    let rows = run("SELECT k, COUNT(*) FROM (SELECT NULL AS k) d GROUP BY k");
    assert_eq!(rows, vec![vec!["NULL".to_owned(), "1".to_owned()]]);
}

#[test]
fn probe_cast_unsigned() {
    println!("GOT {:?}", run("SELECT CAST(-1 AS UNSIGNED)"));
}
#[test]
fn probe_trim_both() {
    println!("GOT {:?}", run("SELECT TRIM(BOTH 'x' FROM 'xxaxx')"));
}
#[test]
fn probe_extract_year_month() {
    // sqlparser rejects composite EXTRACT fields at parse time; the
    // composite-unit support (in progress) pre-rewrites the SQL text.
    assert!(parse_statement("SELECT EXTRACT(YEAR_MONTH FROM '2025-07-21 10:00:00')").is_err());
}
#[test]
fn probe_spaceship() {
    println!("GOT {:?}", run("SELECT NULL <=> NULL, 1 <=> NULL, 1 <=> 1"));
}

#[test]
fn collate_overrides_the_comparison() {
    // The operator escape hatch: bin forces case-sensitivity, general_ci
    // restores insensitivity over a bin-collated JSON result.
    let rows = run("SELECT 'A' = 'a' COLLATE utf8mb4_bin, \
                JSON_UNQUOTE(JSON_EXTRACT('{\"k\":\"A\"}','$.k')) = 'a' COLLATE utf8mb4_general_ci");
    assert_eq!(rows, vec![vec!["0".to_owned(), "1".to_owned()]]);
}

#[test]
fn ordering_by_a_group_key_alias_is_byte_wise() {
    // The GroupKey reference used to be opaque to collation resolution, so
    // this ordered under the case-insensitive default and PREMIUM/premium
    // came back in arbitrary order.
    let rows = run("SELECT meta->>'$.tags[0]' AS t, COUNT(*) FROM orders \
         WHERE meta IS NOT NULL AND meta->>'$.tags[0]' IS NOT NULL \
         GROUP BY meta->>'$.tags[0]' ORDER BY t");
    assert_eq!(
        rows,
        vec![
            vec!["PREMIUM".to_owned(), "1".to_owned()],
            vec!["premium".to_owned(), "2".to_owned()],
        ]
    );
}

#[test]
fn one_binary_operand_forces_byte_comparison() {
    let rows = run("SELECT 'a' = BINARY 'A', BINARY 'A' = 'a', 'a' = BINARY 'a', 'b' > BINARY 'a'");
    assert_eq!(
        rows,
        vec![vec![
            "0".to_owned(),
            "0".to_owned(),
            "1".to_owned(),
            "1".to_owned()
        ]]
    );
}

#[test]
fn binary_forces_byte_comparison_in_a_filter() {
    let rows = run("SELECT COUNT(*) FROM orders \
         WHERE BINARY JSON_UNQUOTE(JSON_EXTRACT(meta,'$.tags[0]')) = 'premium'");
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn json_documents_compare_by_the_ladder() {
    // Both sides JSON: the binder keys them, bytes decide. 1 = 1.0
    // numerically; member order is irrelevant; the ladder puts any array
    // above any number.
    let rows = run(
        "SELECT JSON_EXTRACT('{\"a\":1}','$.a') = JSON_EXTRACT('{\"a\":1.0}','$.a'), \
                JSON_EXTRACT('[{\"a\":1,\"b\":2}]','$[0]') = JSON_EXTRACT('[{\"b\":2,\"a\":1}]','$[0]'), \
                JSON_EXTRACT('[9]','$') > JSON_EXTRACT('{\"n\":99}','$.n')",
    );
    assert_eq!(
        rows,
        vec![vec!["1".to_owned(), "1".to_owned(), "1".to_owned()]]
    );
}

#[test]
fn json_columns_group_and_dedupe_structurally() {
    let rows = run(
        "SELECT COUNT(*) FROM (SELECT DISTINCT JSON_EXTRACT(meta,'$.tags') AS t \
         FROM orders WHERE meta IS NOT NULL) d",
    );
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
    let rows = run("SELECT COUNT(DISTINCT JSON_EXTRACT(meta,'$.tags')) FROM orders");
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
    let rows = run("SELECT COUNT(DISTINCT meta) FROM orders");
    assert_eq!(rows, vec![vec!["4".to_owned()]]);
}

#[test]
fn json_group_by_folds_equal_documents() {
    let rows = run(
        "SELECT COUNT(*) FROM (SELECT JSON_EXTRACT(meta,'$.tags') AS t, COUNT(*) AS n \
         FROM orders WHERE meta IS NOT NULL GROUP BY JSON_EXTRACT(meta,'$.tags')) g",
    );
    assert_eq!(rows, vec![vec!["3".to_owned()]]);
}

#[test]
fn json_order_by_uses_the_ladder() {
    // Scores 0, 1, 2, 3 as JSON numbers; ordering by the JSON value must
    // order numerically, with the NULL row first.
    let rows = run("SELECT id FROM orders ORDER BY JSON_EXTRACT(meta,'$.score'), id");
    assert_eq!(
        rows,
        vec![
            vec!["4".to_owned()],
            vec!["5".to_owned()],
            vec!["1".to_owned()],
            vec!["2".to_owned()],
            vec!["3".to_owned()],
        ]
    );
}

#[test]
fn json_in_and_between_ride_the_same_keys() {
    let rows = run("SELECT COUNT(*) FROM orders \
         WHERE JSON_EXTRACT(meta,'$.score') IN (JSON_EXTRACT('[1,3]','$[0]'), JSON_EXTRACT('[1,3]','$[1]'))");
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
    let rows = run("SELECT COUNT(*) FROM orders \
         WHERE JSON_EXTRACT(meta,'$.score') BETWEEN JSON_EXTRACT('[1]','$[0]') AND JSON_EXTRACT('[2]','$[0]')");
    assert_eq!(rows, vec![vec!["2".to_owned()]]);
}

#[test]
fn mixed_json_and_scalar_comparison_stays_rejected() {
    // MySQL coerces the scalar side to JSON here; Pintail refuses rather
    // than guessing that rule - one side wrapped would byte-compare
    // nonsense.
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(ROWS.iter().map(|(id, meta)| row(*id, *meta)).collect())
        .expect("bulk snapshot");
    let entry = TableEntry::new(
        TableId::new(17),
        "orders",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("table entry");
    let database = DatabaseEntry::new(DatabaseId::new(15), "app", [entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let statement =
        parse_statement("SELECT COUNT(*) FROM orders WHERE meta = 'x'").expect("parse query");
    assert!(Binder::new(&catalog, Some("app")).bind(&statement).is_err());
}

#[test]
fn json_path_wildcards_descent_ranges_and_last() {
    let rows = run("SELECT JSON_EXTRACT('{\"b\":2,\"aa\":1}','$.*'), \
                JSON_EXTRACT('[1,2,3]','$[*]'), \
                JSON_EXTRACT('{\"a\":{\"b\":1},\"c\":{\"b\":2}}','$**.b'), \
                JSON_EXTRACT('[1,2,3,4]','$[1 to 2]'), \
                JSON_EXTRACT('[1,2,3,4]','$[last-2 to last]'), \
                JSON_EXTRACT('[1,2,3]','$[last]'), \
                JSON_EXTRACT('[1,2,3]','$[last-1]'), \
                JSON_EXTRACT('3','$[0]'), \
                JSON_EXTRACT('{\"a\":1}','$[*]')");
    assert_eq!(
        rows,
        vec![vec![
            "[2, 1]".to_owned(),
            "[1, 2, 3]".to_owned(),
            "[1, 2]".to_owned(),
            "[2, 3]".to_owned(),
            "[2, 3, 4]".to_owned(),
            "3".to_owned(),
            "2".to_owned(),
            "3".to_owned(),
            "[{\"a\": 1}]".to_owned(),
        ]]
    );
}

#[test]
fn json_wildcard_misses_and_single_target_refusals() {
    let rows = run("SELECT JSON_EXTRACT('{}','$.*') IS NULL, \
                JSON_EXTRACT('[1]','$[5]') IS NULL, \
                JSON_CONTAINS_PATH('{\"a\":{\"b\":1}}','one','$**.b'), \
                JSON_CONTAINS_PATH('{\"a\":{\"b\":1}}','all','$**.b','$.a.*'), \
                JSON_CONTAINS_PATH('{\"a\":1}','one','$**.zz')");
    assert_eq!(
        rows,
        vec![vec![
            "1".to_owned(),
            "1".to_owned(),
            "1".to_owned(),
            "1".to_owned(),
            "0".to_owned(),
        ]]
    );
    // Single-target functions refuse multi-target paths, as MySQL does.
    let directory = tempfile::tempdir().expect("temporary table");
    drop(directory);
    let result = std::panic::catch_unwind(|| run("SELECT JSON_VALUE('{\"a\":1}','$.*')"));
    assert!(result.is_err());
}
