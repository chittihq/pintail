//! A column added with a default reads that default in every row stored
//! before it existed - in the row path, the packed and fused scans, filters,
//! grouping and joins - and a merge writes it into the rows it rewrites.
//!
//! The fixture stores rows in three segments under the first schema, adds
//! four columns with fills (text, integer, date, decimal) and one without,
//! then writes newer rows that carry values of their own: some flushed, some
//! still in the memtable.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const OLD_ROWS: u64 = 3_000;
const NEW_ROWS: u64 = 40;
const DECIMAL: DataType = DataType::Decimal {
    precision: 8,
    scale: 3,
};

fn base_columns() -> Vec<Column> {
    vec![
        Column::new(1, "id", DataType::UInt64, false),
        Column::new(2, "d", DataType::Int64, true),
    ]
}

fn schema_v1() -> TableSchema {
    TableSchema::new(1, base_columns()).expect("schema v1")
}

fn schema_v2() -> TableSchema {
    let mut columns = base_columns();
    // Placed between existing columns, as `AFTER d` and `FIRST` place them:
    // the store reads by stable ID, never by position.
    columns.insert(
        1,
        Column::new(3, "region", DataType::Utf8, true)
            .with_collation(Some("utf8mb4_0900_ai_ci".to_owned()))
            .with_absent_fill(Some(Value::Utf8("all".to_owned()))),
    );
    columns
        .push(Column::new(4, "qty", DataType::Int64, true).with_absent_fill(Some(Value::Int64(7))));
    columns.push(
        Column::new(5, "day", DataType::Date32, true)
            .with_absent_fill(Some(Value::Utf8("2020-02-29".to_owned()))),
    );
    columns.push(
        Column::new(6, "amt", DECIMAL, true)
            .with_absent_fill(Some(Value::Utf8("1.500".to_owned()))),
    );
    columns.push(Column::new(7, "note", DataType::Utf8, true));
    TableSchema::new(2, columns).expect("schema v2")
}

fn regions_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "code", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
            Column::new(2, "label", DataType::Utf8, true),
        ],
    )
    .expect("regions schema")
}

fn key(id: u64) -> PrimaryKey {
    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key")
}

fn old_row(id: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![Value::UInt64(id), Value::Int64((id % 10).cast_signed())],
        id,
        false,
    )
}

fn new_row(id: u64) -> StoredRow {
    StoredRow::new(
        key(id),
        vec![
            Value::UInt64(id),
            Value::Utf8("east".to_owned()),
            Value::Int64((id % 10).cast_signed()),
            Value::Int64(1),
            Value::Utf8("2024-01-01".to_owned()),
            Value::Utf8("2.250".to_owned()),
            Value::Utf8("n".to_owned()),
        ],
        id,
        false,
    )
}

/// The items table with old rows in segments under the first schema, the
/// schema evolved, and new rows: half flushed, half in the memtable.
fn items(directory: &std::path::Path) -> TableStore {
    let mut table =
        TableStore::open(directory, schema_v1(), StoreOptions::default()).expect("open v1");
    for chunk in 0..3 {
        let start = chunk * (OLD_ROWS / 3) + 1;
        table
            .ingest((start..start + OLD_ROWS / 3).map(old_row).collect())
            .expect("v1 ingest");
        table.flush().expect("v1 flush");
    }
    table.evolve_schema(schema_v2()).expect("evolve");
    let first = OLD_ROWS + 1;
    table
        .ingest((first..first + NEW_ROWS / 2).map(new_row).collect())
        .expect("v2 ingest");
    table.flush().expect("v2 flush");
    table
        .ingest(
            (first + NEW_ROWS / 2..first + NEW_ROWS)
                .map(new_row)
                .collect(),
        )
        .expect("v2 memtable ingest");
    table
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

fn run(items: &TableStore, sql: &str) -> Vec<Vec<String>> {
    let regions_dir = tempfile::tempdir().expect("temporary regions table");
    let mut regions = TableStore::open(
        regions_dir.path(),
        regions_schema(),
        StoreOptions::default(),
    )
    .expect("open regions");
    let region = |code: &str, label: &str| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Utf8(code.to_owned())]).expect("key"),
            vec![Value::Utf8(code.to_owned()), Value::Utf8(label.to_owned())],
            1,
            false,
        )
    };
    regions
        .bulk_ingest_snapshot(vec![region("all", "Everywhere"), region("east", "East")])
        .expect("bulk regions");
    let items_snapshot = items.snapshot();
    let regions_snapshot = regions.snapshot();
    let database_id = DatabaseId::new(31);
    let items_id = TableId::new(32);
    let regions_id = TableId::new(33);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                items_id,
                "items",
                items.schema().clone(),
                TableStatistics::with_row_count(OLD_ROWS + NEW_ROWS),
            )
            .expect("items entry"),
            TableEntry::new(
                regions_id,
                "regions",
                regions_schema(),
                TableStatistics::with_row_count(2),
            )
            .expect("regions entry"),
        ],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, items_id, &items_snapshot),
        (database_id, regions_id, &regions_snapshot),
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

