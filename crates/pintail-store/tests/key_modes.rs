use pintail_store::{StoreOptions, TableStore};
use pintail_types::{
    Column, DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, TableSchema, Value,
};

#[test]
fn append_rowid_mode_retains_duplicate_source_rows_with_generated_keys() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let schema = schema(KeyMode::AppendRowId);
    {
        let mut table = TableStore::open(directory.path(), schema.clone(), StoreOptions::default())
            .expect("open append table");
        table
            .ingest(vec![source_row("first", 1), source_row("second", 2)])
            .expect("append duplicates");
        let rows = table.snapshot().scan().expect("scan appends");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key().parts(), [KeyPart::UInt64(1)]);
        assert_eq!(rows[1].key().parts(), [KeyPart::UInt64(2)]);
        table.flush().expect("flush append rows");
    }

    let mut reopened =
        TableStore::open(directory.path(), schema, StoreOptions::default()).expect("reopen");
    reopened
        .ingest(vec![source_row("third", 3)])
        .expect("continue row IDs");
    let rows = reopened.snapshot().scan().expect("scan reopened appends");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].key().parts(), [KeyPart::UInt64(3)]);
}

#[test]
fn append_rowid_cdc_replay_preserves_deterministic_keys() {
    let directory = tempfile::tempdir().expect("temporary table directory");
    let mut table = TableStore::open(
        directory.path(),
        schema(KeyMode::AppendRowId),
        StoreOptions::default(),
    )
    .expect("open append table");
    let row = StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(42)]).expect("CDC append key"),
        vec![Value::Utf8("once".into())],
        42,
        false,
    );
    table.ingest_cdc(vec![row.clone()]).expect("first replay");
    table.ingest_cdc(vec![row]).expect("duplicate replay");
    let rows = table.snapshot().scan().expect("scan replayed appends");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key().parts(), [KeyPart::UInt64(42)]);

    table
        .ingest(vec![source_row("next", 43)])
        .expect("ordinary append after CDC");
    let rows = table.snapshot().scan().expect("scan mixed appends");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].key().parts(), [KeyPart::UInt64(43)]);
}

#[test]
fn primary_and_unique_modes_resolve_duplicate_keys_by_max_version() {
    for key_mode in [KeyMode::Primary, KeyMode::Unique] {
        let directory = tempfile::tempdir().expect("temporary table directory");
        let mut table =
            TableStore::open(directory.path(), schema(key_mode), StoreOptions::default())
                .expect("open keyed table");
        table
            .ingest(vec![source_row("old", 1), source_row("new", 2)])
            .expect("ingest duplicate key");
        assert_eq!(
            table.snapshot().scan().expect("scan keyed rows"),
            vec![source_row("new", 2)]
        );
    }
}

fn schema(key_mode: KeyMode) -> TableSchema {
    TableSchema::with_key_mode(
        1,
        vec![Column::new(1, "value", DataType::Utf8, false)],
        key_mode,
    )
    .expect("schema")
}

fn source_row(value: &str, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Utf8("same-source-key".into())]).expect("source key"),
        vec![Value::Utf8(value.into())],
        version,
        false,
    )
}
