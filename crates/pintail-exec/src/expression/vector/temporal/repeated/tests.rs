use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_sql::{Binder, parse_statement};
use pintail_types::{Column, DataType, TableSchema, Value};

use crate::batch::{ColumnVector, RecordBatch, SelectionMask};
use crate::collation::Collation;
use crate::execution::take_session_conversion_warnings;
use crate::expression::CompiledExpr;

fn compile(source: DataType, sql: &str) -> (CompiledExpr, DataType) {
    let schema = TableSchema::new(1, vec![Column::new(1, "v", source, true)]).expect("schema");
    let table = TableEntry::new(
        TableId::new(2),
        "t",
        schema,
        TableStatistics::with_row_count(2048),
    )
    .expect("table");
    let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let statement = parse_statement(&format!("SELECT {sql} FROM t")).expect("parse");
    let query = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let projection = &query.projection[0].expr;
    let expression =
        CompiledExpr::compile(projection, &query.tables[0].columns, Collation::default())
            .expect("compile");
    (expression, projection.data_type.expect("type"))
}

fn warnings() -> (Vec<(u16, String)>, u64) {
    let (messages, count) = take_session_conversion_warnings();
    (
        messages
            .into_iter()
            .map(|warning| (warning.code, warning.message))
            .collect(),
        count,
    )
}

fn check(source: DataType, inputs: &[Option<&str>], sql: &str, encoded: bool) {
    let values = (0..2048)
        .map(|row| {
            inputs[row % inputs.len()].map_or(Value::Null, |text| Value::Utf8(text.to_owned()))
        })
        .collect();
    let mut column = ColumnVector::new(source, values).expect("column");
    if encoded {
        let valid = crate::array::ValidityMask::from_bools(
            &(0..2048)
                .map(|row| inputs[row % inputs.len()].is_some())
                .collect::<Vec<_>>(),
        );
        let mut heap = Vec::new();
        let mut offsets = vec![0];
        for text in inputs {
            heap.extend_from_slice(text.unwrap_or_default().as_bytes());
            offsets.push(heap.len());
        }
        let codes = (0..2048)
            .map(|row| u32::try_from(row % inputs.len()).expect("small"))
            .collect();
        let text = crate::array::StrColumn::from_dictionary(&heap, &offsets, codes, valid.clone());
        column = ColumnVector::from_typed(source, crate::batch::TypedValues::Utf8(text), valid);
    }
    let mut batch = RecordBatch::new(2048, vec![column]).expect("batch");
    let mut selection = SelectionMask::all(2048);
    for row in (0..2048).step_by(7) {
        selection.set(row, false).expect("row");
    }
    batch.set_selection(selection).expect("selection");
    let (expression, declared) = compile(source, sql);
    let _ = warnings();
    let expected = batch
        .selection()
        .selected_rows()
        .map(|row| expression.evaluate(&batch, row).expect("row evaluation"))
        .collect::<Vec<_>>();
    let expected_warnings = warnings();
    let _ = crate::take_exec_counters();
    let actual = expression
        .evaluate_column(&batch, Some(declared))
        .expect("column path");
    let counters = crate::take_exec_counters();
    assert!(
        counters.temporal_values_evaluated > 0,
        "{sql}: repeated temporal path"
    );
    assert!(
        counters.temporal_values_evaluated <= inputs.len() as u64 * 2,
        "{sql}: evaluates distinct inputs only: {counters:?}"
    );
    let actual_warnings = warnings();
    assert_eq!(
        actual_warnings.1, expected_warnings.1,
        "{sql}: warning count"
    );
    assert_eq!(
        actual_warnings.0.len(),
        expected_warnings.0.len(),
        "{sql}: retained warnings"
    );
    for (actual, expected) in actual_warnings.0.iter().zip(&expected_warnings.0) {
        assert_eq!(actual, expected, "{sql}: warning order");
    }
    for (row, expected) in batch.selection().selected_rows().zip(expected) {
        assert_eq!(actual.value(row), Some(&expected), "{sql}: row {row}");
    }
}

