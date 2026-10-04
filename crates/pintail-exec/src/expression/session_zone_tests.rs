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

/// Every zone a session can name reads canonical stored text the way the
/// general conversion does: across daylight-saving changes in both
/// directions, a half-hour change, the edges of the `TIMESTAMP` range and
/// every fraction width.
#[test]
fn canonical_reading_spells_what_the_general_conversion_spells() {
    use super::temporal::{READING_BYTES, ZoneCursor, ZoneReading};
    const SECOND: i64 = 1_000_000;
    let spans = [
        // The first TIMESTAMP instants and the day around them.
        (0, 2 * 86_400 * SECOND, 7 * 60 * SECOND + 1),
        // Both 2024 changes in New York and London, second by second
        // around the hour that moves.
        (1_710_050_400 * SECOND, 3 * 3_600 * SECOND, 13 * SECOND),
        (1_730_610_000 * SECOND, 3 * 3_600 * SECOND, 13 * SECOND),
        (1_711_846_800 * SECOND, 2 * 3_600 * SECOND, 11 * SECOND),
        (1_729_990_800 * SECOND, 2 * 3_600 * SECOND, 11 * SECOND),
        // The last TIMESTAMP instant.
        (
            2_147_483_647 * SECOND - 86_400 * SECOND,
            86_400 * SECOND,
            3_607 * SECOND,
        ),
        // Fifty years, a few times a month.
        (
            0,
            50 * 365 * 86_400 * SECOND,
            7 * 86_400 * SECOND + 3_601 * SECOND,
        ),
    ];
    let fractions = [(0_u8, 0_i64), (1, 500_000), (3, 123_000), (6, 999_999)];
    for name in [
        "America/New_York",
        "Europe/London",
        "Australia/Lord_Howe",
        "Asia/Kolkata",
        "UTC",
        "+05:30",
        "-08:00",
        "+00:00",
    ] {
        let zone = ZoneReading::of(name).expect("zone");
        let mut cursor = ZoneCursor::new(zone);
        let mut read = 0;
        for (start, length, step) in spans {
            let mut instant = start;
            while instant <= start + length {
                for (fsp, fraction) in fractions {
                    let micros = instant + fraction;
                    let stored = pintail_types::format_datetime_micros(
                        micros - micros % 10_i64.pow(6 - u32::from(fsp)),
                        fsp,
                    )
                    .expect("stored text");
                    let mut spelled = [0; READING_BYTES];
                    let fast = cursor
                        .read_canonical(stored.as_bytes(), &mut spelled)
                        .map(|reading| String::from_utf8(reading.to_vec()).expect("ascii"));
                    let general = zone.read_value_generally(&stored);
                    assert_eq!(fast.map(Value::Utf8), Some(general), "{name} {stored}");
                    read += 1;
                }
                instant += step;
            }
        }
        assert!(read > 10_000, "{name}: {read}");
    }
    // The zero TIMESTAMP and text in another shape are left to the
    // general conversion.
    let mut cursor = ZoneCursor::new(ZoneReading::of("America/New_York").expect("zone"));
    let mut spelled = [0; READING_BYTES];
    for other in [
        "0000-00-00 00:00:00",
        "2024-02-30 00:00:00",
        "2024-01-01",
        " 2024-01-01 00:00:00",
        "2024-01-01 00:00:00.",
        "2024-01-01 00:00:00.1234567",
    ] {
        assert!(
            cursor
                .read_canonical(other.as_bytes(), &mut spelled)
                .is_none(),
            "{other}"
        );
    }
}

/// A stored `TIMESTAMP(6)` column on the text carrier, as a column holding
/// the zero `TIMESTAMP` is: plain, or coded by distinct value.
fn stored_text_column(values: &[Option<&str>], coded: bool) -> ColumnVector {
    use crate::array::{StrColumn, ValidityMask};
    use crate::batch::TypedValues;
    let data_type = DataType::DateTime64 { fsp: 6 };
    let validity =
        ValidityMask::from_bools(&values.iter().map(Option::is_some).collect::<Vec<_>>());
    let text = if coded {
        let mut entries: Vec<&str> = Vec::new();
        let row_codes = values
            .iter()
            .map(|value| {
                let value = value.unwrap_or("");
                let code = entries
                    .iter()
                    .position(|entry| *entry == value)
                    .unwrap_or_else(|| {
                        entries.push(value);
                        entries.len() - 1
                    });
                u32::try_from(code).expect("code")
            })
            .collect::<Vec<_>>();
        let mut heap = Vec::new();
        let mut offsets = vec![0];
        for entry in entries {
            heap.extend_from_slice(entry.as_bytes());
            offsets.push(heap.len());
        }
        StrColumn::from_dictionary(&heap, &offsets, row_codes, validity.clone())
    } else {
        let mut text = StrColumn::default();
        for value in values {
            text.push(value.unwrap_or("").as_bytes());
        }
        text
    };
    ColumnVector::from_typed(data_type, TypedValues::Utf8(text), validity)
}

fn session_reading(zone: &str) -> CompiledExpr {
    let data_type = DataType::DateTime64 { fsp: 6 };
    CompiledExpr::Scalar {
        function: ScalarFunction::SessionTimestamp,
        args: vec![
            CompiledExpr::Column(0),
            CompiledExpr::Literal(Value::Utf8(zone.into())),
        ],
        argument_types: vec![Some(data_type), Some(DataType::Utf8)],
        literal_regex: None,
        session_zone: super::temporal::ZoneReading::of(zone),
        variables: None,
        data_type: Some(data_type),
        collation: Collation::default(),
        overflow: None,
    }
}

