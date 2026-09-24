//! Answers the `MariaDB` suite's replay caught drifting, pinned to what
//! `MySQL` 8.4 returns for each: zero-part dates read from numbers and from
//! text with any punctuation, EXTRACT on a short-part date, an introducer
//! over bytes that are not UTF-8, and a table-free subquery reading an
//! outer grouping column two levels up.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, TableSchema};

fn answer(sql: &str) -> String {
    let schema =
        TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema,
        TableStatistics::with_row_count(0),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("db")])
            .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = match parse_statement(sql)
        .map_err(|e| e.to_string())
        .and_then(|s| {
            Binder::new(&catalog, Some("app"))
                .bind(&s)
                .map_err(|e| e.to_string())
        }) {
        Ok(bound) => bound,
        Err(error) => return format!("BIND ERROR {error}"),
    };
    let physical = match PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    ) {
        Ok(p) => p,
        Err(e) => return format!("PLAN ERROR {e}"),
    };
    let mut execution =
        Execution::start(physical, &provider, 1 << 26, Collation::default()).expect("start");
    match execution.next_batch() {
        Ok(Some(batch)) => {
            let row = batch.selection().selected_rows().next().expect("row");
            (0..batch.columns().len())
                .map(|c| {
                    format!(
                        "{:?}",
                        batch.column(c).and_then(|col| col.value(row)).cloned()
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ")
        }
        Ok(None) => "NO ROWS".to_owned(),
        Err(error) => format!("EXEC ERROR {error}"),
    }
}

#[test]
fn zero_part_dates_and_short_dates_answer_as_mysql_does() {
    let loose = |sql: &str| {
        pintail_sql::with_parse_mode(
            pintail_sql::ParseMode::from_sql_mode("NO_ENGINE_SUBSTITUTION"),
            || answer(sql),
        )
    };
    for (sql, expected) in [
        ("SELECT CAST(0 AS DATE)", r#"Some(Utf8("0000-00-00"))"#),
        (
            "SELECT CAST('12:00:00-12.34.56' AS DATETIME)",
            r#"Some(Utf8("2012-00-00 12:34:56"))"#,
        ),
        (
            "SELECT CAST('12:00:00 12.34.56' AS DATETIME)",
            r#"Some(Utf8("2012-00-00 12:34:56"))"#,
        ),
        (
            "SELECT CAST(200012010000 AS DATETIME), CAST(200012010000 AS DATE)",
            r#"Some(Utf8("2020-00-12 01:00:00")) | Some(Utf8("2020-00-12"))"#,
        ),
        (
            "SELECT EXTRACT(HOUR_SECOND FROM CAST(200012010000 AS DATETIME))",
            "Some(Int64(10000))",
        ),
        (
            "SELECT EXTRACT(HOUR_SECOND FROM CAST(200012010000 AS DATE))",
            "Some(Int64(0))",
        ),
    ] {
        assert_eq!(loose(sql), expected, "{sql} without NO_ZERO_DATE");
    }
    // MySQL's default modes refuse the same zero parts.
    for sql in [
        "SELECT CAST(0 AS DATE)",
        "SELECT CAST('12:00:00-12.34.56' AS DATETIME)",
        "SELECT CAST(200012010000 AS DATE)",
    ] {
        assert_eq!(answer(sql), "Some(Null)", "{sql} under the default modes");
    }
    assert_eq!(
        answer(
            "SELECT HOUR('1-2-3'), EXTRACT(HOUR FROM '1-2-3'), EXTRACT(HOUR FROM '10:20:30'), EXTRACT(DAY FROM '1-2-3')"
        ),
        "Some(Int64(0)) | Some(Int64(0)) | Some(Int64(10)) | Some(Int64(3))"
    );
    assert_eq!(
        answer("SELECT STR_TO_DATE(CAST(_utf8'2001÷01÷01' AS CHAR),CAST(_utf8'%Y÷%m÷%d' AS CHAR))"),
        r#"Some(Utf8("2001-01-01"))"#
    );
}

fn table_answer(sql: &str) -> String {
    use pintail_types::{KeyPart, PrimaryKey, StoredRow, Value};
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "c", DataType::Int64, true),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            [Value::Int64(1), Value::Int64(2), Value::Null]
                .into_iter()
                .zip(0_u64..)
                .map(|(c, id)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), c],
                        id + 1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("rows");
    let entry = TableEntry::new(
        TableId::new(1),
        "t",
        schema,
        TableStatistics::with_row_count(3),
    )
    .expect("entry")
    .with_key_columns([1])
    .expect("key");
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("db")])
            .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = match parse_statement(sql)
        .map_err(|e| e.to_string())
        .and_then(|s| {
            Binder::new(&catalog, Some("app"))
                .bind(&s)
                .map_err(|e| e.to_string())
        }) {
        Ok(bound) => bound,
        Err(error) => return format!("BIND ERROR {error}"),
    };
    let physical = match PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    ) {
        Ok(p) => p,
        Err(e) => return format!("PLAN ERROR {e}"),
    };
    let mut execution = match Execution::start(physical, &provider, 1 << 26, Collation::default()) {
        Ok(e) => e,
        Err(e) => return format!("START ERROR {e}"),
    };
    let mut out = Vec::new();
    loop {
        match execution.next_batch() {
            Ok(Some(batch)) => {
                for row in batch.selection().selected_rows() {
                    out.push(
                        (0..batch.columns().len())
                            .map(|c| {
                                format!(
                                    "{:?}",
                                    batch.column(c).and_then(|col| col.value(row)).cloned()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("|"),
                    );
                }
            }
            Ok(None) => break,
            Err(error) => return format!("EXEC ERROR {error}"),
        }
    }
    out.join(" ; ")
}

#[test]
fn a_table_free_subquery_reads_an_outer_grouping_column() {
    // MySQL: 0, 0, NULL (the NULL group fails its HAVING).
    let mut rows = table_answer("SELECT (SELECT 0 GROUP BY c HAVING (SELECT c)) FROM t GROUP BY c")
        .split(" ; ")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(rows, ["Some(Int64(0))", "Some(Int64(0))", "Some(Null)"]);
}