#[test]
fn repeated_calendar_edges_match_values_and_warnings() {
    let dates = [
        Some("0000-00-00"),
        Some("2024-02-00"),
        Some("2024-02-30"),
        Some("0000-01-01"),
        Some("9999-12-31"),
        Some("2024-02-29"),
        None,
    ];
    let mut expressions = vec![
        "YEAR(v)".to_owned(),
        "MONTH(v)".to_owned(),
        "DAY(v)".to_owned(),
        "LAST_DAY(v)".to_owned(),
        "TO_DAYS(v)".to_owned(),
        "TO_SECONDS(v)".to_owned(),
        "DAYNAME(v)".to_owned(),
        "MONTHNAME(v)".to_owned(),
        "DATEDIFF('2024-03-05', v)".to_owned(),
        "DATE_ADD(v, INTERVAL 1 DAY)".to_owned(),
        "DATE_SUB(v, INTERVAL 1 MONTH)".to_owned(),
        "TIMESTAMPDIFF(MONTH, '2023-01-01', v)".to_owned(),
        "EXTRACT(WEEK FROM v)".to_owned(),
    ];
    for mode in 0..8 {
        expressions.push(format!("WEEK(v, {mode})"));
        expressions.push(format!("YEARWEEK(v, {mode})"));
    }
    for directive in [
        "%Y-%m-%d",
        "%a %b %W %M",
        "%U %u %V %v %X %x",
        "%j %D %f",
        "%H:%i:%s.%f",
        "%",
    ] {
        expressions.push(format!("DATE_FORMAT(v, '{directive}')"));
    }
    for coded in [false, true] {
        for sql in &expressions {
            check(DataType::Date32, &dates, sql, coded);
        }
        for fsp in [0, 3, 6] {
            let texts = dates
                .iter()
                .map(|date| {
                    date.map(|date| {
                        format!(
                            "{date} 23:59:59{}",
                            if fsp == 0 {
                                String::new()
                            } else {
                                format!(".{}", "9".repeat(usize::from(fsp)))
                            }
                        )
                    })
                })
                .collect::<Vec<_>>();
            let inputs = texts.iter().map(Option::as_deref).collect::<Vec<_>>();
            for sql in &expressions {
                check(DataType::DateTime64 { fsp }, &inputs, sql, coded);
            }
        }
    }
}

#[test]
fn repeated_durations_preserve_fraction_sign_and_clamping() {
    let times = [
        Some("-838:59:59.000000"),
        Some("-25:30:00.000001"),
        Some("00:00:00.000000"),
        Some("23:59:59.999999"),
        Some("838:59:59.000000"),
        None,
    ];
    for coded in [false, true] {
        for sql in [
            "TIME_TO_SEC(v)",
            "TIME_FORMAT(v, '%H %h %p %r %T %f')",
            "ADDTIME(v, '00:00:00.000001')",
            "SUBTIME(v, '00:00:00.000001')",
            "TIMEDIFF(v, '00:00:00')",
        ] {
            check(DataType::Time64 { fsp: 6 }, &times, sql, coded);
        }
    }
}

#[test]
fn repeated_text_keeps_the_bound_zero_date_policy() {
    let inputs = [
        Some("0000-00-00"),
        Some("2024-02-30"),
        Some("2024-02-00"),
        Some("2024-02-29"),
        None,
    ];
    for mode in ["", "NO_ZERO_DATE,NO_ZERO_IN_DATE", "ALLOW_INVALID_DATES"] {
        pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(mode), || {
            for sql in ["DATE_FORMAT(v, '%Y %m %d %W')", "WEEK(v, 7)", "LAST_DAY(v)"] {
                check(DataType::Utf8, &inputs, sql, false);
            }
        });
    }
}

#[test]
fn high_cardinality_declines_without_evaluating_or_warning() {
    let source = DataType::Date32;
    let values = (0..256)
        .map(|row| Value::Utf8(format!("invalid date {row}")))
        .collect();
    let batch = RecordBatch::new(
        256,
        vec![ColumnVector::new(source, values).expect("column")],
    )
    .expect("batch");
    let (expression, declared) = compile(source, "DAYNAME(v)");
    let _ = warnings();
    let _ = crate::take_exec_counters();
    assert!(
        expression
            .evaluate_vector_column_quietly(&batch, Some(declared))
            .is_none()
    );
    assert_eq!(crate::take_exec_counters().temporal_values_evaluated, 0);
    assert_eq!(warnings().1, 0);
}

