//! Where `MySQL` hands an ENUM or SET on as plain text, and where a value
//! that prints as its label still answers its position or member mask.
//!
//! An IF, CASE or NULLIF over such a column sorts, groups and prints as the
//! label, and in a numeric context answers the number of the branch it
//! chose. A scalar subquery, `ANY_VALUE` and a window function hand the label
//! on as a string, a SET's as much as an ENUM's.
//!
//! The declarations make label order, position order and mask order
//! disagree everywhere. Every expectation is `MySQL` 8.4's answer for this
//! data.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Positions: zeta 1, alpha 2, mid 3.
const GRADES: [&str; 3] = ["zeta", "alpha", "mid"];
/// Masks: x 1, b 2, a 4.
const MARKS: [&str; 3] = ["x", "b", "a"];

/// (id, grade, marks, lot, flag)
type ShelfRow = (u64, Option<&'static str>, Option<&'static str>, i64, i64);

const SHELF: [ShelfRow; 6] = [
    (1, Some("zeta"), Some("x,a"), 1, 1),
    (2, Some("alpha"), Some("b"), 1, 0),
    (3, Some("mid"), Some("a"), 2, 1),
    (4, Some("alpha"), Some("x,b"), 2, 0),
    (5, None, None, 3, 1),
    (6, Some("mid"), Some("b,a"), 3, 0),
];

/// (id, grade, marks)
const BIN: [(u64, &str, &str); 3] = [(1, "mid", "a"), (2, "zeta", "x"), (3, "alpha", "b,a")];

fn declared(first: u32) -> [Column; 2] {
    [
        Column::new(first, "grade", DataType::Utf8, true)
            .with_enum_labels(Some(GRADES.iter().map(ToString::to_string).collect())),
        Column::new(first + 1, "marks", DataType::Utf8, true)
            .with_set_members(Some(MARKS.iter().map(ToString::to_string).collect())),
    ]
}

fn shelf_schema() -> TableSchema {
    let [grade, marks] = declared(2);
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            grade,
            marks,
            Column::new(4, "lot", DataType::Int64, true),
            Column::new(5, "flag", DataType::Int64, true),
        ],
    )
    .expect("schema")
}

fn bin_schema() -> TableSchema {
    let [grade, marks] = declared(2);
    TableSchema::new(
        1,
        vec![Column::new(1, "id", DataType::UInt64, false), grade, marks],
    )
    .expect("schema")
}

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Utf8(value.to_owned()))
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn render(value: Option<&Value>) -> String {
    match value {
        Some(Value::Null) | None => "NULL".to_owned(),
        Some(Value::Utf8(text) | Value::Enum { label: text, .. }) => text.clone(),
        Some(Value::UInt64(number)) => number.to_string(),
        Some(Value::Int64(number)) => number.to_string(),
        Some(Value::Boolean(truth)) => u8::from(*truth).to_string(),
        Some(Value::Float64(number)) => number.get().to_string(),
        Some(other) => format!("{other:?}"),
    }
}