fn answers(items: &TableStore) -> Vec<Vec<Vec<String>>> {
    [
        // Filter on the filled text column.
        "SELECT COUNT(*) FROM items WHERE region = 'all'",
        "SELECT COUNT(*) FROM items WHERE region IS NULL OR qty IS NULL OR day IS NULL",
        // Grouping on it, with aggregates over the other fills.
        "SELECT region, COUNT(*), SUM(qty), MIN(day), MAX(amt), COUNT(note) FROM items \
         GROUP BY region ORDER BY region",
        // A join keyed on the fill.
        "SELECT r.label, COUNT(*) FROM items i JOIN regions r ON r.code = i.region \
         GROUP BY r.label ORDER BY r.label",
        // Folds without grouping: the fused and packed scan paths.
        "SELECT COUNT(region), SUM(qty), SUM(amt), MIN(day), MAX(day) FROM items",
        "SELECT SUM(d) FROM items WHERE qty = 7 AND day = '2020-02-29' AND amt = 1.5",
        // The row path, through a point read and an ordered tail.
        "SELECT id, region, qty, day, amt, note FROM items WHERE id = 17",
        "SELECT id, region, qty FROM items ORDER BY id DESC LIMIT 2",
    ]
    .iter()
    .map(|sql| run(items, sql))
    .collect()
}

#[test]
fn rows_stored_before_a_column_was_added_read_its_fill_everywhere() {
    let directory = tempfile::tempdir().expect("temporary items table");
    let mut table = items(directory.path());
    let old = OLD_ROWS.to_string();
    let old_d: i64 = (1..=OLD_ROWS).map(|id| (id % 10).cast_signed()).sum();
    let filtered_d: i64 = old_d;
    let expected = vec![
        vec![vec![old.clone()]],
        vec![vec!["0".to_owned()]],
        vec![
            vec![
                "all".to_owned(),
                old.clone(),
                (OLD_ROWS * 7).to_string(),
                "2020-02-29".to_owned(),
                "1.500".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "east".to_owned(),
                NEW_ROWS.to_string(),
                NEW_ROWS.to_string(),
                "2024-01-01".to_owned(),
                "2.250".to_owned(),
                NEW_ROWS.to_string(),
            ],
        ],
        vec![
            vec!["East".to_owned(), NEW_ROWS.to_string()],
            vec!["Everywhere".to_owned(), old],
        ],
        vec![vec![
            (OLD_ROWS + NEW_ROWS).to_string(),
            (OLD_ROWS * 7 + NEW_ROWS).to_string(),
            format!("{}.000", OLD_ROWS * 3 / 2 + NEW_ROWS * 9 / 4),
            "2020-02-29".to_owned(),
            "2024-01-01".to_owned(),
        ]],
        vec![vec![filtered_d.to_string()]],
        vec![vec![
            "17".to_owned(),
            "all".to_owned(),
            "7".to_owned(),
            "2020-02-29".to_owned(),
            "1.500".to_owned(),
            "NULL".to_owned(),
        ]],
        vec![
            vec![
                (OLD_ROWS + NEW_ROWS).to_string(),
                "east".to_owned(),
                "1".to_owned(),
            ],
            vec![
                (OLD_ROWS + NEW_ROWS - 1).to_string(),
                "east".to_owned(),
                "1".to_owned(),
            ],
        ],
    ];
    assert_eq!(answers(&table), expected, "before the merge");

    // A merge writes the fill into every row it rewrites, after which the
    // answers do not depend on it any more.
    table.flush().expect("flush the memtable");
    let schema = table.schema().clone();
    drop(table);
    let mut table = TableStore::open(
        directory.path(),
        schema,
        StoreOptions {
            compaction_file_pressure: 2,
            ..StoreOptions::default()
        },
    )
    .expect("reopen to merge");
    for _ in 0..16 {
        if table.compact().expect("compact").output_path().is_none() {
            break;
        }
    }
    let rewritten = table.schema().clone();
    let without_fill = TableSchema::new(
        3,
        rewritten
            .columns()
            .iter()
            .map(|column| column.clone().with_absent_fill(None))
            .collect(),
    )
    .expect("schema without fills");
    let segments_lack_no_column = table
        .snapshot()
        .scan()
        .expect("scan merged")
        .iter()
        .all(|row| row.values()[1] != Value::Null);
    assert!(segments_lack_no_column);
    table.evolve_schema(without_fill).expect("drop the fills");
    assert_eq!(answers(&table), expected, "after the merge");
}