#[test]
fn warning_effects_are_discarded_when_the_parent_declines() {
    let source = DataType::Date32;
    let values = vec![Value::Utf8("0000-00-00".into()); 128];
    let batch = RecordBatch::new(
        128,
        vec![ColumnVector::new(source, values).expect("column")],
    )
    .expect("batch");
    let (expression, declared) = compile(source, "DAYNAME(v)");
    let _ = warnings();
    assert!(
        expression
            .evaluate_vector_column_quietly(&batch, Some(declared))
            .is_none()
    );
    assert_eq!(
        warnings().1,
        0,
        "quiet attempts leave diagnostics unchanged"
    );
    let _ = expression
        .evaluate_column(&batch, Some(declared))
        .expect("normal evaluation");
    assert_eq!(warnings().1, 128, "one warning per selected row");
}

#[test]
fn selected_rows_only_determine_dictionary_and_warnings() {
    let source = DataType::Date32;
    let values = (0..256)
        .map(|row| {
            Value::Utf8(if row % 3 == 0 {
                "0000-00-00".to_owned()
            } else {
                format!("invalid date {row}")
            })
        })
        .collect();
    let mut batch = RecordBatch::new(
        256,
        vec![ColumnVector::new(source, values).expect("column")],
    )
    .expect("batch");
    let mut selection = SelectionMask::all(256);
    for row in 0..256 {
        selection.set(row, row % 3 == 0).expect("row");
    }
    batch.set_selection(selection).expect("selection");
    let (expression, declared) = compile(source, "DAYNAME(v)");
    let _ = warnings();
    crate::execution::record_conversion_warning("earlier warning".to_owned());
    let _ = crate::take_exec_counters();
    let answer = expression
        .evaluate_column(&batch, Some(declared))
        .expect("distinct selected input");
    assert_eq!(crate::take_exec_counters().temporal_values_evaluated, 1);
    let (messages, count) = warnings();
    assert_eq!(count, 87);
    assert_eq!(messages[0].1, "earlier warning");
    for row in 0..256 {
        assert_eq!(answer.value(row), Some(&Value::Null));
    }
}

#[test]
fn repeated_constant_conversion_warnings_keep_their_multiplicity() {
    check(
        DataType::Date32,
        &[Some("0000-00-00"), Some("2024-02-29"), None],
        "DATE_ADD(v, INTERVAL '1x' DAY)",
        false,
    );
}

#[test]
fn constant_temporal_subtrees_keep_arithmetic_vectorized() {
    let inputs = [
        Some("2024-01-01"),
        Some("2024-02-29"),
        Some("2024-03-01"),
        None,
    ];
    for sql in [
        "TO_DAYS(v) - TO_DAYS('2024-03-01')",
        "TO_SECONDS(v) - TO_SECONDS('2024-03-01')",
    ] {
        check(DataType::Date32, &inputs, sql, true);
    }
}

#[test]
fn nested_conversion_warnings_decline_to_keep_row_order() {
    let source = DataType::Date32;
    let batch = RecordBatch::new(
        2,
        vec![
            ColumnVector::new(
                source,
                vec![
                    Value::Utf8("0000-00-00".into()),
                    Value::Utf8("2024-00-00".into()),
                ],
            )
            .expect("column"),
        ],
    )
    .expect("batch");
    let (expression, declared) = compile(source, "TO_DAYS(v) + TO_SECONDS(v)");
    let _ = warnings();
    assert!(expression.evaluate_column(&batch, Some(declared)).is_none());
    assert_eq!(warnings().1, 0, "declined attempt leaves no diagnostics");
    for row in 0..2 {
        assert_eq!(expression.evaluate(&batch, row).expect("row"), Value::Null);
    }
    let (messages, count) = warnings();
    assert_eq!(count, 4);
    assert!(messages[0].1.contains("0000-00-00"));
    assert!(messages[1].1.contains("0000-00-00"));
    assert!(messages[2].1.contains("2024-00-00"));
    assert!(messages[3].1.contains("2024-00-00"));
}

#[test]
fn warning_constants_decline_without_leaking_diagnostics() {
    let source = DataType::Date32;
    let batch = RecordBatch::new(
        2,
        vec![ColumnVector::new(source, vec![Value::Utf8("0000-00-00".into()); 2]).expect("column")],
    )
    .expect("batch");
    let (expression, declared) = compile(source, "TO_DAYS(v) - TO_DAYS('0000-00-00')");
    let _ = warnings();
    assert!(expression.evaluate_column(&batch, Some(declared)).is_none());
    assert_eq!(warnings().1, 0);
    for row in 0..2 {
        assert_eq!(expression.evaluate(&batch, row).expect("row"), Value::Null);
    }
    assert_eq!(warnings().1, 4);
}