/// Runs `sql` and renders the rows as `a|b;c|d`.
fn run(sql: &str) -> String {
    let shelf_directory = tempfile::tempdir().expect("tempdir");
    let mut shelf = TableStore::open(
        shelf_directory.path(),
        shelf_schema(),
        StoreOptions::default(),
    )
    .expect("open");
    shelf
        .bulk_ingest_snapshot(
            SHELF
                .iter()
                .map(|(id, grade, marks, lot, flag)| {
                    stored(
                        *id,
                        vec![
                            Value::UInt64(*id),
                            text(*grade),
                            text(*marks),
                            Value::Int64(*lot),
                            Value::Int64(*flag),
                        ],
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let bin_directory = tempfile::tempdir().expect("tempdir");
    let mut bin = TableStore::open(bin_directory.path(), bin_schema(), StoreOptions::default())
        .expect("open");
    bin.bulk_ingest_snapshot(
        BIN.iter()
            .map(|(id, grade, marks)| {
                stored(
                    *id,
                    vec![Value::UInt64(*id), text(Some(grade)), text(Some(marks))],
                )
            })
            .collect(),
    )
    .expect("ingest");
    let (shelf_snapshot, bin_snapshot) = (shelf.snapshot(), bin.snapshot());
    let database_id = DatabaseId::new(1);
    let (shelf_id, bin_id) = (TableId::new(1), TableId::new(2));
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                shelf_id,
                "shelf",
                shelf_schema(),
                TableStatistics::with_row_count(SHELF.len() as u64),
            )
            .expect("entry"),
            TableEntry::new(
                bin_id,
                "bin",
                bin_schema(),
                TableStatistics::with_row_count(BIN.len() as u64),
            )
            .expect("entry"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, shelf_id, &shelf_snapshot),
        (database_id, bin_id, &bin_snapshot),
    ])
    .expect("provider");
    let statement = parse_statement(sql).unwrap_or_else(|error| panic!("parse {sql}: {error}"));
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .unwrap_or_else(|error| panic!("plan {sql}: {error}"));
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("execute {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| render(column.value(row)))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
    }
    rows.join(";")
}

fn check(cases: &[(&str, &str)]) {
    for (sql, expected) in cases {
        assert_eq!(run(sql), *expected, "{sql}");
    }
}

/// IF, CASE and NULLIF over an ENUM or SET, a truth value read from one,
/// and the plain text a subquery, `ANY_VALUE` or a window function hands on.
const CHOICES: &[(&str, &str)] = &[
    (
        "SELECT id, IF(flag, grade, grade) + 0, IF(flag, grade, 'alpha') + 0, IF(flag, grade, NULL) + 0 FROM shelf ORDER BY id",
        "1|1|1|1;2|2|0|NULL;3|3|3|3;4|2|0|NULL;5|NULL|NULL|NULL;6|3|0|NULL",
    ),
    (
        "SELECT id, CASE WHEN flag THEN grade ELSE grade END + 0, CASE flag WHEN 1 THEN grade ELSE 'zeta' END + 0, CASE WHEN flag THEN grade END + 0 FROM shelf ORDER BY id",
        "1|1|1|1;2|2|0|NULL;3|3|3|3;4|2|0|NULL;5|NULL|NULL|NULL;6|3|0|NULL",
    ),
    (
        "SELECT id, NULLIF(grade, 'alpha') + 0, NULLIF(grade, 2) + 0, NULLIF(marks, 'b') + 0 FROM shelf ORDER BY id",
        "1|1|1|5;2|NULL|NULL|NULL;3|3|3|4;4|NULL|NULL|3;5|NULL|NULL|NULL;6|3|3|6",
    ),
    (
        "SELECT id, IF(flag, marks, marks) + 0, CASE WHEN flag THEN marks ELSE grade END + 0 FROM shelf ORDER BY id",
        "1|5|5;2|2|2;3|4|4;4|3|2;5|NULL|NULL;6|6|3",
    ),
    (
        "SELECT id FROM shelf WHERE IF(flag, grade, grade) = 3 ORDER BY id",
        "3;6",
    ),
    (
        "SELECT id FROM shelf WHERE CASE WHEN flag THEN grade ELSE grade END >= 2 ORDER BY id",
        "2;3;4;6",
    ),
    (
        "SELECT id FROM shelf WHERE NULLIF(grade, 'zeta') = 2 ORDER BY id",
        "2;4",
    ),
    (
        "SELECT id FROM shelf WHERE IF(flag, marks, marks) = 5 ORDER BY id",
        "1",
    ),
    (
        "SELECT id, IF(flag, grade, grade) IN (1, 3), IF(flag, grade, grade) BETWEEN 2 AND 3, IF(flag, grade, grade) <=> 3 FROM shelf ORDER BY id",
        "1|1|0|0;2|0|1|0;3|1|1|1;4|0|1|0;5|NULL|NULL|0;6|1|1|1",
    ),
    (
        "SELECT id, -IF(flag, grade, grade), IF(flag, grade, grade) * 2, IF(flag, grade, grade) DIV 1, IF(flag, grade, grade) % 2 FROM shelf ORDER BY id",
        "1|-1|2|1|1;2|-2|4|2|0;3|-3|6|3|1;4|-2|4|2|0;5|NULL|NULL|NULL|NULL;6|-3|6|3|1",
    ),
    (
        "SELECT id, ROUND(IF(flag, grade, grade)), ABS(IF(flag, grade, grade)), FLOOR(IF(flag, marks, marks)), POW(IF(flag, grade, grade), 2) FROM shelf ORDER BY id",
        "1|1|1|5|1;2|2|2|2|4;3|3|3|4|9;4|2|2|3|4;5|NULL|NULL|NULL|NULL;6|3|3|6|9",
    ),
    (
        "SELECT id, IF(flag, grade, grade) | 0, IF(flag, marks, marks) & 3, IF(flag, grade, grade) << 1 FROM shelf ORDER BY id",
        "1|1|1|2;2|2|2|4;3|3|0|6;4|2|3|4;5|NULL|NULL|NULL;6|3|2|6",
    ),
    (
        "SELECT SUM(IF(flag, grade, grade)), AVG(IF(flag, grade, grade)), SUM(CASE WHEN flag THEN grade END), BIT_OR(IF(flag, marks, marks)) FROM shelf",
        "11|2.2|4|7",
    ),
    (
        "SELECT MIN(IF(flag, grade, grade)), MAX(IF(flag, grade, grade)), COUNT(IF(flag, grade, grade)) FROM shelf",
        "alpha|zeta|5",
    ),
    (
        "SELECT id, CAST(IF(flag, grade, grade) AS SIGNED), CAST(IF(flag, grade, grade) AS UNSIGNED), CAST(IF(flag, grade, grade) AS DECIMAL(5,1)), CAST(IF(flag, grade, grade) AS CHAR) FROM shelf ORDER BY id",
        "1|0|0|1.0|zeta;2|0|0|2.0|alpha;3|0|0|3.0|mid;4|0|0|2.0|alpha;5|NULL|NULL|NULL|NULL;6|0|0|3.0|mid",
    ),
    (
        "SELECT id, NOT IF(flag, grade, grade), IF(flag, grade, grade) AND 1, IF(IF(flag, grade, grade), 'y', 'n') FROM shelf ORDER BY id",
        "1|0|1|y;2|0|1|y;3|0|1|y;4|0|1|y;5|NULL|NULL|n;6|0|1|y",
    ),
    (
        "SELECT id, NOT grade, grade AND 1, IF(grade, 'y', 'n'), grade OR 0, grade XOR 1, NOT marks FROM shelf ORDER BY id",
        "1|0|1|y|1|0|0;2|0|1|y|1|0|0;3|0|1|y|1|0|0;4|0|1|y|1|0|0;5|NULL|NULL|n|NULL|NULL|NULL;6|0|1|y|1|0|0",
    ),
    ("SELECT id FROM shelf WHERE grade ORDER BY id", "1;2;3;4;6"),
    (
        "SELECT id FROM shelf WHERE IF(flag, grade, grade) ORDER BY id",
        "1;2;3;4;6",
    ),
    (
        "SELECT id, IF(flag, grade, grade) x FROM shelf ORDER BY x, id",
        "5|NULL;2|alpha;4|alpha;3|mid;6|mid;1|zeta",
    ),
    (
        "SELECT id, CASE WHEN flag THEN marks ELSE marks END x FROM shelf ORDER BY x, id",
        "5|NULL;3|a;2|b;6|b,a;1|x,a;4|x,b",
    ),
    (
        "SELECT IF(flag, grade, grade) x, COUNT(*) FROM shelf GROUP BY x ORDER BY x",
        "NULL|1;alpha|2;mid|2;zeta|1",
    ),
    (
        "SELECT id, CONCAT(IF(flag, grade, grade), '!'), IF(flag, grade, grade) = 'mid', IF(flag, grade, grade) < 'n', IFNULL(grade, 'mid') + 0, COALESCE(grade, grade) + 0 FROM shelf ORDER BY id",
        "1|zeta!|0|0|0|0;2|alpha!|0|1|0|0;3|mid!|1|1|0|0;4|alpha!|0|1|0|0;5|NULL|NULL|NULL|0|NULL;6|mid!|1|1|0|0",
    ),
    (
        "SELECT id, IF(flag, IF(flag, grade, 'x'), NULLIF(grade, 'mid')) + 0, NULLIF(IF(flag, grade, grade), 'mid') + 0, IF(flag, grade, 5) + 0 FROM shelf ORDER BY id",
        "1|1|1|1;2|2|2|5;3|3|NULL|3;4|2|2|5;5|NULL|NULL|NULL;6|NULL|NULL|5",
    ),
    (
        "SELECT id, IF(flag, grade, grade) = id, IF(flag, grade, grade) + id, IF(flag, grade, grade) IN (lot, id) FROM shelf ORDER BY id",
        "1|1|2|1;2|1|4|1;3|1|6|1;4|0|6|1;5|NULL|NULL|NULL;6|0|9|1",
    ),
    (
        "SELECT id, +grade, +IF(flag, grade, grade), +grade + 1 FROM shelf ORDER BY id",
        "1|zeta|zeta|2;2|alpha|alpha|3;3|mid|mid|4;4|alpha|alpha|3;5|NULL|NULL|NULL;6|mid|mid|4",
    ),
    (
        "SELECT id, (SELECT marks FROM bin WHERE bin.id = shelf.id) x FROM shelf ORDER BY x, id",
        "4|NULL;5|NULL;6|NULL;1|a;3|b,a;2|x",
    ),
    (
        "SELECT id, (SELECT marks FROM bin WHERE bin.id = shelf.id) + 0, (SELECT marks FROM bin WHERE bin.id = 2) + 0, (SELECT marks FROM bin WHERE bin.id = shelf.id) = 4 FROM shelf ORDER BY id",
        "1|0|0|0;2|0|0|0;3|0|0|0;4|NULL|0|NULL;5|NULL|0|NULL;6|NULL|0|NULL",
    ),
    (
        "SELECT lot, ANY_VALUE(marks) x, ANY_VALUE(marks) + 0 FROM shelf WHERE marks IS NOT NULL GROUP BY lot ORDER BY x, lot",
        "2|a|0;3|b,a|0;1|x,a|0",
    ),
    (
        "SELECT lot, (SELECT grade FROM bin WHERE bin.id = shelf.lot) x FROM shelf GROUP BY lot ORDER BY x, lot",
        "3|alpha;1|mid;2|zeta",
    ),
    (
        "SELECT lot, (SELECT MAX(grade) FROM bin WHERE bin.id = shelf.lot) x FROM shelf GROUP BY lot ORDER BY x, lot",
        "3|alpha;1|mid;2|zeta",
    ),
    (
        "SELECT lot, (SELECT marks FROM bin WHERE bin.id = shelf.lot) x FROM shelf GROUP BY lot ORDER BY x, lot",
        "1|a;3|b,a;2|x",
    ),
    (
        "SELECT lot, (SELECT grade FROM bin WHERE bin.id = shelf.lot) + 0 x, COUNT(*) FROM shelf GROUP BY lot ORDER BY lot",
        "1|0|2;2|0|2;3|0|2",
    ),
    (
        "SELECT id, (SELECT MAX(grade) FROM bin WHERE bin.id = shelf.id) x FROM shelf ORDER BY x, id",
        "4|NULL;5|NULL;6|NULL;3|alpha;1|mid;2|zeta",
    ),
    (
        "SELECT id, (SELECT MAX(marks) FROM bin WHERE bin.id = shelf.id) x FROM shelf ORDER BY x, id",
        "4|NULL;5|NULL;6|NULL;1|a;3|b,a;2|x",
    ),
    (
        "SELECT id, (SELECT MIN(grade) FROM bin WHERE bin.id <> shelf.id) x FROM shelf ORDER BY x, id",
        "1|alpha;2|alpha;4|alpha;5|alpha;6|alpha;3|mid",
    ),
    (
        "SELECT id, LAG(marks) OVER (ORDER BY id) l, LAG(marks) OVER (ORDER BY id) + 0, FIRST_VALUE(marks) OVER (ORDER BY id) + 0 FROM shelf ORDER BY l, id",
        "1|NULL|NULL|0;6|NULL|NULL|0;4|a|0|0;3|b|0|0;2|x,a|0|0;5|x,b|0|0",
    ),
    (
        "SELECT id FROM shelf WHERE (SELECT marks FROM bin WHERE bin.id = shelf.id) = 4 ORDER BY id",
        "",
    ),
    (
        "SELECT id FROM shelf ORDER BY (SELECT marks FROM bin WHERE bin.id = shelf.id), id",
        "4;5;6;1;3;2",
    ),
];

#[test]
fn a_choice_among_declared_values_answers_the_chosen_number() {
    check(CHOICES);
}
