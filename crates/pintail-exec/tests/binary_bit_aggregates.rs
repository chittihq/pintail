//! Binary bit folds retain their declared identity width without input rows.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn query(sql: &str) -> Result<Vec<Vec<String>>, pintail_exec::ExecError> {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "payload", DataType::Binary, true).with_binary_width(Some(6)),
        ],
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).unwrap();
    table
        .bulk_ingest_snapshot(
            [
                Value::Binary(vec![0x01, 0x23, 0x45, 0x67, 0x89, 0x01]),
                Value::Binary(vec![0x01, 0x03, 0x45, 0x21, 0x01, 0x00]),
                Value::Null,
                Value::Binary(vec![0xab, 0xcd, 0xef]),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let id = index as u64 + 1;
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
                    vec![Value::UInt64(id), value],
                    id,
                    false,
                )
            })
            .collect(),
        )
        .unwrap();
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "items",
        schema,
        TableStatistics::with_row_count(4),
    )
    .unwrap();
    let catalog =
        CatalogSnapshot::new([DatabaseEntry::new(database_id, "app", [entry]).unwrap()]).unwrap();
    let provider = SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).unwrap();
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).unwrap())
        .unwrap();
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )?;
    let mut execution = Execution::start(plan, &provider, 64 * 1024 * 1024, Collation::default())?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch()? {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).unwrap() {
                        Value::Utf8(text) => text.clone(),
                        Value::UInt64(number) => number.to_string(),
                        value => format!("{value:?}"),
                    })
                    .collect(),
            );
        }
    }
    Ok(rows)
}

#[test]
fn binary_folds_preserve_bytes_and_empty_identities() {
    for (filter, expected) in [
        ("id <= 2", ["010345210100", "012345678901", "002000468801"]),
        ("id = 3", ["FFFFFFFFFFFF", "000000000000", "000000000000"]),
        ("id = 0", ["FFFFFFFFFFFF", "000000000000", "000000000000"]),
        ("id = 4", ["ABCDEF", "ABCDEF", "ABCDEF"]),
    ] {
        let sql = format!(
            "SELECT HEX(BIT_AND(payload)), HEX(BIT_OR(payload)), HEX(BIT_XOR(payload)) FROM items WHERE {filter}"
        );
        assert_eq!(
            query(&sql).unwrap(),
            vec![expected.map(str::to_owned).to_vec()],
            "{sql}"
        );
    }
}

#[test]
fn binary_folds_reject_unequal_non_null_lengths() {
    assert_eq!(
        query("SELECT BIT_AND(payload) FROM items"),
        Err(pintail_exec::ExecError::BinaryBitwiseLength)
    );
    assert_eq!(
        query("SELECT BIT_AND(CAST(NULL AS BINARY(512))) FROM items"),
        Err(pintail_exec::ExecError::BinaryBitwiseAggregateWidth)
    );
}

#[test]
fn binary_folds_retain_width_across_groups_and_windows() {
    assert_eq!(
        query("SELECT id, HEX(BIT_AND(payload)) FROM items GROUP BY id ORDER BY id").unwrap(),
        vec![
            vec!["1".to_owned(), "012345678901".to_owned()],
            vec!["2".to_owned(), "010345210100".to_owned()],
            vec!["3".to_owned(), "FFFFFFFFFFFF".to_owned()],
            vec!["4".to_owned(), "ABCDEF".to_owned()],
        ]
    );
    assert_eq!(
        query("SELECT HEX(BIT_AND(payload) OVER ()) FROM items WHERE id = 3").unwrap(),
        vec![vec!["FFFFFFFFFFFF".to_owned()]]
    );
    assert_eq!(
        query("SELECT HEX(BIT_AND(b)) FROM (SELECT payload AS b FROM items WHERE id = 3) p")
            .unwrap(),
        vec![vec!["FFFFFFFFFFFF".to_owned()]]
    );
}

#[test]
fn binary_strings_and_hex_literals_keep_their_aggregate_domains() {
    assert_eq!(query("SELECT HEX(BIT_AND(_binary'a')), BIT_AND(X'AA'), BIT_AND(X'010000000000000000'), HEX(BIT_AND(CAST(NULL AS BINARY(6)))) FROM items").unwrap(), vec![vec!["61".to_owned(), "170".to_owned(), "0".to_owned(), "FFFFFFFFFFFF".to_owned()]]);
}

#[test]
fn scalar_bit_operators_preserve_binary_width_and_literal_domains() {
    let rows = query("SELECT HEX(~payload), HEX(payload << 4), HEX(payload >> 4), BIT_COUNT(payload), HEX(payload & _binary X'FFFFFF') FROM items WHERE id=4").unwrap();
    assert_eq!(
        rows,
        vec![vec!["543210", "BCDEF0", "0ABCDE", "17", "ABCDEF"]]
    );
    for (expression, expected) in [
        ("HEX(X'ABCDEF' & X'123456')", "20446"),
        ("HEX(_binary X'ABCDEF' & X'123456')", "020446"),
        ("HEX(~X'0102')", "FFFFFFFFFFFFFEFD"),
        ("HEX(~_binary X'0102')", "FEFD"),
        ("HEX(_binary X'0102' << 16)", "0000"),
        ("HEX(_binary X'0102' >> -1)", "0000"),
        ("HEX(_binary'a' & 'b')", "0"),
        ("HEX(_binary X'0003' << (_binary X'38' | X'38'))", "0300"),
        ("BIT_COUNT(X'010000000000000001')", "0"),
        ("BIT_AND(X'010000000000000001')", "0"),
        ("BIT_AND(X'000000000000000001')", "1"),
        ("HEX(X'010000000000000001' | 0)", "0"),
        ("HEX(_binary X'0102' << 0)", "0102"),
        ("HEX(_binary X'0102' << 8)", "0200"),
        ("HEX(_binary X'0102' >> 8)", "0001"),
        ("BIT_COUNT(_binary X'010000000000000001')", "2"),
    ] {
        assert_eq!(
            query(&format!("SELECT {expression} FROM items WHERE id=1")).unwrap(),
            vec![vec![expected]],
            "{expression}"
        );
    }
    assert!(matches!(
        query("SELECT payload & _binary'a' FROM items WHERE id=4"),
        Err(pintail_exec::ExecError::BinaryBitwiseLength)
    ));
}
