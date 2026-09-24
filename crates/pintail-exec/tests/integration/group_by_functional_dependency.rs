//! The Chitti LMS analytics shape, end to end: `GROUP BY` a foreign key
//! while selecting the left-joined dimension's name.
//!
//! `MySQL` accepts it because the join equality carries the grouping key onto
//! `payment_type`'s primary key, and a primary key fixes the rest of the row.
//! Pintail refused it with `ER_WRONG_FIELD_WITH_GROUP`, which took the
//! dashboard down for every query written this way.
//!
//! Binding it is only half the answer - the value has to come back one row
//! per group, with the counts of the WHOLE group beside it. A dependent
//! column added to the grouping keys would bind just as happily and then
//! split the group in two, so these expectations are about row counts and
//! aggregate values as much as about the name.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn enrollment_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "payment_type_id", DataType::Int64, true),
            Column::new(3, "status", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn payment_type_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "product_id", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

/// `(id, payment_type_id, status)`. Payment type 10 holds three enrollments
/// of which two are running, 20 holds two of which one is, and one
/// enrollment has no payment type at all - the group `MySQL` keys by NULL.
const ENROLLMENTS: [(u64, Option<i64>, &str); 6] = [
    (1, Some(10), "active"),
    (2, Some(10), "completed"),
    (3, Some(20), "active"),
    (4, Some(20), "hold"),
    (5, None, "active"),
    (6, Some(10), "active"),
];

/// `(id, name, product_id)`. Scholarship is joined by nobody; product 7 is
/// the one every enrollment above belongs to except Installment's, which
/// exists to give the second ON conjunct something to reject.
const PAYMENT_TYPES: [(i64, &str, i64); 3] = [
    (10, "Full", 7),
    (20, "Installment", 8),
    (30, "Scholarship", 7),
];

fn enrollment_store() -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("enrollment directory");
    let mut store = TableStore::open(
        directory.path(),
        enrollment_schema(),
        StoreOptions::default(),
    )
    .expect("open enrollment");
    store
        .bulk_ingest_snapshot(
            ENROLLMENTS
                .iter()
                .map(|(id, payment_type, status)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            payment_type.map_or(Value::Null, Value::Int64),
                            Value::Utf8((*status).to_owned()),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest enrollments");
    (directory, store)
}

fn payment_type_store() -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("payment type directory");
    let mut store = TableStore::open(
        directory.path(),
        payment_type_schema(),
        StoreOptions::default(),
    )
    .expect("open payment type");
    store
        .bulk_ingest_snapshot(
            PAYMENT_TYPES
                .iter()
                .map(|(id, name, product)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Int64(*id)]).expect("key"),
                        vec![
                            Value::Int64(*id),
                            Value::Utf8((*name).to_owned()),
                            Value::Int64(*product),
                        ],
                        u64::try_from(*id).expect("sequence"),
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest payment types");
    (directory, store)
}

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const ENROLLMENT_ID: TableId = TableId::new(1);
const PAYMENT_TYPE_ID: TableId = TableId::new(2);

fn catalog() -> CatalogSnapshot {
    let database = DatabaseEntry::new(
        DATABASE_ID,
        "app",
        [
            TableEntry::new(
                ENROLLMENT_ID,
                "enrollment",
                enrollment_schema(),
                TableStatistics::with_row_count(ENROLLMENTS.len() as u64),
            )
            .expect("enrollment entry")
            .with_key_columns([1])
            .expect("enrollment key"),
            TableEntry::new(
                PAYMENT_TYPE_ID,
                "payment_type",
                payment_type_schema(),
                TableStatistics::with_row_count(PAYMENT_TYPES.len() as u64),
            )
            .expect("payment type entry")
            .with_key_columns([1])
            .expect("payment type key"),
        ],
    )
    .expect("database");
    CatalogSnapshot::new([database]).expect("catalog")
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
    let (_enrollment_directory, enrollment) = enrollment_store();
    let (_payment_type_directory, payment_type) = payment_type_store();
    let enrollment_snapshot = enrollment.snapshot();
    let payment_type_snapshot = payment_type.snapshot();
    let catalog = catalog();
    let provider = SnapshotScanProvider::new([
        (DATABASE_ID, ENROLLMENT_ID, &enrollment_snapshot),
        (DATABASE_ID, PAYMENT_TYPE_ID, &payment_type_snapshot),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
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
                    .map(|column| {
                        column
                            .value(row)
                            .map_or_else(|| "MISSING".to_owned(), render)
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

/// The reported query, reduced to two of its eight count lanes. Grouping is
/// by `payment_type_id` alone, so the NULL payment type is one group and the
/// counts belong to the whole group.
#[test]
fn a_left_joined_dimension_name_reads_off_the_grouped_foreign_key() {
    let rows = run("SELECT PaymentType.name AS name, \
         COUNT(DISTINCT CASE WHEN e.status IN ('active', 'completed') THEN e.id END) AS running, \
         COUNT(DISTINCT CASE WHEN e.status = 'active' THEN e.id END) AS active \
         FROM enrollment e \
         LEFT JOIN payment_type PaymentType ON PaymentType.id = e.payment_type_id \
         GROUP BY e.payment_type_id ORDER BY name");
    assert_eq!(
        rows,
        [
            ["NULL", "1", "1"],
            ["Full", "3", "2"],
            ["Installment", "1", "1"],
        ],
        "one row per payment type, counts over the whole group"
    );
}

/// Grouping by a table's own primary key determines every other column of
/// that row - the shape the limitation entry used to name.
#[test]
fn the_primary_key_determines_the_rest_of_its_row() {
    let rows = run("SELECT e.id, e.status, COUNT(*) FROM enrollment e \
         GROUP BY e.id ORDER BY e.id");
    assert_eq!(
        rows,
        [
            ["1", "active", "1"],
            ["2", "completed", "1"],
            ["3", "active", "1"],
            ["4", "hold", "1"],
            ["5", "active", "1"],
            ["6", "active", "1"],
        ]
    );
}

/// A second ON conjunct the grouping key does not decide - the reported
/// query's `School.productId = PaymentType.productId` - does not undo the
/// first. Payment type 20 fails it, so its rows are NULL-complemented.
///
/// The name is deliberately not asserted: within a group whose join matched
/// for some rows and not others, `MySQL` returns an arbitrary one and so
/// does this. What must hold is the SHAPE - one row per group, with the
/// group's whole count beside it, rather than a group split by the name.
#[test]
fn an_undecided_join_conjunct_does_not_split_the_group() {
    let rows = run("SELECT COUNT(*), e.payment_type_id FROM enrollment e \
         LEFT JOIN payment_type PaymentType \
         ON PaymentType.id = e.payment_type_id AND PaymentType.product_id = 7 \
         GROUP BY e.payment_type_id ORDER BY e.payment_type_id");
    assert_eq!(rows, [["1", "NULL"], ["3", "10"], ["2", "20"]]);
    let named = run("SELECT PaymentType.name, COUNT(*) FROM enrollment e \
         LEFT JOIN payment_type PaymentType \
         ON PaymentType.id = e.payment_type_id AND PaymentType.product_id = 7 \
         GROUP BY e.payment_type_id ORDER BY e.payment_type_id");
    assert_eq!(
        named.len(),
        3,
        "three payment-type groups, whatever name each reports: {named:?}"
    );
    let counts: Vec<&str> = named.iter().map(|row| row[1].as_str()).collect();
    assert_eq!(counts, ["1", "3", "2"]);
}

/// The refusal still stands where nothing determines the column. Answering
/// this one with an arbitrary value would be worse than the error, because
/// `MySQL` reports the error too.
#[test]
fn an_undetermined_column_is_still_refused() {
    let statement =
        parse_statement("SELECT e.status, COUNT(*) FROM enrollment e GROUP BY e.payment_type_id")
            .expect("parse");
    assert!(
        Binder::new(&catalog(), Some("app"))
            .bind(&statement)
            .is_err()
    );
}
