//! A string function whose result would be longer than the session's
//! `max_allowed_packet` answers NULL with warning 1301, as `MySQL` 8.4 does,
//! and its size is decided before the value is built. The session's limit is
//! set to 1024 bytes here, `MySQL`'s minimum, so every boundary is cheap to
//! reach; each expected row and warning list was read from `MySQL` 8.4 with
//! `max_allowed_packet` at 1024.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, TableSchema, Value};

/// One statement's row as the `mysql` client prints it, tab separated, and
/// the function each of its warnings names, in order.
fn run(sql: &str) -> (String, Vec<String>) {
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
    let _ = pintail_exec::take_session_conversion_warnings();
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 1 << 28, Collation::default()).expect("start");
    let batch = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .expect("one row");
    let row = batch.selection().selected_rows().next().expect("a row");
    let cells = batch
        .columns()
        .iter()
        .map(|column| match column.value(row).expect("cell") {
            Value::Null => "NULL".to_owned(),
            Value::Boolean(truth) => u8::from(*truth).to_string(),
            Value::Utf8(text) => text.clone(),
            Value::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Value::Int64(number) => number.to_string(),
            Value::UInt64(number) => number.to_string(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    let (warnings, count) = pintail_exec::take_session_conversion_warnings();
    assert_eq!(count, warnings.len() as u64, "{sql}");
    let suffix = format!(
        "() was larger than max_allowed_packet ({}) - truncated",
        pintail_exec::session_max_allowed_packet()
    );
    let functions = warnings
        .iter()
        .map(|warning| {
            assert_eq!(warning.code, 1301, "{sql}: {}", warning.message);
            assert_eq!(warning.sql_state, b"HY000");
            let function = warning
                .message
                .strip_prefix("Result of ")
                .and_then(|rest| rest.strip_suffix(suffix.as_str()))
                .unwrap_or_else(|| panic!("{sql}: {}", warning.message));
            function.to_owned()
        })
        .collect();
    (cells.join("\t"), functions)
}

#[test]
fn a_result_past_the_session_packet_is_null_with_a_warning() {
    pintail_exec::set_session_max_allowed_packet(Some(1024));
    let cases: [(&str, &str, &[&str]); 12] = [
        (
            "SELECT LENGTH(REPEAT('a', 1024)), REPEAT('a', 1025), LENGTH(REPEAT('ab', 512)), REPEAT('ab', 513), REPEAT('', 100000), REPEAT('a', 0), REPEAT('a', -1), REPEAT('a', 18446744073709551615)",
            "1024\tNULL\t1024\tNULL\t\t\t\tNULL",
            &["repeat", "repeat", "repeat"],
        ),
        (
            "SELECT LENGTH(SPACE(1024)), SPACE(1025), SPACE(-5), LENGTH(SPACE(0))",
            "1024\tNULL\t\t0",
            &["space"],
        ),
        // A pad is sized as its target times the widest character before it
        // is built: four bytes for text, one for a binary subject.
        (
            "SELECT LENGTH(LPAD('x', 256, 'y')), LPAD('x', 257, 'y'), LENGTH(RPAD('x', 256, 'y')), RPAD('x', 257, 'y'), LPAD('abc', 2000, ''), LPAD(REPEAT('a', 900), 2, 'x'), LENGTH(LPAD(CAST('x' AS BINARY), 1024, 'y')), LPAD(CAST('x' AS BINARY), 1025, 'y'), LPAD('x', -1, 'y')",
            "256\tNULL\t256\tNULL\tNULL\taa\t1024\tNULL\tNULL",
            &["lpad", "rpad", "lpad", "lpad"],
        ),
        (
            "SELECT LENGTH(INSERT('abc', 2, 1, REPEAT('b', 1022))), INSERT('abc', 2, 1, REPEAT('b', 1023)), INSERT('abc', 10, 1, REPEAT('b', 1023)), LENGTH(INSERT(REPEAT('a', 1024), 1, 1, 'b'))",
            "1024\tNULL\tabc\t1024",
            &["insert"],
        ),
        (
            "SELECT LENGTH(CONCAT(REPEAT('a', 1000), REPEAT('b', 24))), CONCAT(REPEAT('a', 1000), REPEAT('b', 25)), CONCAT(NULL, REPEAT('a', 1024), 'b'), LENGTH(CONCAT(1, REPEAT('a', 1023))), CONCAT(12, REPEAT('a', 1023))",
            "1024\tNULL\tNULL\t1024\tNULL",
            &["concat", "concat"],
        ),
        // A lone part is measured with a separator after it.
        (
            "SELECT LENGTH(CONCAT_WS(',', REPEAT('a', 1023), NULL)), CONCAT_WS(',', REPEAT('a', 1024)), CONCAT_WS(',', NULL, REPEAT('a', 1024)), LENGTH(CONCAT_WS(',', 'b', REPEAT('a', 1022))), CONCAT_WS(',', 'b', REPEAT('a', 1023)), CONCAT_WS(NULL, 'a'), CONCAT_WS(REPEAT(',', 1024), 'a', 'b')",
            "1023\tNULL\tNULL\t1024\tNULL\tNULL\tNULL",
            &["concat_ws", "concat_ws", "concat_ws", "concat_ws"],
        ),
        (
            "SELECT LENGTH(REPLACE(REPEAT('ab', 300), 'a', 'xx')), REPLACE(REPEAT('ab', 400), 'a', 'xx'), LENGTH(REPLACE(REPEAT('a', 1024), 'b', 'cc')), LENGTH(REPLACE(REPEAT('ab', 512), 'b', '')), REPLACE(REPEAT('a', 1024), 'a', NULL), LENGTH(REPLACE(REPEAT('a', 1024), '', 'xx')), LENGTH(REPLACE(REPEAT('ab', 341), 'a', 'xx')), REPLACE(REPEAT('ab', 342), 'a', 'xx')",
            "900\tNULL\t1024\t512\tNULL\t1024\t1023\tNULL",
            &["replace", "replace"],
        ),
        // The line breaks every 76 characters count.
        (
            "SELECT LENGTH(TO_BASE64(REPEAT('a', 756))), TO_BASE64(REPEAT('a', 757)), TO_BASE64(REPEAT('a', 759))",
            "1021\tNULL\tNULL",
            &["to_base64", "to_base64"],
        ),
        (
            "SELECT LENGTH(JSON_ARRAY(REPEAT('a', 1020))), JSON_ARRAY(REPEAT('a', 1021)), LENGTH(JSON_OBJECT('k', REPEAT('a', 1015))), JSON_OBJECT('k', REPEAT('a', 1016)), LENGTH(JSON_ARRAY(REPEAT('a', 1017), 1)), JSON_ARRAY(REPEAT('a', 1018), 1)",
            "1024\tNULL\t1024\tNULL\t1024\tNULL",
            &["json_array", "json_object", "json_array"],
        ),
        (
            "SELECT LENGTH(JSON_SET('{\"a\":1}', '$.a', REPEAT('b', 1015))), JSON_SET('{\"a\":1}', '$.a', REPEAT('b', 1016)), JSON_INSERT('{\"a\":1}', '$.b', REPEAT('b', 1016)), JSON_REPLACE('{\"a\":1}', '$.a', REPEAT('b', 1016)), LENGTH(JSON_INSERT('{\"a\":1}', '$.a', REPEAT('b', 1016)))",
            "1024\tNULL\tNULL\tNULL\t8",
            &["json_set", "json_insert", "json_replace"],
        ),
        (
            "SELECT LENGTH(JSON_MERGE_PATCH('{\"a\":1}', CONCAT('{\"b\":\"', REPEAT('b', 1007), '\"}'))), JSON_MERGE_PATCH('{\"a\":1}', CONCAT('{\"b\":\"', REPEAT('b', 1008), '\"}')), LENGTH(JSON_PRETTY(CONCAT('[\"', REPEAT('a', 1016), '\"]'))), JSON_PRETTY(CONCAT('[\"', REPEAT('a', 1017), '\"]'))",
            "1024\tNULL\t1024\tNULL",
            &["json_merge_patch", "json_pretty"],
        ),
        // HEX, JSON_QUOTE and UPPER are not limited: MySQL builds them at
        // any length, and only a limited function that reads one refuses.
        (
            "SELECT LENGTH(CAST(REPEAT('a', 10) AS BINARY(1024))), CAST(REPEAT('a', 10) AS BINARY(1025)), LENGTH(HEX(HEX(HEX(REPEAT('a', 200))))), CONCAT(HEX(REPEAT('a', 600)), ''), LENGTH(JSON_QUOTE(REPEAT('\"', 600))), LENGTH(UPPER(REPEAT('a', 1024)))",
            "1024\tNULL\t1600\tNULL\t1202\t1024",
            &["cast_as_binary", "concat"],
        ),
    ];
    let mut failures = Vec::new();
    for (sql, row, warnings) in cases {
        let (actual, raised) = run(sql);
        if actual != row || raised != warnings {
            failures.push(format!(
                "{sql}\n  MySQL:   {row:?} {warnings:?}\n  Pintail: {actual:?} {raised:?}"
            ));
        }
    }
    // At the default packet the same statements build what they ask for.
    pintail_exec::set_session_max_allowed_packet(None);
    let (row, raised) = run("SELECT LENGTH(REPEAT('a', 1025)), LENGTH(LPAD('x', 4096, 'y'))");
    assert_eq!((row.as_str(), raised.len()), ("1025\t4096", 0));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The bound a projection reserves for a result is no larger than the
/// packet allows, and nothing for a literal result past it: before, nested
/// REPLACE calls multiplied their bounds to tens of gigabytes and a
/// literal REPEAT past 4 KiB was an error.
#[test]
fn reservations_stop_at_the_packet() {
    let (row, raised) = run(
        "SELECT REPEAT('a', 67108865) IS NULL, LENGTH(REPLACE(REPLACE(REPLACE(REPEAT('ab', 2048), 'a', REPEAT('xy', 64)), 'q', REPEAT('w', 64)), 'r', REPEAT('s', 64)))",
    );
    assert_eq!(row, "1\t264192");
    assert_eq!(raised, ["repeat"]);
}
