//! Which comparisons an integer column takes exactly, and which it takes
//! through a double.
//!
//! Comparing a number with a string is a double comparison in `MySQL`. Two
//! places hide that rule, because they are the two `MySQL` optimizes: an
//! equality against a constant and an `IN` list of constants convert the
//! constant to the column's own exact type while the statement is prepared.
//! The conversion needs every item to convert, so one number in the list ends
//! it, and a simple `CASE x WHEN ...` never gets it at all.
//!
//! The distinction is invisible below 2^53, where a double still holds every
//! integer. These ids sit above it and differ by five, so a double cannot
//! tell them apart and every shape below answers differently depending on
//! which rule applies. Each expectation is `MySQL` 8.4's own answer for this
//! data, measured against a server started the way the replay harness starts
//! its oracle.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Three ids five apart, all above 2^53 (9007199254740992), so the three
/// share one `f64` and differ as integers.
const IDS: [u64; 3] = [
    97_716_021_308_405_770,
    97_716_021_308_405_775,
    97_716_021_308_405_780,
];

fn schema() -> TableSchema {
    TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).expect("schema")
}

fn run(sql: &str) -> Vec<String> {
    let directory = tempfile::tempdir().expect("temporary table");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open table");
    table
        .bulk_ingest_snapshot(
            IDS.iter()
                .enumerate()
                .map(|(index, id)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![Value::UInt64(*id)],
                        index as u64 + 1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk snapshot");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "q",
        schema(),
        TableStatistics::with_row_count(IDS.len() as u64),
    )
    .expect("table entry")
    .with_key_columns([1])
    .expect("key");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            let rendered = (0..batch.columns().len())
                .map(
                    |column| match batch.column(column).and_then(|column| column.value(row)) {
                        Some(Value::UInt64(number)) => number.to_string(),
                        Some(Value::Int64(number)) => number.to_string(),
                        Some(Value::Utf8(text)) => text.clone(),
                        Some(Value::Null) | None => "NULL".to_owned(),
                        Some(other) => format!("{other:?}"),
                    },
                )
                .collect::<Vec<_>>();
            rows.push(rendered.join(" "));
        }
    }
    rows
}

/// A list of constants, all of one kind, converts and compares exactly.
#[test]
fn an_all_constant_list_compares_the_integer_exactly() {
    assert_eq!(
        run("SELECT id FROM q WHERE id IN ('97716021308405775') ORDER BY id"),
        ["97716021308405775"]
    );
    assert_eq!(
        run("SELECT id FROM q WHERE id = '97716021308405775' ORDER BY id"),
        ["97716021308405775"]
    );
    assert_eq!(
        run("SELECT id FROM q WHERE id IN (1234, 97716021308405775) ORDER BY id"),
        ["97716021308405775"]
    );
    // Two strings still convert: the escape needs one kind, not one item.
    assert_eq!(
        run("SELECT id FROM q WHERE id IN ('97716021308405775','97716021308405770') ORDER BY id"),
        ["97716021308405770", "97716021308405775"]
    );
}

/// A mixed list is decided per ITEM, and Pintail does not do that yet.
///
/// Measured against `MySQL` 8.4: in a list holding both a string and a
/// number, each numeric item still compares exactly while each STRING item
/// compares as a double. So `id IN ('1234', 97716021308405775)` answers for
/// one id - the string matches nothing and the exact integer matches itself -
/// while `id IN (1234, '97716021308405775')` answers for three, the string
/// now being the wide one whose double covers all three. The two lists are
/// structurally identical; only which value is quoted differs.
///
/// Pintail carries one comparison type for a whole list, so it answers the
/// first of those correctly and the second exactly (one id) where `MySQL`
/// answers three. The case is recorded rather than asserted: writing down
/// today's wrong answer would make a later fix look like a regression.
#[test]
fn a_mixed_list_decides_each_item_separately() {
    assert_eq!(
        run("SELECT id FROM q WHERE id IN ('1234',97716021308405775) ORDER BY id"),
        ["97716021308405775"]
    );
    assert_eq!(
        run("SELECT id FROM q WHERE id IN (1234,'97716021308405775') ORDER BY id"),
        [
            "97716021308405770",
            "97716021308405775",
            "97716021308405780",
        ]
    );
}

/// `CASE x WHEN ...` is not `WHERE x = ...`: it gets no constant conversion,
/// so the plain number-against-string rule applies and one branch answers
/// for all three ids.
#[test]
fn a_simple_case_compares_its_operand_through_a_double() {
    assert_eq!(
        run("SELECT id, CASE id WHEN '97716021308405770' THEN 'hit' END FROM q ORDER BY id"),
        [
            "97716021308405770 hit",
            "97716021308405775 hit",
            "97716021308405780 hit",
        ]
    );
    // Several string branches: the first still takes every row.
    assert_eq!(
        run("SELECT id, CASE id WHEN '97716021308405770' THEN '70' \
             WHEN '97716021308405775' THEN '75' END FROM q ORDER BY id"),
        [
            "97716021308405770 70",
            "97716021308405775 70",
            "97716021308405780 70",
        ]
    );
    // An integer branch converts, so each row answers for itself.
    assert_eq!(
        run("SELECT id, CASE id WHEN 97716021308405770 THEN 'hit' END FROM q ORDER BY id"),
        [
            "97716021308405770 hit",
            "97716021308405775 NULL",
            "97716021308405780 NULL",
        ]
    );
    // A searched CASE is an ordinary equality and keeps the conversion.
    assert_eq!(
        run("SELECT id, CASE WHEN id = '97716021308405770' THEN 'hit' END FROM q ORDER BY id"),
        [
            "97716021308405770 hit",
            "97716021308405775 NULL",
            "97716021308405780 NULL",
        ]
    );
}
