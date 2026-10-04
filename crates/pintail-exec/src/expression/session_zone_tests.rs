use super::{CompiledExpr, evaluate_eager_scalar};
use crate::collation::Collation;
use crate::{ColumnVector, RecordBatch};
use pintail_sql::{BoundExpr, BoundExprKind, ScalarFunction};
use pintail_types::{DataType, Value};

#[test]
fn literal_session_zone_is_prepared_for_rows_and_the_batch_adapter() {
    let data_type = DataType::DateTime64 { fsp: 6 };
    let values = vec![
        Value::Null,
        Value::Utf8("0000-00-00 00:00:00.000000".into()),
        Value::Utf8("1970-01-01 00:00:01.000001".into()),
        Value::Utf8("2024-03-10 06:59:59.999999".into()),
        Value::Utf8("2024-03-10 07:00:00.000000".into()),
        Value::Utf8("2024-11-03 05:30:00.123456".into()),
        Value::Utf8("2024-11-03 06:30:00.123456".into()),
    ];
    let batch = RecordBatch::new(
        values.len(),
        vec![ColumnVector::new(data_type, values.clone()).expect("column")],
    )
    .expect("batch");
    for name in [
        "America/New_York",
        "america/new_york",
        "+05:30",
        "-08:00",
        "+00:00",
    ] {
        let zone = Value::Utf8(name.into());
        let expression = BoundExpr {
            kind: BoundExprKind::Scalar {
                function: ScalarFunction::SessionTimestamp,
                args: vec![
                    BoundExpr {
                        kind: BoundExprKind::Literal(values[2].clone()),
                        data_type: Some(data_type),
                        nullable: true,
                    },
                    BoundExpr {
                        kind: BoundExprKind::Literal(zone.clone()),
                        data_type: Some(DataType::Utf8),
                        nullable: false,
                    },
                ],
            },
            data_type: Some(data_type),
            nullable: true,
        };
        let mut compiled =
            CompiledExpr::compile(&expression, &[], Collation::default()).expect("compile");
        let CompiledExpr::Scalar {
            args, session_zone, ..
        } = &mut compiled
        else {
            panic!("scalar");
        };
        assert!(session_zone.is_some(), "literal zone must be prepared");
        args[0] = CompiledExpr::Column(0);
        let column = compiled
            .evaluate_column(&batch, Some(data_type))
            .expect("adapter");
        for (row, value) in values.iter().enumerate() {
            let expected = evaluate_eager_scalar(
                ScalarFunction::SessionTimestamp,
                &[value.clone(), zone.clone()],
                Some(data_type),
            )
            .expect("scalar conversion");
            assert_eq!(compiled.evaluate(&batch, row).expect("row"), expected);
            assert_eq!(column.value_owned(row), Some(expected));
        }
        assert_eq!(column.value_owned(0), Some(Value::Null));
        assert_eq!(column.value_owned(1), Some(values[1].clone()));
        if name == "America/New_York" {
            assert_eq!(
                column.value_owned(4),
                Some(Value::Utf8("2024-03-10 03:00:00.000000".into()))
            );
            assert_eq!(column.value_owned(5), column.value_owned(6));
        }
    }
}

#[test]
fn session_timestamp_preserves_dictionary_codes_and_zero_dates() {
    use crate::array::{StrColumn, ValidityMask};
    use crate::batch::TypedValues;
    let data_type = DataType::DateTime64 { fsp: 6 };
    let entries = [
        "0000-00-00 00:00:00.000000",
        "2024-11-03 05:30:00.123456",
        "2024-11-03 06:30:00.123456",
        "",
    ];
    let mut heap = Vec::new();
    let mut offsets = vec![0];
    for entry in entries {
        heap.extend_from_slice(entry.as_bytes());
        offsets.push(heap.len());
    }
    let codes = vec![0, 1, 2, 1, 2, 3, 3];
    let validity = ValidityMask::from_bools(&[true, true, true, true, true, false, true]);
    let input = ColumnVector::from_typed(
        data_type,
        TypedValues::Utf8(StrColumn::from_dictionary(
            &heap,
            &offsets,
            codes.clone(),
            validity.clone(),
        )),
        validity,
    );
    let batch = RecordBatch::new(codes.len(), vec![input]).expect("batch");
    let expression = CompiledExpr::Scalar {
        function: ScalarFunction::SessionTimestamp,
        args: vec![
            CompiledExpr::Column(0),
            CompiledExpr::Literal(Value::Utf8("America/New_York".into())),
        ],
        argument_types: vec![Some(data_type), Some(DataType::Utf8)],
        literal_regex: None,
        session_zone: super::temporal::ZoneReading::of("America/New_York"),
        variables: None,
        data_type: Some(data_type),
        collation: Collation::default(),
        overflow: None,
    };
    let output = expression
        .evaluate_vector_column_quietly(&batch, Some(data_type))
        .expect("dictionary kernel without row adapter");
    let (TypedValues::Utf8(text), _) = output.typed().expect("typed") else {
        panic!("text dictionary");
    };
    let (out_codes, out_entries) = text.dictionary().expect("dictionary retained");
    assert_eq!(out_codes, codes);
    assert_eq!(out_entries.len(), entries.len());
    for row in 0..codes.len() {
        assert_eq!(
            output.value_owned(row),
            Some(expression.evaluate(&batch, row).expect("row"))
        );
    }
    assert_eq!(output.value_owned(0), Some(Value::Utf8(entries[0].into())));
    assert_eq!(output.value_owned(1), output.value_owned(2));
    assert_eq!(output.value_owned(5), Some(Value::Null));
    assert_eq!(output.value_owned(6), Some(Value::Null));
}