/// Stored instants around the edges a named zone has: NULL, the zero
/// `TIMESTAMP`, the first instants, the hour New York skips and the hour
/// it repeats (two instants that read alike), and the last instant.
const EDGES: [Option<&str>; 11] = [
    None,
    Some("0000-00-00 00:00:00.000000"),
    Some("1970-01-01 00:00:01.000000"),
    Some("1970-01-01 05:00:00.000000"),
    Some("2024-03-10 06:59:59.999999"),
    Some("2024-03-10 07:00:00.000000"),
    Some("2024-11-03 05:30:00.123456"),
    Some("2024-11-03 06:30:00.123456"),
    Some("2038-01-19 03:14:07.999999"),
    Some("2024-11-03 05:30:00.123456"),
    None,
];

#[test]
fn stored_text_is_read_in_a_session_zone_without_the_general_conversion() {
    let batch =
        RecordBatch::new(EDGES.len(), vec![stored_text_column(&EDGES, false)]).expect("batch");
    for zone in ["America/New_York", "Europe/London", "+05:30", "-08:00"] {
        let reading = session_reading(zone);
        let _ = crate::take_exec_counters();
        let column = reading
            .evaluate_vector_column_quietly(&batch, Some(DataType::DateTime64 { fsp: 6 }))
            .expect("text kernel");
        assert_eq!(
            crate::take_exec_counters().session_texts_read,
            EDGES.len() as u64,
            "{zone}: read by the text kernel"
        );
        for row in 0..EDGES.len() {
            assert_eq!(
                column.value_owned(row),
                Some(reading.evaluate(&batch, row).expect("row")),
                "{zone} row {row}"
            );
        }
    }
    let reading = session_reading("America/New_York");
    let column = reading
        .evaluate_vector_column_quietly(&batch, Some(DataType::DateTime64 { fsp: 6 }))
        .expect("text kernel");
    let expected = [
        Value::Null,
        Value::Utf8("0000-00-00 00:00:00.000000".into()),
        Value::Utf8("1969-12-31 19:00:01.000000".into()),
        Value::Utf8("1970-01-01 00:00:00.000000".into()),
        Value::Utf8("2024-03-10 01:59:59.999999".into()),
        Value::Utf8("2024-03-10 03:00:00.000000".into()),
        Value::Utf8("2024-11-03 01:30:00.123456".into()),
        Value::Utf8("2024-11-03 01:30:00.123456".into()),
        Value::Utf8("2038-01-18 22:14:07.999999".into()),
    ];
    for (row, value) in expected.into_iter().enumerate() {
        assert_eq!(column.value_owned(row), Some(value), "row {row}");
    }
}

#[test]
fn a_session_comparison_over_stored_text_reads_rows_without_converting_them() {
    use pintail_sql::BinaryOp;
    let literals = [
        "2024-11-03 01:30:00.123456",
        "1970-01-01 00:00:00.000000",
        "1969-12-31 19:00:01.000000",
        "2024-03-10 03:00:00.000000",
        "2024-03-10 02:30:00.000000",
        "2038-01-18 22:14:07.999999",
        "0000-00-00 00:00:00.000000",
        "1970-01-01 00:00:01",
    ];
    let ops = [
        BinaryOp::Equal,
        BinaryOp::NotEqual,
        BinaryOp::Less,
        BinaryOp::LessOrEqual,
        BinaryOp::Greater,
        BinaryOp::GreaterOrEqual,
    ];
    for coded in [false, true] {
        let batch =
            RecordBatch::new(EDGES.len(), vec![stored_text_column(&EDGES, coded)]).expect("batch");
        for zone in ["America/New_York", "Europe/London", "+05:30", "-08:00"] {
            let reading = session_reading(zone);
            for literal in literals {
                for op in ops {
                    let _ = crate::take_exec_counters();
                    let mask = reading
                        .session_comparison_mask(&batch, op, &Value::Utf8(literal.into()))
                        .expect("text comparison kernel");
                    assert_eq!(
                        crate::take_exec_counters().session_texts_read,
                        EDGES.len() as u64,
                        "{zone} {op:?} {literal}: decided by the text kernel"
                    );
                    for row in 0..EDGES.len() {
                        let expected = match reading.evaluate(&batch, row).expect("row") {
                            Value::Utf8(read) => {
                                let ordering = read.as_bytes().cmp(literal.as_bytes());
                                match op {
                                    BinaryOp::Equal => ordering.is_eq(),
                                    BinaryOp::NotEqual => ordering.is_ne(),
                                    BinaryOp::Less => ordering.is_lt(),
                                    BinaryOp::LessOrEqual => ordering.is_le(),
                                    BinaryOp::Greater => ordering.is_gt(),
                                    _ => ordering.is_ge(),
                                }
                            }
                            _ => false,
                        };
                        assert_eq!(
                            mask.is_selected(row),
                            expected,
                            "{zone} coded={coded} row {row} {op:?} {literal}"
                        );
                    }
                }
            }
        }
        // The repeated hour: both instants New York reads as 01:30 match it,
        // and the hour it skips matches nothing.
        let reading = session_reading("America/New_York");
        let equal = |literal: &str| {
            let mask = reading
                .session_comparison_mask(&batch, BinaryOp::Equal, &Value::Utf8(literal.into()))
                .expect("mask");
            (0..EDGES.len())
                .filter(|row| mask.is_selected(*row))
                .collect::<Vec<_>>()
        };
        assert_eq!(equal("2024-11-03 01:30:00.123456"), vec![6, 7, 9]);
        assert_eq!(equal("2024-03-10 02:30:00.000000"), Vec::<usize>::new());
        assert_eq!(equal("0000-00-00 00:00:00.000000"), vec![1]);
    }
}
