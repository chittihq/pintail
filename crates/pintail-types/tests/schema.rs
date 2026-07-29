use pintail_types::{
    Column, DataType, KeyPart, PrimaryKey, SchemaError, StoredRow, TableSchema, Value,
};

#[test]
fn schema_accepts_a_well_typed_versioned_row() {
    let schema = TableSchema::new(
        7,
        vec![
            Column::new(10, "account_id", DataType::UInt64, false),
            Column::new(11, "display_name", DataType::Utf8, true),
        ],
    )
    .expect("valid schema");
    let row = StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(42)]).expect("primary key"),
        vec![Value::UInt64(42), Value::Utf8("Ada".into())],
        99,
        false,
    );

    schema.validate_row(&row).expect("well typed row");
    assert_eq!(schema.version(), 7);
    assert_eq!(row.version(), 99);
}

#[test]
fn schema_rejects_nulls_and_values_that_do_not_match_the_column() {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "note", DataType::Utf8, true),
        ],
    )
    .expect("valid schema");
    let key = PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("primary key");

    let null_id = StoredRow::new(
        key.clone(),
        vec![Value::Null, Value::Utf8("ok".into())],
        1,
        false,
    );
    assert_eq!(
        schema.validate_row(&null_id),
        Err(SchemaError::NullInRequiredColumn("id".into()))
    );

    let wrong_type = StoredRow::new(
        key,
        vec![Value::UInt64(1), Value::Binary(vec![1, 2, 3])],
        2,
        false,
    );
    assert_eq!(
        schema.validate_row(&wrong_type),
        Err(SchemaError::WrongType {
            column: "note".into(),
            expected: DataType::Utf8,
            actual: DataType::Binary,
        })
    );
}
