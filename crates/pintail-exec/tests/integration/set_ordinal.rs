//! `MySQL` sorts a SET by its member BITMASK - member one is bit 0 - never
//! by the label text. Declaration order here makes bitmask order and
//! alphabetical order disagree everywhere, and the comma-joined subsets
//! make masks non-obvious: 'Trailers'=1 sorts before 'Behind'=8 while
//! 'Trailers,Behind'=9 sorts after both. Verified byte-exact against
//! `MySQL` 8.4 over `sakila.film.special_features` on the live pair.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Declaration order: Trailers(1), Commentaries(2), Deleted(4), Behind(8).
const MEMBERS: [&str; 4] = ["Trailers", "Commentaries", "Deleted", "Behind"];

/// (id, features): masks 1, 8, 3, 9, 2, 0, 8, 3, 12, 1.
const ROWS: [(u64, &str); 10] = [
    (1, "Trailers"),
    (2, "Behind"),
    (3, "Trailers,Commentaries"),
    (4, "Trailers,Behind"),
    (5, "Commentaries"),
    (6, ""),
    (7, "Behind"),
    (8, "Trailers,Commentaries"),
    (9, "Deleted,Behind"),
    (10, "Trailers"),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "features", DataType::Utf8, true)
                .with_set_members(Some(MEMBERS.iter().map(ToString::to_string).collect())),
        ],
    )
    .expect("schema")
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, features)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![Value::UInt64(*id), Value::Utf8((*features).to_owned())],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "films",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row) {
                        Some(Value::Null) | None => "NULL".to_owned(),
                        Some(Value::Utf8(text) | Value::Enum { label: text, .. }) => text.clone(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(other) => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

fn column(rows: &[Vec<String>], index: usize) -> Vec<String> {
    rows.iter().map(|row| row[index].clone()).collect()
}

#[test]
fn v1_order_by_ascends_by_mask() {
    let rows = run("SELECT features FROM films ORDER BY features, id");
    assert_eq!(
        column(&rows, 0),
        [
            "",
            "Trailers",
            "Trailers",
            "Commentaries",
            "Trailers,Commentaries",
            "Trailers,Commentaries",
            "Behind",
            "Behind",
            "Trailers,Behind",
            "Deleted,Behind",
        ]
    );
}

#[test]
fn v2_order_by_descends_by_mask() {
    let rows = run("SELECT features FROM films ORDER BY features DESC, id LIMIT 3");
    assert_eq!(
        column(&rows, 0),
        ["Deleted,Behind", "Trailers,Behind", "Behind"]
    );
}

#[test]
fn v3_grouping_orders_groups_by_mask() {
    let rows = run("SELECT features, COUNT(*) FROM films GROUP BY features ORDER BY features");
    assert_eq!(
        rows,
        [
            ["", "1"],
            ["Trailers", "2"],
            ["Commentaries", "1"],
            ["Trailers,Commentaries", "2"],
            ["Behind", "2"],
            ["Trailers,Behind", "1"],
            ["Deleted,Behind", "1"],
        ]
    );
}

#[test]
fn v4_grouped_tie_breaks_on_the_mask() {
    let rows = run("SELECT features, COUNT(*) FROM films GROUP BY features \
         ORDER BY COUNT(*) DESC, features LIMIT 3");
    assert_eq!(
        rows,
        [
            ["Trailers", "2"],
            ["Trailers,Commentaries", "2"],
            ["Behind", "2"]
        ]
    );
}

#[test]
fn v5_distinct_orders_by_mask() {
    let rows = run("SELECT DISTINCT features FROM films ORDER BY features");
    assert_eq!(
        column(&rows, 0),
        [
            "",
            "Trailers",
            "Commentaries",
            "Trailers,Commentaries",
            "Behind",
            "Trailers,Behind",
            "Deleted,Behind",
        ]
    );
}

#[test]
fn v6_min_and_max_compare_as_strings() {
    // MySQL's MIN/MAX compare a SET as its label STRING, exactly as they
    // compare an ENUM - only sorting follows the mask. Measured on 8.4:
    // MAX is 'Trailers,Commentaries' (lexically last), not mask 12.
    let rows = run("SELECT MIN(features), MAX(features) FROM films");
    assert_eq!(rows, [["", "Trailers,Commentaries"]]);
}

#[test]
fn v7_a_topk_limit_keeps_the_lowest_masks() {
    let rows = run("SELECT id, features FROM films ORDER BY features, id LIMIT 4");
    assert_eq!(
        rows,
        [
            ["6", ""],
            ["1", "Trailers"],
            ["10", "Trailers"],
            ["5", "Commentaries"],
        ]
    );
}

#[test]
fn v8_a_window_order_walks_the_mask() {
    let rows = run(
        "SELECT features, ROW_NUMBER() OVER (ORDER BY features, id) AS r \
         FROM films ORDER BY r LIMIT 3",
    );
    assert_eq!(rows, [["", "1"], ["Trailers", "2"], ["Trailers", "3"]]);
}

#[test]
fn v9_equality_still_matches_by_text() {
    let rows = run("SELECT COUNT(*) FROM films WHERE features = 'Trailers,Commentaries'");
    assert_eq!(rows, [["2"]]);
}

#[test]
fn v10_group_representative_keeps_the_declared_spelling() {
    // The stored spelling round-trips; masks never rewrite the text.
    let rows = run("SELECT features FROM films WHERE id IN (4, 9) ORDER BY features");
    assert_eq!(column(&rows, 0), ["Trailers,Behind", "Deleted,Behind"]);
}

fn run_mixed(sql: &str) -> Vec<Vec<String>> {
    // Same rows, but the tail arrives through the WAL/memtable path the
    // CDC stream uses, not the snapshot bulk loader - the split the e2e
    // gate exposed (#256's lesson: the two paths can disagree).
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    let build = |(id, features): &(u64, &str)| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
            vec![Value::UInt64(*id), Value::Utf8((*features).to_owned())],
            *id,
            false,
        )
    };
    table
        .bulk_ingest_snapshot(ROWS[..7].iter().map(build).collect())
        .expect("snapshot ingest");
    table
        .ingest(ROWS[7..].iter().map(build).collect())
        .expect("memtable ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "films",
        schema(),
        TableStatistics::with_row_count(ROWS.len() as u64),
    )
    .expect("entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row) {
                        Some(Value::Null) | None => "NULL".to_owned(),
                        Some(Value::Utf8(text) | Value::Enum { label: text, .. }) => text.clone(),
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(other) => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

#[test]
fn grouped_set_walks_the_bitmask_including_empty() {
    // MySQL groups a SET by value and orders the groups by member bitmask;
    // the empty set is bitmask 0 and sorts FIRST. Found by the e2e corpus:
    // the grouped path dropped the empty group to the back.
    assert_eq!(
        run("SELECT features, COUNT(*) FROM films GROUP BY features ORDER BY features"),
        vec![
            vec![String::new(), "1".to_owned()],
            vec!["Trailers".to_owned(), "2".to_owned()],
            vec!["Commentaries".to_owned(), "1".to_owned()],
            vec!["Trailers,Commentaries".to_owned(), "2".to_owned()],
            vec!["Behind".to_owned(), "2".to_owned()],
            vec!["Trailers,Behind".to_owned(), "1".to_owned()],
            vec!["Deleted,Behind".to_owned(), "1".to_owned()],
        ]
    );
}

#[test]
fn grouped_set_walks_the_bitmask_across_snapshot_and_memtable() {
    assert_eq!(
        run_mixed("SELECT features, COUNT(*) FROM films GROUP BY features ORDER BY features"),
        vec![
            vec![String::new(), "1".to_owned()],
            vec!["Trailers".to_owned(), "2".to_owned()],
            vec!["Commentaries".to_owned(), "1".to_owned()],
            vec!["Trailers,Commentaries".to_owned(), "2".to_owned()],
            vec!["Behind".to_owned(), "2".to_owned()],
            vec!["Trailers,Behind".to_owned(), "1".to_owned()],
            vec!["Deleted,Behind".to_owned(), "1".to_owned()],
        ]
    );
}
