//! Scalar answers the upstream suite replay found diverging from `MySQL`
//! 8.4. Each expectation below is the oracle's own answer, so a change here
//! is a change in parity, not in taste.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "clock", DataType::Time64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

/// The single value `SELECT <expression> FROM one` answers, rendered as text.
fn scalar(expression: &str) -> String {
    std::panic::catch_unwind(|| evaluate(expression)).unwrap_or_else(|panic| {
        panic
            .downcast_ref::<String>()
            .map_or_else(|| "error".to_owned(), |message| format!("error {message}"))
    })
}

fn evaluate(expression: &str) -> String {
    evaluate_rows(expression, 1)
}

fn evaluate_rows(expression: &str, rows: u64) -> String {
    evaluate_rows_after_plan(expression, rows, || {})
}

fn evaluate_rows_after_plan(expression: &str, rows: u64, after_plan: impl FnOnce()) -> String {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(
            (1..=rows)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            match id {
                                2 => Value::Utf8("-11:11:11".to_owned()),
                                3 => Value::Null,
                                _ => Value::Utf8("11:11:11".to_owned()),
                            },
                        ],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "one",
        schema(),
        TableStatistics::with_row_count(rows),
    )
    .expect("entry");
    let database = DatabaseEntry::new(database_id, "app", [entry]).expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let sql = format!("SELECT {expression} FROM one");
    let statement = parse_statement(&sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    after_plan();
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut values = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            values.push(match batch.columns()[0].value(row) {
                Some(Value::Null) | None => "NULL".to_owned(),
                Some(Value::Utf8(text)) => text.clone(),
                Some(Value::Int64(number)) => number.to_string(),
                Some(Value::UInt64(number)) => number.to_string(),
                Some(Value::Float64(number)) => format!("float {}", number.get()),
                Some(other) => format!("{other:?}"),
            });
        }
    }
    assert_eq!(values.len(), 1, "{sql}");
    values.remove(0)
}

fn assert_answers(cases: &[(&str, &str)]) {
    let failures = cases
        .iter()
        .filter_map(|(expression, expected)| {
            let actual = scalar(expression);
            (actual != *expected).then(|| format!("{expression}: {actual} (MySQL {expected})"))
        })
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn string_boundaries_match_for_literals_and_column_expressions() {
    for text in ["'pearl'", "REPEAT('pearl', id)"] {
        assert_answers(&[
            (&format!("REPLACE({text}, '', 'x')"), "pearl"),
            (&format!("INSERT({text}, 6, 0, 'x')"), "pearl"),
            (&format!("SUBSTRING({text}, -6)"), ""),
            (&format!("SUBSTRING({text}, -5, 2)"), "pe"),
            (&format!("INSERT({text}, 5, 0, 'x')"), "pearxl"),
        ]);
    }
    assert_answers(&[
        ("REPLACE('é猫', '', 'x')", "é猫"),
        ("SUBSTRING('é猫', -3)", ""),
        ("SUBSTRING('é猫', -2, 1)", "é"),
        ("INSERT('é猫', 3, 0, 'x')", "é猫x"),
        ("INSERT('', 1, 0, 'x')", ""),
    ]);
}

#[test]
fn binary_string_functions_preserve_bytes_and_result_types() {
    for bytes in ["X'FF0080'", "IF(id = 1, X'FF0080', NULL)"] {
        assert_answers(&[
            (&format!("HEX(LEFT({bytes}, 2))"), "FF00"),
            (&format!("HEX(RIGHT({bytes}, 2))"), "0080"),
            (&format!("HEX(REPLACE({bytes}, X'FF', X'FE'))"), "FE0080"),
            (&format!("HEX(REPLACE({bytes}, X'', X'FE'))"), "FF0080"),
            (&format!("HEX(INSERT({bytes}, 2, 1, X'FE'))"), "FFFE80"),
            (&format!("HEX(INSERT({bytes}, 4, 0, X'FE'))"), "FF0080"),
        ]);
    }
    assert_answers(&[
        ("LEFT(X'FF0080', 2)", "Binary([255, 0])"),
        ("RIGHT(X'FF0080', 2)", "Binary([0, 128])"),
        ("REPLACE(X'FF0080', X'FF', X'FE')", "Binary([254, 0, 128])"),
        ("INSERT(X'FF0080', 2, 1, X'FE')", "Binary([255, 254, 128])"),
        ("HEX(INSERT('é猫', 2, 1, X'20'))", "C3A920"),
        ("REPLACE('é猫', '猫', X'C3A9')", "éé"),
        ("HEX(LEFT(X'FF0080', 0))", ""),
        ("HEX(RIGHT(X'FF0080', 8))", "FF0080"),
        ("HEX(REPLACE(X'FF', NULL, X'80'))", "NULL"),
    ]);
}

#[test]
fn insert_uses_subject_charset_and_byte_boundary() {
    for text in ["'é猫'", "REPEAT('é猫', id)"] {
        assert_answers(&[
            (&format!("HEX(INSERT({text}, 2, 1, X'20'))"), "C3A920"),
            (&format!("HEX(INSERT({text}, 2, 1, X'C3A9'))"), "C3A9C3A9"),
            (&format!("INSERT({text}, 3, 0, 'x')"), "é猫x"),
            (&format!("INSERT({text}, 4, 0, 'x')"), "é猫"),
        ]);
        assert!(scalar(&format!("INSERT({text}, 2, 1, X'FF')")).starts_with("error"));
        assert!(scalar(&format!("REPLACE({text}, '猫', X'FF')")).starts_with("error"));
    }
    assert_answers(&[
        ("INSERT('é', 2, 1, X'20')", "é "),
        ("HEX(INSERT(X'C3A9E78CAB', 2, 1, ' '))", "C320E78CAB"),
    ]);
}

#[test]
fn a_binary_cast_holds_exactly_its_declared_bytes() {
    assert_answers(&[
        ("HEX(CAST('a' AS BINARY(2)))", "6100"),
        ("HEX(CAST('abc' AS BINARY(2)))", "6162"),
        ("HEX(CAST('ab' AS BINARY))", "6162"),
        ("LENGTH(CAST('' AS BINARY(3)))", "3"),
    ]);
}

#[test]
fn a_temporal_value_reads_as_its_digits_in_numeric_context() {
    assert_answers(&[
        ("SEC_TO_TIME(9001)+0", "23001"),
        ("CAST(DATE'2007-02-03' AS SIGNED)", "20070203"),
        (
            "CAST(TIMESTAMP'2007-02-03 04:05:06' AS UNSIGNED)",
            "20070203040506",
        ),
        (
            "CAST(TIMESTAMP'2007-02-03 04:05:06.5' AS SIGNED)",
            "20070203040507",
        ),
        ("CAST(CAST('01-01-01' AS DATE) AS SIGNED)", "20010101"),
        ("DATE'2007-02-03' + 0", "20070203"),
        ("TIMESTAMP'2007-02-03 04:05:06' + 0", "20070203040506"),
        ("TIMESTAMP'2007-02-03 04:05:06.25' + 0", "20070203040506.25"),
        ("CAST(TIME'01:02:03' AS SIGNED)", "10203"),
        ("CAST(DATE'2007-02-03' AS DECIMAL(10,1))", "20070203.0"),
        ("DATE'2007-02-03' * 2", "40140406"),
    ]);
}

#[test]
fn a_date_part_reads_numbers_short_times_and_two_digit_years() {
    assert_answers(&[
        ("DAYOFMONTH(19970323)", "23"),
        ("QUARTER(980303)", "1"),
        ("SECOND(230322)", "22"),
        ("WEEK(19980101)", "0"),
        ("MINUTE('23:03:22')", "3"),
        ("HOUR('23:03:22')", "23"),
        ("YEAR('98-02-03')", "1998"),
        ("YEAR('69-02-03')", "2069"),
        ("YEAR('70-02-03')", "1970"),
        ("MONTH(20010203040506)", "2"),
    ]);
}

#[test]
fn an_integer_cast_of_text_is_exact() {
    assert_answers(&[
        ("CAST(REPEAT('1',20) AS UNSIGNED)", "11111111111111111111"),
        (
            "CAST('18446744073709551615' AS UNSIGNED)",
            "18446744073709551615",
        ),
        (
            "CAST('9223372036854775807' AS SIGNED)",
            "9223372036854775807",
        ),
        ("CAST('12.7abc' AS UNSIGNED)", "12"),
    ]);
}

#[test]
fn time_arithmetic_distinguishes_date_text_from_typed_dates() {
    assert_answers(&[
        ("ADDTIME('2021-01-01','01:01:01')", "01:21:22"),
        (
            "ADDTIME(DATE'2021-01-01','01:01:01')",
            "2021-01-01 01:01:01",
        ),
        (
            "ADDTIME('2021-01-01 00:00:00','01:01:01')",
            "2021-01-01 01:01:01",
        ),
        ("SUBTIME('2021-01-01','01:01:01')", "-00:40:40"),
        ("ADDTIME('2021xyz','01:01:01')", "01:21:22"),
        ("ADDTIME('123456-01-01','01:01:01')", "13:35:57"),
        ("ADDTIME('20210101','01:01:01')", "838:59:59"),
        ("ADDTIME('2021/01/01','01:01:01')", "01:21:22"),
    ]);
}

#[test]
fn decimal_integer_casts_round_and_saturate_at_integer_bounds() {
    assert_answers(&[
        (
            "CAST(19999999999999999999 AS SIGNED)",
            "9223372036854775807",
        ),
        (
            "CAST(-19999999999999999999 AS SIGNED)",
            "-9223372036854775808",
        ),
        (
            "CAST(9223372036854775808.0 AS SIGNED)",
            "9223372036854775807",
        ),
        (
            "CAST(-19999999999999999999 AS UNSIGNED)",
            "9223372036854775808",
        ),
        (
            "CAST(19999999999999999999 AS UNSIGNED)",
            "18446744073709551615",
        ),
        ("CAST(1.5 AS SIGNED)", "2"),
        ("CAST(-1.5 AS SIGNED)", "-2"),
        ("CAST(-1.5 AS UNSIGNED)", "18446744073709551614"),
    ]);
}

#[test]
fn truncate_and_div_keep_integers_exact() {
    assert_answers(&[
        ("TRUNCATE(18446744073709551615, -1)", "18446744073709551610"),
        ("TRUNCATE(9223372036854775807, -2)", "9223372036854775800"),
        ("TRUNCATE(-9223372036854775807, -2)", "-9223372036854775800"),
        ("TRUNCATE(123, 0)", "123"),
        ("TRUNCATE(123, 2)", "123"),
        ("1.23456789e19 DIV 2", "6172839450000000000"),
        ("7.5 DIV 2.5", "3"),
        ("7.9e0 DIV 2", "3"),
    ]);
}

#[test]
fn crc32_reads_the_text_mysql_would_show() {
    assert_answers(&[
        ("CRC32(CAST('{\"a\": 1}' AS JSON))", "4221669015"),
        ("CRC32(1.5e0)", "2270993338"),
        ("CRC32(1.50)", "3756579112"),
    ]);
}

#[test]
fn a_decimal_cast_clamps_to_its_declared_range() {
    assert_answers(&[
        ("CAST(TIME'838:59:59' AS DECIMAL(7,2))", "99999.99"),
        ("CAST(TIME'01:02:03' AS DECIMAL(7,2))", "10203.00"),
        ("CAST(123456.7 AS DECIMAL(7,2))", "99999.99"),
        ("CAST(clock AS DECIMAL(7,2))", "99999.99"),
        ("CAST(clock AS DECIMAL(8,2))", "111111.00"),
        ("CAST(id * 111111 AS DECIMAL(7,2))", "99999.99"),
        ("CAST(REPEAT('1', id * 6) AS DECIMAL(7,2))", "99999.99"),
        (
            "CAST(CAST(REPEAT('1', id * 6) AS TIME) AS DECIMAL(7,2))",
            "99999.99",
        ),
    ]);
}

#[test]
fn temporal_column_casts_clamp_both_signs_and_preserve_nulls() {
    assert_eq!(
        evaluate_rows("GROUP_CONCAT(CAST(clock AS DECIMAL(7,2)) ORDER BY id)", 3),
        "99999.99,-99999.99"
    );
}

#[test]
fn group_concat_orders_by_argument_positions() {
    assert_eq!(evaluate_rows("GROUP_CONCAT(4-id ORDER BY 1)", 3), "1,2,3");
    assert_eq!(
        evaluate_rows("GROUP_CONCAT(4-id ORDER BY 1 DESC)", 3),
        "3,2,1"
    );
    assert_eq!(
        evaluate_rows("GROUP_CONCAT(id,4-id ORDER BY 2)", 3),
        "31,22,13"
    );
    for invalid in ["GROUP_CONCAT(id ORDER BY 0)", "GROUP_CONCAT(id ORDER BY 2)"] {
        assert!(scalar(invalid).starts_with("error"));
    }
}

#[test]
fn group_concat_distinct_has_sorted_output_without_an_order_clause() {
    assert_eq!(evaluate_rows("GROUP_CONCAT(DISTINCT 4-id)", 3), "1,2,3");
    assert_eq!(
        evaluate_rows("GROUP_CONCAT(DISTINCT IF(id=2, 2, 10))", 3),
        "2,10"
    );
}

#[test]
fn a_count_or_position_past_the_64_bit_range_saturates() {
    assert_answers(&[
        (
            "CONCAT('[', LEFT('hello', 18446744073709551616), ']')",
            "[hello]",
        ),
        (
            "CONCAT('[', LEFT('hello', -18446744073709551616), ']')",
            "[]",
        ),
        (
            "CONCAT('[', RIGHT('hello', 18446744073709551615), ']')",
            "[hello]",
        ),
        (
            "CONCAT('[', SUBSTRING('hello', -18446744073709551616, 1), ']')",
            "[]",
        ),
        (
            "CONCAT('[', SUBSTRING('hello', 1, 18446744073709551617), ']')",
            "[hello]",
        ),
        ("LOCATE('lo', 'hello', -18446744073709551615)", "0"),
        ("LOCATE('lo', 'hello', 18446744073709551617)", "0"),
        ("INSERT('hello', 1, 18446744073709551616, 'hi')", "hi"),
        ("INSERT('hello', -18446744073709551615, 1, 'hi')", "hello"),
        ("LENGTH(LEFT(REPEAT('a', 300), 150))", "150"),
        ("LENGTH(SUBSTRING(REPEAT('a', 300), 101))", "200"),
    ]);
}

#[test]
fn a_character_set_introducer_decides_what_the_literal_bytes_mean() {
    assert_answers(&[
        ("CHAR_LENGTH(_utf8mb4 X'D0A0')", "1"),
        ("CHAR_LENGTH(_utf8mb4 0xD0A0)", "1"),
        ("IF(_binary 'a' = 'A', 1, 0)", "0"),
        ("IF('a' = 'A', 1, 0)", "1"),
        ("_latin1 'abc'", "abc"),
        ("_latin1 'café'", "cafÃ©"),
        ("_latin1 X'636166E9'", "café"),
        ("_ucs2 X'0420'", "Р"),
        ("_utf16 'ab'", "慢"),
        ("HEX(_binary 'ab')", "6162"),
    ]);
    // Reading these bytes as UTF-8 would answer different text.
    for refused in ["_ucs2 X'D800'", "_utf16 X'D800'", "_koi8u 'caf\u{e9}'"] {
        assert!(scalar(refused).starts_with("error"), "{refused}");
    }
}

#[test]
fn dates_written_as_digits_or_with_other_punctuation_are_dates() {
    assert_answers(&[
        ("DAYNAME(19700101)", "Thursday"),
        ("MONTHNAME(19700101)", "January"),
        (
            "DATE_FORMAT('19980131131415', '%Y|%H|%i|%S')",
            "1998|13|14|15",
        ),
        ("TO_DAYS('960101')", "729024"),
        ("CAST(FROM_DAYS(TO_DAYS('960101')) AS CHAR)", "1996-01-01"),
        ("CAST(CAST('2006.1.1' AS DATE) AS CHAR)", "2006-01-01"),
        ("CAST(CAST(20060101 AS DATE) AS CHAR)", "2006-01-01"),
        (
            "CAST('2005.09.01' - INTERVAL 6 MONTH AS CHAR)",
            "2005-03-01",
        ),
        ("CAST(DATE('98/02/03') AS CHAR)", "1998-02-03"),
    ]);
}

#[test]
fn a_date_shifted_past_year_9999_is_null() {
    assert_answers(&[
        (
            "DATE_ADD('1997-12-31 23:59:59', INTERVAL 100000 MONTH)",
            "NULL",
        ),
        ("DATE_ADD('9999-12-31 23:59:59', INTERVAL 1 SECOND)", "NULL"),
    ]);
}

#[test]
fn text_read_as_a_time_is_clamped_and_compact_digits_stay_a_time() {
    assert_answers(&[
        ("TIME_TO_SEC('916:40:00')", "3020399"),
        ("SUBTIME('916:40:00', '416:40:00')", "422:19:59"),
        ("EXTRACT(HOUR FROM '100000:02:03')", "838"),
        ("HOUR('230322')", "23"),
        ("ADDTIME('230322', '1')", "23:03:23"),
        ("CAST(REPEAT('1', id * 6) AS TIME)", "11:11:11"),
        (
            "CAST(CONCAT(REPEAT('1', id * 6), '.25') AS TIME(2))",
            "11:11:11.25",
        ),
        (
            "CAST(CONCAT('-', REPEAT('1', id * 6)) AS TIME)",
            "-11:11:11",
        ),
        ("CAST(CONCAT('2006010', id) AS TIME)", "838:59:59"),
        ("CAST(CONCAT('2006010111121', id) AS TIME)", "11:12:11"),
    ]);
}

#[test]
fn greatest_and_least_compare_in_one_domain() {
    assert_answers(&[
        ("GREATEST('11', 5, 2)", "5"),
        ("LEAST('11', 5, 2)", "11"),
        (
            "CAST(GREATEST(DATE '2005-05-05', 20010101, 20040404, 20030303) AS CHAR)",
            "2005-05-05",
        ),
        (
            "CAST(LEAST(DATE '2005-05-05', 20030303, 20010101, 20040404) AS CHAR)",
            "2001-01-01",
        ),
        (
            "CAST(LEAST(DATE '2005-05-05', '20030303', '20010101', '20040404') AS CHAR)",
            "2001-01-01",
        ),
        ("GREATEST(1, 2.5, 3)", "3.0"),
    ]);
}

#[test]
fn an_assignment_inside_a_query_is_refused() {
    // Only SET assigns a user variable; see docs/limitations.md.
    assert!(scalar("@n := 1").starts_with("error"));
}

#[test]
fn time_and_makedate_follow_the_type_ranges() {
    assert_answers(&[
        ("TIME('-73:42:12')", "-73:42:12"),
        ("TIME('838:59:59')", "838:59:59"),
        ("CAST(MAKEDATE(03, 1) AS CHAR)", "2003-01-01"),
        ("CAST(MAKEDATE(99, 1) AS CHAR)", "1999-01-01"),
        ("MAKEDATE(9999, 366)", "NULL"),
    ]);
}

#[test]
fn unix_conversions_use_the_session_zone_and_keep_fractional_seconds() {
    struct ResetZone;
    impl Drop for ResetZone {
        fn drop(&mut self) {
            assert!(pintail_exec::set_session_time_zone(None));
        }
    }
    let _reset = ResetZone;
    assert!(pintail_exec::set_session_time_zone(Some("+02:00")));
    assert_answers(&[
        ("FROM_UNIXTIME(0)", "1970-01-01 02:00:00"),
        ("FROM_UNIXTIME(id)", "1970-01-01 02:00:01"),
        ("FROM_UNIXTIME(1.25)", "1970-01-01 02:00:01.25"),
        ("FROM_UNIXTIME(id + 0.25)", "1970-01-01 02:00:01.25"),
        ("FROM_UNIXTIME(-0.000001)", "NULL"),
        ("FROM_UNIXTIME(-0.0000001)", "NULL"),
        ("FROM_UNIXTIME('1.25')", "1970-01-01 02:00:01.250000"),
        ("FROM_UNIXTIME(0.0000005)", "1970-01-01 02:00:00.000001"),
        (
            "FROM_UNIXTIME(32536771199.999999)",
            "3001-01-19 01:59:59.999999",
        ),
        ("FROM_UNIXTIME(32536771199.9999999)", "NULL"),
        ("UNIX_TIMESTAMP('1970-01-01 02:00:01')", "1"),
        (
            "UNIX_TIMESTAMP(CONCAT('1970-01-01 02:00:0', id))",
            "1.000000",
        ),
        ("UNIX_TIMESTAMP('1970-01-01 02:00:01.25')", "1.25"),
        ("UNIX_TIMESTAMP('invalid')", "0.000000"),
    ]);
    assert!(pintail_exec::set_session_time_zone(Some(
        "America/New_York"
    )));
    assert_answers(&[
        ("FROM_UNIXTIME(0)", "1969-12-31 19:00:00"),
        ("UNIX_TIMESTAMP('1969-12-31 19:00:01')", "1"),
    ]);
}

#[test]
fn calendar_locale_is_captured_before_execution() {
    struct ResetLocale;
    impl Drop for ResetLocale {
        fn drop(&mut self) {
            assert!(pintail_exec::set_session_calendar_locale(None));
        }
    }
    let _reset = ResetLocale;
    for (locale, expected) in [
        ("fr_FR", "lundi mars lun mar"),
        ("ru_RU", "Понедельник Марта Пнд Мар"),
        ("ja_JP", "月曜日 3月 月  3月"),
    ] {
        assert!(pintail_exec::set_session_calendar_locale(Some(locale)));
        let actual = evaluate_rows_after_plan(
            "DATE_FORMAT(DATE_ADD('2024-03-03', INTERVAL id DAY), '%W %M %a %b')",
            1,
            || {
                assert!(pintail_exec::set_session_calendar_locale(None));
            },
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn default_week_mode_is_captured_before_execution() {
    struct ResetWeek;
    impl Drop for ResetWeek {
        fn drop(&mut self) {
            pintail_exec::set_session_default_week_format(None);
        }
    }
    let _reset = ResetWeek;
    pintail_exec::set_session_default_week_format(Some(3));
    let actual =
        evaluate_rows_after_plan("WEEK(DATE_ADD('2020-12-31', INTERVAL id DAY))", 1, || {
            pintail_exec::set_session_default_week_format(None);
        });
    assert_eq!(actual, "53");
}

#[test]
fn prefixed_adjacent_strings_form_one_value() {
    assert_answers(&[
        ("_utf8mb4 'first' 'second'", "firstsecond"),
        ("CONCAT(_utf8mb4 'a' 'b', 'c')", "abc"),
    ]);
}

#[test]
fn wide_character_sets_preserve_character_and_byte_operations() {
    for (expression, expected) in [
        ("HEX(_ucs2 X'004100E9')", "004100E9"),
        ("HEX(LOWER(_ucs2 X'004100C9'))", "006100E9"),
        ("LENGTH(_utf16 X'D83DDE00')", "4"),
        ("CHAR_LENGTH(_utf16 X'D83DDE00')", "1"),
        ("HEX(CONVERT('é' USING utf16le))", "E900"),
        ("HEX(_utf32 X'01')", "00000001"),
        ("HEX(_ucs2 X'D800')", "D800"),
        ("ORD(_ucs2 X'0041')", "65"),
        ("ASCII(_ucs2 X'0041')", "0"),
        (
            "SHA1(_ucs2 X'0061')",
            "3106600e0327ca77371f2526df794ed84322585c",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn connection_encoding_is_captured_in_generated_and_aggregate_text() {
    for (expression, expected) in [
        ("HEX(CONCAT(id))", "0031"),
        ("HEX(GROUP_CONCAT(id, 7))", "00310037"),
        ("HEX(CAST('é' AS BINARY))", "00E9"),
        ("HEX(CASE WHEN id=1 THEN 'é' ELSE 'x' END)", "00E9"),
        ("HEX(CONVERT(_utf16 X'D83DDE00' USING utf8mb4))", "F09F9880"),
        ("HEX(SUBSTRING(CONCAT(id, 'é'), 2))", "00E9"),
    ] {
        pintail_sql::set_session_character_set(Some(pintail_types::CharacterSet::Ucs2));
        let answer = evaluate_rows_after_plan(expression, 1, || {
            pintail_sql::set_session_character_set(None);
        });
        assert_eq!(answer, expected, "{expression}");
    }
}

#[test]
fn encoded_constants_keep_temporal_and_binary_comparisons() {
    for (expression, expected) in [
        ("CAST('2016-12-13' AS DATE) = '20161213'", "Boolean(true)"),
        (
            "CAST('2016-12-13' AS DATE) IN ('20161213')",
            "Boolean(true)",
        ),
        ("'a' = 'a '", "Boolean(true)"),
        ("'a\\0' < 'a'", "Boolean(true)"),
        ("BINARY 'a\\0' > 'a'", "Boolean(true)"),
        ("HEX(CONVERT(0xAA USING ucs2))", "00AA"),
        ("HEX(CONVERT(0xFF USING utf8mb4))", ""),
        ("HEX(CONVERT(0xD800 USING utf16))", "NULL"),
    ] {
        pintail_sql::set_session_character_set(Some(pintail_types::CharacterSet::Ucs2));
        pintail_sql::set_session_default_collation(Some("ucs2_general_ci"));
        let answer = scalar(expression);
        pintail_sql::set_session_character_set(None);
        pintail_sql::set_session_default_collation(None);
        assert_eq!(answer, expected, "{expression}");
    }
}

#[test]
fn year_casts_and_seconds_read_temporal_shapes() {
    assert_answers(&[
        ("CAST(TIMESTAMP'2010-01-01 00:00' AS YEAR)", "2010"),
        ("CAST(TIMESTAMP'0579-10-10 10:10:10' AS YEAR)", "NULL"),
        ("CAST(CAST('{}' AS JSON) AS YEAR)", "0"),
        ("TIME_TO_SEC(CAST('2030' AS YEAR))", "1230"),
        ("TIME_TO_SEC(-2030.12)", "-1230"),
        ("TIME_TO_SEC('2001-01-02 03:04:05')", "11045"),
        ("TIME_TO_SEC('900:00:00')", "3020399"),
    ]);
}

#[test]
fn a_time_column_year_captures_the_statement_clock() {
    pintail_exec::set_session_timestamp_micros(Some(1_593_561_600_000_000));
    let answer = evaluate_rows_after_plan("CAST(clock AS YEAR)", 1, || {
        pintail_exec::set_session_timestamp_micros(None);
    });
    assert_eq!(answer, "2020");
}

#[test]
fn float_casts_narrow_values_and_preserve_decimal_guard_digits() {
    assert_answers(&[
        ("CAST(CAST(16777217 AS FLOAT) AS SIGNED)", "16777216"),
        (
            "CAST(CAST(1.23456789 AS FLOAT) AS DOUBLE)",
            "float 1.2345678806304932",
        ),
        ("CAST(1/3 AS DOUBLE)", "float 0.333333333"),
        ("MAKETIME(1, 2, CAST('1.6' AS FLOAT))", "01:02:01.600000"),
        (
            "TIMEDIFF(CAST('101112' AS DOUBLE), TIME'101010')",
            "00:01:02.000000",
        ),
        ("CONCAT(CAST(1.23456789 AS FLOAT))", "1.23457"),
        ("CONCAT(CAST(20000101235959 AS FLOAT))", "2.00001e13"),
        ("CAST(CAST(1.23456789 AS FLOAT) AS CHAR)", "1.23457"),
        (
            "CAST(CAST(1.23456789 AS FLOAT) AS DECIMAL(12,9))",
            "1.234567881",
        ),
    ]);
}

#[test]
fn string_search_boundaries_and_conversion_overflow() {
    for (expression, expected) in [
        ("LOCATE('', '')", "1"),
        ("LOCATE('', 'abc', 4)", "4"),
        ("LOCATE('', 'abc', 5)", "0"),
        ("CHAR(92) LIKE CHAR(92)", "Boolean(true)"),
        (
            "CONCAT('a', CHAR(92)) LIKE CONCAT('%', CHAR(92))",
            "Boolean(true)",
        ),
        ("CONV('9223372036854775808', -10, 16)", "7FFFFFFFFFFFFFFF"),
        ("CONV('-9223372036854775809', -10, 16)", "8000000000000000"),
        ("CONV('-18446744073709551615', 10, 16)", "1"),
        ("CONV('-18446744073709551616', 10, 16)", "0"),
        ("CONV('29223372036854775809', -10, 16)", "7FFFFFFFFFFFFFFF"),
        ("CONV('-29223372036854775809', -10, 16)", "8000000000000000"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn string_search_and_padding_use_subject_semantics() {
    for (expression, expected) in [
        ("LOCATE('HE', 'hello' COLLATE utf8mb4_bin)", "0"),
        ("LOCATE('HE' COLLATE utf8mb4_bin, 'hello')", "1"),
        ("INSTR('hello', BINARY 'HE')", "1"),
        ("INSTR(BINARY 'hello', 'HE')", "0"),
        ("LOCATE(X'44', _utf8mb4'abcdef')", "4"),
        ("INSTR(_utf8mb4'abcdef', X'44')", "4"),
        ("HEX(RPAD('я', 3, X'20'))", "D18F2020"),
        ("HEX(LPAD('я', 3, X'20'))", "2020D18F"),
        ("HEX(RPAD(BINARY 'я', 3, ' '))", "D18F20"),
        ("FIELD('b', 'A' COLLATE utf8mb4_bin, 'B')", "2"),
        ("FIELD('b' COLLATE utf8mb4_bin, 'A', 'B')", "0"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn string_search_preserves_accents_and_source_character_positions() {
    for (expression, expected) in [
        ("LOCATE('e', 'café')", "0"),
        ("LOCATE('ss', 'straße')", "5"),
        ("LOCATE('s', 'ß')", "0"),
        ("LOCATE('s', 'ßs')", "2"),
        ("LOCATE('e', 'straße')", "6"),
        ("LOCATE('é', 'e\u{301}')", "1"),
        ("LOCATE('e', 'e\u{301}')", "1"),
        ("LOCATE('x', 'e\u{301}x')", "3"),
        ("LOCATE('ss', 'straße' COLLATE utf8mb4_general_ci)", "0"),
        ("LOCATE('é', 'e\u{301}' COLLATE utf8mb4_general_ci)", "0"),
        ("INSTR('café', 'E')", "0"),
        ("INSTR('straße', 'E')", "6"),
        ("LOCATE('Σ', 'ς')", "1"),
        ("LOCATE('ffi', 'ﬃ')", "1"),
        ("LOCATE('f', 'ﬃ')", "0"),
        ("LOCATE('ss', 'ßss', 2)", "2"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn conditional_binary_branches_keep_bytes_and_comparison_domain() {
    for (expression, expected) in [
        ("IF(id = 1, BINARY 'A', 'a') = 'a'", "Boolean(false)"),
        ("IF(id = 1, 'A', BINARY 'a') = 'a'", "Boolean(false)"),
        (
            "CASE WHEN id = 1 THEN BINARY 'A' ELSE 'a' END = 'a'",
            "Boolean(false)",
        ),
        (
            "COALESCE(IF(id = 1, NULL, 'a'), BINARY 'A') = 'a'",
            "Boolean(false)",
        ),
        ("HEX(IF(id = 1, X'FF', 'a'))", "FF"),
        ("HEX(IF(id = 1, _ucs2 X'044F', BINARY 'a'))", "044F"),
        ("HEX(COALESCE(_ucs2 X'044F', BINARY 'a'))", "044F"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn dynamic_decimal_rounding_retains_declared_scale() {
    for (expression, expected) in [
        ("ROUND(1.2345, id)", "1.2000"),
        ("ROUND(1.2345, id - 1)", "1.0000"),
        ("ROUND(15.2345, -CAST(id AS SIGNED))", "20.0000"),
        ("TRUNCATE(1.2345, id)", "1.2000"),
        ("TRUNCATE(15.2345, -CAST(id AS SIGNED))", "10.0000"),
        ("ROUND(1.2345, 1)", "1.2"),
        ("TRUNCATE(1.2345, 1)", "1.2"),
        (
            "ROUND(LEAST(15, -4939092, 0.2704), STDDEV('a'))",
            "-4939092.0000",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn date_format_parsing_handles_partial_values_and_mysql_directives() {
    for (expression, expected) in [
        (
            "STR_TO_DATE('17-03-2008 2:59:58.999', '%d-%m-%Y %H:%i:%s.%f')",
            "2008-03-17 02:59:58.999000",
        ),
        ("STR_TO_DATE('10:20:10', '%h:%i:%s.%f')", "10:20:10.000000"),
        ("STR_TO_DATE('10:00 PM', '%h:%i %p')", "22:00:00"),
        (
            "STR_TO_DATE('17-03-2008', '%d-%m-%Y %H:%i:%S')",
            "2008-03-17 00:00:00",
        ),
        ("STR_TO_DATE('02', '%d')", "0000-00-02"),
        ("STR_TO_DATE('08-03-17', '%Y-%m-%d')", "2008-03-17"),
        ("STR_TO_DATE('0008-03-17', '%Y-%m-%d')", "0008-03-17"),
        ("STR_TO_DATE('17 SEPTEMB 2008', '%d %M %Y')", "2008-09-17"),
        ("STR_TO_DATE('17th May 2008', '%D %b %Y')", "2008-05-17"),
        (
            "STR_TO_DATE('2008-....03ABCD-17 2:11:12.0012', '%Y-%.%m%@-%d %H:%i:%S.%f')",
            "2008-03-17 02:11:12.001200",
        ),
        ("STR_TO_DATE('060 2008', '%j %Y')", "2008-02-29"),
        ("STR_TO_DATE('0000-00-00', '%Y-%m-%d')", "0000-00-00"),
        ("STR_TO_DATE(SPACE(2), '1')", "0000-00-00"),
        (
            "STR_TO_DATE('2008-03-17 10:11:12 PM', '%Y-%m-%d %H:%i:%S %p')",
            "NULL",
        ),
        ("STR_TO_DATE('2023-02-31', '%Y-%m-%d')", "NULL"),
        (
            "STR_TO_DATE('2008-03-17', IF(id = 1, '%Y-%m-%d', '%d'))",
            "2008-03-17 00:00:00.000000",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn date_format_parsing_captures_zero_date_policy() {
    for (mode, expression, expected) in [
        ("NO_ZERO_DATE", "STR_TO_DATE('02', '%d')", "NULL"),
        (
            "NO_ZERO_DATE",
            "STR_TO_DATE('0000-01-02', '%Y-%m-%d')",
            "NULL",
        ),
        (
            "NO_ZERO_IN_DATE",
            "STR_TO_DATE('2020-00-02', '%Y-%m-%d')",
            "NULL",
        ),
        (
            "NO_ZERO_IN_DATE",
            "STR_TO_DATE('0000-01-02', '%Y-%m-%d')",
            "0000-01-02",
        ),
        (
            "NO_ZERO_DATE",
            "STR_TO_DATE('10:20:10', '%H:%i:%s')",
            "10:20:10",
        ),
        (
            "",
            "STR_TO_DATE('Tuesday 00 2002', '%W %U %Y')",
            "2002-01-01",
        ),
        (
            "",
            "STR_TO_DATE('Thursday 53 1998', '%W %u %Y')",
            "1998-12-31",
        ),
        (
            "",
            "STR_TO_DATE('Sunday 01 2001', '%W %v %x')",
            "2001-01-07",
        ),
        (
            "",
            "STR_TO_DATE('Tuesday 52 2001', '%W %V %X')",
            "2002-01-01",
        ),
        ("", "STR_TO_DATE('Tuesday 52 2001', '%W %V %x')", "NULL"),
    ] {
        pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(mode), || {
            assert_eq!(scalar(expression), expected, "{mode}: {expression}");
        });
    }
}

#[test]
fn parsed_partial_dates_survive_temporal_consumers() {
    let date = "STR_TO_DATE('10:20:10', IF(id = 1, '%H:%i:%s', '%d'))";
    for (expression, expected) in [
        (format!("CAST({date} AS DATETIME)"), "0000-00-00 10:20:10"),
        (format!("DATE({date})"), "0000-00-00"),
        (format!("TIME({date})"), "10:20:10.000000"),
        (
            "TIME(STR_TO_DATE('2008-03-17 10:20:10', '%Y-%m-%d %H:%i:%s.%f'))".to_owned(),
            "10:20:10.000000",
        ),
        ("MONTHNAME(STR_TO_DATE(1, '%m'))".to_owned(), "January"),
        ("LAST_DAY('2008-02-00')".to_owned(), "2008-02-29"),
        ("FROM_DAYS(1)".to_owned(), "0000-00-00"),
        (
            "STR_TO_DATE('02 10:11:12', '%d %H:%i:%S.%f')".to_owned(),
            "58:11:12.000000",
        ),
        (
            "DATE_FORMAT('0000-01-01', '%W %d %M %Y')".to_owned(),
            "Sunday 01 January 0000",
        ),
        (
            "DATE_FORMAT('0000-02-28', '%W %d %M %Y')".to_owned(),
            "Tuesday 28 February 0000",
        ),
    ] {
        assert_eq!(scalar(&expression), expected, "{expression}");
    }
}

#[test]
fn weekday_names_keep_their_implicit_numeric_value() {
    for (expression, expected) in [
        ("DAYNAME('2008-03-18') + 0", "float 1"),
        ("DAYNAME('2008-03-18') = 1", "Boolean(true)"),
        ("DAYNAME('2008-03-18') = 'Tuesday'", "Boolean(true)"),
        ("CAST(DAYNAME('2008-03-18') AS SIGNED)", "0"),
        ("CONCAT(DAYNAME('2008-03-18')) + 0", "float 0"),
        ("IF(id = 1, DAYNAME('2008-03-18'), '') + 0", "float 1"),
        ("COALESCE(DAYNAME('2008-03-18'), '') + 0", "float 0"),
        ("SUM(DAYNAME('2008-03-18'))", "float 1"),
        ("SUM(DAYNAME('2008-03-18')) OVER ()", "float 1"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn time_intervals_keep_signed_duration_and_precision() {
    for (expression, expected) in [
        ("DATE_ADD(TIME '23:59:59', INTERVAL 2 SECOND)", "24:00:01"),
        ("DATE_SUB(TIME '00:00:01', INTERVAL 2 SECOND)", "-00:00:01"),
        ("DATE_ADD(TIME '838:59:59', INTERVAL 1 SECOND)", "NULL"),
        (
            "DATE_ADD(TIME '10:20:30.125', INTERVAL 1 MINUTE)",
            "10:21:30.125",
        ),
        ("DATE_ADD('10:20:30', INTERVAL 1 MINUTE)", "NULL"),
        (
            "STR_TO_DATE('10:20:30', '%H:%i:%s') + INTERVAL 10 MINUTE",
            "10:30:30",
        ),
        ("clock + INTERVAL 1 SECOND", "11:11:12"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn extract_time_fields_preserve_duration_sign_and_day_prefix() {
    for (expression, expected) in [
        ("EXTRACT(DAY_MINUTE FROM '02 10:11:12')", "5811"),
        ("EXTRACT(DAY_SECOND FROM '0000-00-00')", "0"),
        ("EXTRACT(DAY_SECOND FROM '2020-00-00 10:11:12')", "101112"),
        ("EXTRACT(DAY_SECOND FROM 20200317)", "NULL"),
        ("EXTRACT(DAY_SECOND FROM '20200317')", "8385959"),
        ("EXTRACT(DAY_SECOND FROM 123456)", "123456"),
        ("EXTRACT(DAY_SECOND FROM '20200317101112')", "17101112"),
        ("EXTRACT(DAY_SECOND FROM '225 10:11:12')", "8385959"),
        ("EXTRACT(DAY_SECOND FROM '-02 10:11:12')", "-581112"),
        ("EXTRACT(HOUR_MINUTE FROM '-02 10:11:12')", "-5811"),
        ("EXTRACT(MINUTE_SECOND FROM '-02 10:11:12')", "-1112"),
        ("EXTRACT(HOUR FROM '-02 10:11:12')", "-58"),
        ("EXTRACT(DAY_SECOND FROM '2020-03-17 10:11:12')", "17101112"),
        ("EXTRACT(DAY_HOUR FROM '2020-03-17 10:11:12')", "1710"),
        ("EXTRACT(YEAR_MONTH FROM '2020-03-17 10:11:12')", "202003"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn time_calendar_conversions_capture_the_statement_date() {
    for (expression, expected) in [
        ("CAST(clock AS DATETIME)", "2020-07-01 11:11:11"),
        ("DATE(TIME '-01:00:00')", "2020-06-30"),
        ("CAST(TIME '-01:00:00' AS DATETIME)", "2020-06-30 23:00:00"),
        ("CAST(TIME '25:00:00' AS DATE)", "2020-07-02"),
        ("DATE_ADD(clock, INTERVAL 1 DAY)", "2020-07-02 11:11:11"),
        ("DATE_ADD(clock, INTERVAL 1 MONTH)", "2020-08-01 11:11:11"),
    ] {
        pintail_exec::set_session_timestamp_micros(Some(1_593_561_600_000_000));
        let answer = evaluate_rows_after_plan(expression, 1, || {
            pintail_exec::set_session_timestamp_micros(None);
        });
        assert_eq!(answer, expected, "{expression}");
    }
}

#[test]
fn integer_division_reads_internal_decimal_digits() {
    for (expression, expected) in [
        ("27 DIV (1 / 97)", "2619"),
        ("27 DIV (id / 97)", "2619"),
        ("27 DIV (id / 97) / 17", "154.0588"),
        ("27 DIV CAST(id / 97 AS CHAR)", "2621"),
        ("27 DIV CAST(id / 97 AS DOUBLE)", "2619"),
        ("-27 DIV (id / 97)", "-2619"),
        ("1 DIV (1 / 3)", "3"),
        ("3 DIV (1 / 3)", "9"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn accent_and_case_sensitive_collation_keeps_comparison_and_search_domains() {
    for (expression, expected) in [
        ("('a' COLLATE utf8mb4_0900_as_cs) = 'A'", "Boolean(false)"),
        ("('a' COLLATE utf8mb4_0900_as_cs) = 'á'", "Boolean(false)"),
        ("('a' COLLATE utf8mb4_0900_as_cs) < 'A'", "Boolean(true)"),
        ("('é' COLLATE utf8mb4_0900_as_cs) < 'f'", "Boolean(true)"),
        (
            "('é' COLLATE utf8mb4_0900_as_cs) = 'e\u{301}'",
            "Boolean(true)",
        ),
        ("('a' COLLATE utf8mb4_0900_as_cs) = 'a '", "Boolean(false)"),
        ("LOCATE('a', 'A' COLLATE utf8mb4_0900_as_cs)", "0"),
        ("LOCATE('é', 'e\u{301}' COLLATE utf8mb4_0900_as_cs)", "0"),
        (
            "('a' COLLATE utf8mb4_0900_as_cs) LIKE 'A'",
            "Boolean(false)",
        ),
        (
            "REGEXP_LIKE('a' COLLATE utf8mb4_0900_as_cs, 'A')",
            "Boolean(false)",
        ),
        (
            "REGEXP_LIKE('a' COLLATE utf8mb4_0900_as_cs, 'A', 'i')",
            "Boolean(true)",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
    assert_eq!(
        evaluate_rows(
            "COUNT(DISTINCT (CASE WHEN id=1 THEN 'a' WHEN id=2 THEN 'A' ELSE 'á' END) COLLATE utf8mb4_0900_as_cs)",
            3
        ),
        "3"
    );
}

#[test]
fn calendar_casts_capture_zero_and_invalid_date_policies() {
    for (mode, text, expected) in [
        ("", "0000-00-00", "0000-00-00"),
        ("NO_ZERO_DATE", "0000-00-00", "NULL"),
        ("NO_ZERO_DATE", "0000-01-01", "0000-01-01"),
        ("NO_ZERO_DATE", "2000-01-00", "2000-01-00"),
        ("NO_ZERO_IN_DATE", "0000-00-00", "0000-00-00"),
        ("NO_ZERO_IN_DATE", "2000-01-00", "NULL"),
        ("", "2000-00-01", "2000-00-01"),
        ("", "2000-02-31", "NULL"),
        ("ALLOW_INVALID_DATES", "2000-02-31", "2000-02-31"),
        ("ALLOW_INVALID_DATES", "2000-02-32", "NULL"),
        ("ALLOW_INVALID_DATES,NO_ZERO_IN_DATE", "2000-00-01", "NULL"),
    ] {
        pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(mode), || {
            for input in [format!("'{text}'"), format!("IF(id=1, '{text}', NULL)")] {
                for target in ["DATE", "DATETIME(6)"] {
                    let sql = format!("CAST({input} AS {target})");
                    let answer = evaluate(&sql);
                    let expected = if expected != "NULL" && target == "DATETIME(6)" {
                        format!("{expected} 00:00:00.000000")
                    } else {
                        expected.to_owned()
                    };
                    assert_eq!(answer, expected, "{mode}: {sql}");
                }
            }
        });
    }
}

#[test]
fn accepted_invalid_calendar_values_keep_their_fields() {
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("ALLOW_INVALID_DATES"),
        || {
            for (function, expected) in [
                ("YEAR", "2000"),
                ("MONTH", "2"),
                ("DAY", "31"),
                ("DATE", "2000-02-31"),
                ("MONTHNAME", "February"),
                ("LAST_DAY", "2000-02-29"),
            ] {
                assert_eq!(
                    evaluate(&format!(
                        "{function}(CAST(IF(id=1,'2000-02-31',NULL) AS DATE))"
                    )),
                    expected,
                    "{function}"
                );
            }
        },
    );
}

#[test]
fn chained_between_binds_its_upper_bound_before_the_outer_comparison() {
    for (expression, expected) in [
        ("5 BETWEEN 0 AND 10 BETWEEN 0 AND 1", "Boolean(false)"),
        ("(5 BETWEEN 0 AND 10) BETWEEN 0 AND 1", "Boolean(true)"),
        ("5 NOT BETWEEN 0 AND 10 BETWEEN 0 AND 1", "Boolean(true)"),
        ("5 BETWEEN 0 AND 10 NOT BETWEEN 0 AND 1", "Boolean(false)"),
        (
            "id BETWEEN 0 AND 2 BETWEEN 0 AND 1 BETWEEN 0 AND 1",
            "Boolean(false)",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn utf8_conversion_keeps_the_valid_prefix_before_invalid_bytes() {
    for (expression, expected) in [
        ("HEX(CONVERT(0xFF USING utf8mb4))", ""),
        ("HEX(CONVERT(0x41FF42 USING utf8mb4))", "41"),
        ("HEX(CONVERT(0x41E1 USING utf8mb4))", "41"),
        ("HEX(CONVERT(0x41F09D8C8642 USING utf8mb3))", "NULL"),
        ("HEX(CONVERT(0x41F09D8C8642 USING utf8mb4))", "41F09D8C8642"),
        ("HEX(CONVERT(0xFF USING utf8mb3))", ""),
        ("HEX(CONVERT(IF(id=1,0x41FF42,0x42) USING utf8mb4))", "41"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn base_digits_keep_raw_bytes_under_a_wide_connection_encoding() {
    pintail_sql::set_session_character_set(Some(pintail_types::CharacterSet::Utf16Le));
    for (expression, expected) in [
        ("HEX(CONV(CONVERT('123' USING utf16le),10,16))", "3742"),
        ("HEX(CONV(CONVERT('123' USING utf16le),-10,16))", "3742"),
        ("CONV(123,10,16)", "䈷"),
        ("CONV(1,10,16)", ""),
        ("HEX(BIN(3))", "3131"),
        ("HEX(OCT(9))", "3131"),
        ("HEX(LOWER(CONV(1,10,16)))", "31"),
        ("HEX(UPPER(CONV(1,10,16)))", "31"),
        ("LENGTH(CONV(1,10,16))", "1"),
        ("CHAR_LENGTH(CONV(1,10,16))", "0"),
        ("HEX(CONCAT(CONV(1,10,16)))", "31"),
        ("HEX(CONCAT(CONV(1,10,16),'a'))", "316100"),
        ("HEX(CONCAT_WS('',CONV(1,10,16),'a'))", "316100"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
    pintail_sql::set_session_character_set(None);
}

#[test]
fn fixed_float_precision_survives_string_consumers() {
    for (expression, expected) in [
        ("CRC32(PI())", "2969982827"),
        ("CONCAT(PI())", "3.141593"),
        ("CONCAT(PI()+0)", "3.141593"),
        ("CONCAT(PI()+0.0000001)", "3.1415928"),
        ("CONCAT(PI()*PI())", "9.869604"),
        ("CONCAT(PI()/2)", "1.5707963268"),
        ("CONCAT(ROUND(PI(),2))", "3.14"),
        ("CONCAT(COALESCE(PI(),0))", "3.141593"),
        ("CONCAT(IF(id=1,PI(),0))", "3.141593"),
        ("CONCAT(PI()+0e0)", "3.141592653589793"),
        ("CONCAT(CAST(PI() AS DOUBLE))", "3.141592653589793"),
        ("PI()>3.1415926 AND PI()<3.1415927", "Boolean(false)"),
        ("PI()>3.1415925 AND PI()<3.1415928", "Boolean(true)"),
        ("PI()+0e0>3.1415926 AND PI()+0e0<3.1415927", "Boolean(true)"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn strict_utf8_conversion_rejects_invalid_bytes_instead_of_truncating() {
    for mode in ["STRICT_TRANS_TABLES", "STRICT_ALL_TABLES", "TRADITIONAL"] {
        pintail_sql::with_parse_mode(pintail_sql::ParseMode::from_sql_mode(mode), || {
            for expression in [
                "HEX(CONVERT(0xFF USING utf8mb4))",
                "HEX(CONVERT(0x41FF42 USING utf8mb3))",
            ] {
                assert_eq!(scalar(expression), "NULL", "{mode}: {expression}");
            }
            assert_eq!(scalar("HEX(CONVERT(0x41 USING utf8mb4))"), "41");
        });
    }
}

#[test]
fn temporal_extrema_choose_the_comparison_domain_of_the_consumer() {
    for (expression, expected) in [
        ("LEAST(time '00:00:00',120000)", "00:00:00"),
        ("GREATEST(time '20:00:00',120000)", "20:00:00"),
        ("LEAST(time '20:00:00',120000)", "120000"),
        ("GREATEST(time '-20:00:00',-120000)", "-20:00:00"),
        ("GREATEST(time '100:00:00',200000)", "200000"),
        ("GREATEST(time '100:00:00',time '20:00:00')", "100:00:00"),
        ("GREATEST(time '-20:00:00',time '-12:00:00')", "-12:00:00"),
        ("GREATEST(time '20:00:00',120000)+0", "float 200000"),
        ("GREATEST(time '20:00:00',120000)+0.00", "float 200000"),
        ("LEAST(time '-20:00:00',-120000)+0", "float -200000"),
        ("GREATEST(time '100:00:00',200000)+0", "float 1000000"),
        (
            "GREATEST(date '1995-05-05',19910101,20050505,19930303)+0.00",
            "float 20050505",
        ),
        (
            "GREATEST('95-05-05',date '10-10-10')+0.00",
            "float 20101010",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn ordered_group_concat_places_later_equal_keys_first() {
    for (expression, expected) in [
        ("GROUP_CONCAT(id ORDER BY (id=3))", "4,2,1,3"),
        ("GROUP_CONCAT(id ORDER BY (id=3) DESC)", "3,4,2,1"),
        ("GROUP_CONCAT(DISTINCT id ORDER BY (id=3))", "4,2,1,3"),
        ("GROUP_CONCAT(id ORDER BY (id=3),id)", "1,2,4,3"),
        ("GROUP_CONCAT(id)", "1,2,3,4"),
    ] {
        assert_eq!(evaluate_rows(expression, 4), expected, "{expression}");
    }
}

#[test]
fn find_in_set_uses_bytes_when_either_operand_is_binary() {
    for (expression, expected) in [
        ("FIND_IN_SET(BINARY 'a',BINARY 'A,B,C')", "0"),
        ("FIND_IN_SET('a',BINARY 'A,B,C')", "0"),
        ("FIND_IN_SET(BINARY 'a','A,B,C')", "0"),
        ("FIND_IN_SET('a','A,B,C')", "1"),
        ("FIND_IN_SET(_binary X'FF',_binary X'FE2CFF')", "2"),
        ("FIND_IN_SET(_binary X'C3A9','a,é')", "2"),
        ("FIND_IN_SET('é',_binary X'612CC3A9')", "2"),
        ("FIND_IN_SET(_binary '',_binary ',a')", "1"),
        ("FIND_IN_SET(_binary '',_binary '')", "0"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn time_arithmetic_preserves_typed_precision_and_renders_dynamic_fractions() {
    for (expression, expected) in [
        ("SUBTIME('02:01:01.999999','01:01:01.999999')", "01:00:00"),
        ("ADDTIME('-01:01:01.01','-23:59:59.1')", "-25:01:00.110000"),
        ("ADDTIME('01:00:00.1','00:00:00.1')", "01:00:00.200000"),
        ("ADDTIME(time '01:00:00.1','00:00:00.1')", "01:00:00.2"),
        (
            "SUBTIME(time '02:01:01.999999',time '01:01:01.999999')",
            "01:00:00.000000",
        ),
        ("TIMEDIFF('01:00:00.1','00:00:00.1')", "01:00:00.0"),
        (
            "TIMEDIFF('2000:01:01 00:00:00','2000:01:01 00:00:00.000001')",
            "-00:00:00.000001",
        ),
        (
            "TIMEDIFF('20:01:01 00:00:00','20:01:01 00:00:01')",
            "-00:00:01",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn numeric_consumers_reselect_temporal_extrema_without_changing_explicit_casts() {
    for (expression, expected) in [
        ("ABS(GREATEST(time '20:00:00',120000))", "float 200000"),
        ("GREATEST(time '20:00:00',120000)=200000", "Boolean(true)"),
        ("200000=GREATEST(time '20:00:00',120000)", "Boolean(true)"),
        (
            "GREATEST(time '20:00:00',120000)='20:00:00'",
            "Boolean(true)",
        ),
        ("CAST(GREATEST(time '20:00:00',120000) AS SIGNED)", "20"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn datediff_accepts_invalid_calendar_days_only_under_the_captured_mode() {
    let expression = "DATEDIFF('1997-11-31 23:59:59.000001','1997-12-31')";
    assert_eq!(scalar(expression), "NULL");
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("ALLOW_INVALID_DATES"),
        || {
            assert_eq!(scalar(expression), "-30");
            assert_eq!(scalar("DATEDIFF('2000-02-31','2000-03-02')"), "0");
            assert_eq!(scalar("DATEDIFF('2000-02-32','2000-03-02')"), "NULL");
            assert_eq!(scalar("DATEDIFF('2000-00-01','2000-01-01')"), "NULL");
        },
    );
}

#[test]
fn concat_respects_explicit_encoding_before_binary_coercibility() {
    for (expression, expected) in [
        (
            "HEX(CONCAT(_utf32 0x0410 COLLATE utf32_general_ci,0x61))",
            "0000041000000061",
        ),
        (
            "HEX(CONCAT(_utf32 0x0410 COLLATE utf32_general_ci,0x6162))",
            "0000041000006162",
        ),
        (
            "HEX(CONCAT(0x61,_utf32 0x0410 COLLATE utf32_general_ci))",
            "0000006100000410",
        ),
        ("HEX(CONCAT(_utf32 0x0410,0x61))", "0000041061"),
        ("HEX(CONCAT(_utf16 0x0410,0x61))", "041061"),
        ("HEX(CONCAT(_binary X'FF','a'))", "FF61"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn floating_consumers_read_decimal_guard_digits_before_display_rounding() {
    for (expression, expected) in [
        ("CAST(1/3 AS DOUBLE)", "float 0.333333333"),
        ("1/3+0e0", "float 0.333333333"),
        ("CAST(1/3 AS CHAR)", "0.3333"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
    assert_eq!(evaluate_rows("STD(id/3)", 3), "float 0.27216552711199143");
    assert_eq!(
        evaluate_rows("VARIANCE(id/3)", 3),
        "float 0.07407407414814815"
    );
}

#[test]
fn time_columns_round_untyped_constant_bounds_to_their_declared_precision() {
    for (expression, expected) in [
        ("clock='11:11:11.001'", "Boolean(true)"),
        ("clock=111111.001", "Boolean(true)"),
        ("'11:11:11.001'=clock", "Boolean(true)"),
        ("clock='11:11:11.9'", "Boolean(false)"),
        ("clock<'11:11:11.001'", "Boolean(false)"),
        ("clock<='11:11:11.001'", "Boolean(true)"),
        ("clock IN('11:11:11.001')", "Boolean(true)"),
        (
            "clock BETWEEN '11:11:11.001' AND '11:11:11.9'",
            "Boolean(true)",
        ),
        ("clock=TIME'11:11:11.001'", "Boolean(false)"),
        ("TIME'11:11:11'='11:11:11.001'", "Boolean(false)"),
        ("clock=CAST('11:11:11.001' AS CHAR)", "Boolean(true)"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn nullif_year_returns_text_to_numeric_aggregates() {
    for (expression, expected) in [
        ("AVG(NULLIF(YEAR('2001-01-01'),10))", "float 2001"),
        ("SUM(NULLIF(YEAR('2001-01-01'),10))", "float 2001"),
        ("CONCAT(AVG(NULLIF(2001,10)))", "2001.0000"),
        ("CONCAT(AVG(YEAR('2001-01-01')))", "2001.0000"),
        ("NULLIF(YEAR('2001-01-01'),2001)", "NULL"),
        ("NULLIF(YEAR('2001-01-01'),10)", "2001"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn time_expressions_compare_with_strings_as_text() {
    for (expression, expected) in [
        ("CAST('1:2:3' AS TIME)='1:02:03'", "Boolean(false)"),
        ("CAST('1:2:3' AS TIME)='01:02:03'", "Boolean(true)"),
        ("CAST('1:2:3' AS TIME)='01:02:03.000000'", "Boolean(false)"),
        ("CAST('1:2:3' AS TIME)<> 'x'", "Boolean(true)"),
        ("CAST('1:2:3' AS TIME)=10203", "Boolean(true)"),
        ("CAST('1:2:3' AS TIME) IN ('1:02:03')", "Boolean(false)"),
        ("CAST('1:2:3' AS TIME) IN (TIME'1:02:03')", "Boolean(true)"),
        (
            "CAST('1:2:3' AS TIME) BETWEEN '1:00:00' AND '2:00:00'",
            "Boolean(false)",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn inet_aton_short_forms_still_require_one_byte_per_component() {
    for (expression, expected) in [
        ("INET_ATON('122.256')", "NULL"),
        ("INET_ATON('256')", "NULL"),
        ("INET_ATON('255')", "255"),
        ("INET_ATON('1.2.3')", "16908291"),
        ("INET_ATON('1.65535')", "NULL"),
        ("INET_ATON('1.+2')", "NULL"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn convert_tz_leaves_dates_outside_the_timestamp_range_unchanged() {
    for (date, expected) in [
        ("1969-12-31 23:59:59", "1969-12-31 23:59:59"),
        ("1970-01-01 00:00:00", "1970-01-01 00:00:00"),
        ("3001-01-18 23:59:59", "3001-01-19 00:59:59"),
        ("3001-01-19 00:00:00", "3001-01-19 00:00:00"),
    ] {
        assert_eq!(
            scalar(&format!("CONVERT_TZ('{date}','+00:00','+01:00')")),
            expected
        );
    }
}

#[test]
fn from_unixtime_captures_fraction_truncation_mode() {
    struct ResetZone;
    impl Drop for ResetZone {
        fn drop(&mut self) {
            assert!(pintail_exec::set_session_time_zone(None));
        }
    }
    let _reset = ResetZone;
    assert!(pintail_exec::set_session_time_zone(Some("+00:00")));
    assert_eq!(
        scalar("FROM_UNIXTIME(2147483647.9999999)"),
        "2038-01-19 03:14:08.000000"
    );
    assert_eq!(scalar("FROM_UNIXTIME(32536771199.9999999)"), "NULL");
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("TIME_TRUNCATE_FRACTIONAL"),
        || {
            for (expression, expected) in [
                (
                    "FROM_UNIXTIME(2147483647.9999999)",
                    "2038-01-19 03:14:07.999999",
                ),
                (
                    "FROM_UNIXTIME(32536771199.9999999)",
                    "3001-01-18 23:59:59.999999",
                ),
                ("FROM_UNIXTIME(1.9999999e0)", "1970-01-01 00:00:01.999999"),
                ("FROM_UNIXTIME('1.9999999')", "1970-01-01 00:00:01.999999"),
            ] {
                assert_eq!(scalar(expression), expected, "{expression}");
            }
        },
    );
}

#[test]
fn mysql_xor_and_high_not_precedence_bind_before_arithmetic_and_comparison() {
    for (expression, expected) in [
        ("1+2^3", "2"),
        ("-1^1", "18446744073709551614"),
        ("~1^1", "18446744073709551615"),
        ("NOT 2=1", "Boolean(true)"),
        ("NOT 2+3", "Boolean(false)"),
        ("NOT NULL IS NULL", "Boolean(false)"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("HIGH_NOT_PRECEDENCE"),
        || {
            for (expression, expected) in [
                ("NOT 2=1", "Boolean(false)"),
                ("NOT 2+3", "3"),
                ("NOT NULL IS NULL", "Boolean(true)"),
                ("NOT(2 BETWEEN 2 AND 3)", "Boolean(false)"),
                ("NOT 0 AND 0", "Boolean(false)"),
            ] {
                assert_eq!(scalar(expression), expected, "{expression}");
            }
        },
    );
}

#[test]
fn partial_calendar_casts_accept_minute_precision_clocks() {
    pintail_sql::with_parse_mode(pintail_sql::ParseMode::default(), || {
        for (expression, expected) in [
            ("CAST(TIMESTAMP'0000-00-00 00:00' AS YEAR)", "0"),
            (
                "CAST('0000-00-00 00:00' AS DATETIME)",
                "0000-00-00 00:00:00",
            ),
            (
                "CAST('2001-01-01 00:00' AS DATETIME)",
                "2001-01-01 00:00:00",
            ),
        ] {
            assert_eq!(scalar(expression), expected, "{expression}");
        }
    });
}

#[test]
fn executable_comments_follow_mysql_version_prefix_rules() {
    for (expression, expected) in [
        ("1 /*!999999 +1 */", "1"),
        ("1 + /*!800001+0 */ + 0", "2"),
        ("1 /*!800001 +1 */", "1"),
        ("1 /*!080400 +1 */", "2"),
        ("1 /*!80400 +1 */", "2"),
        ("1 /*!80499 +1 */", "1"),
        ("1 /*! +1 */", "2"),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}

#[test]
fn like_default_escape_follows_the_statement_sql_mode() {
    pintail_sql::with_parse_mode(
        pintail_sql::ParseMode::from_sql_mode("NO_BACKSLASH_ESCAPES"),
        || {
            assert_answers(&[
                (r"_utf8mb4'a\bc' LIKE _utf8mb4'a\%'", "Boolean(true)"),
                (r"'a%b' LIKE 'a!%b' ESCAPE '!'", "Boolean(true)"),
                (r"'abc' LIKE 'a\bc'", "Boolean(false)"),
            ]);
        },
    );
}

#[test]
fn hex_reads_decimal_operands_as_rounded_signed_integers() {
    assert_answers(&[
        ("HEX(29223372036854775809)", "7FFFFFFFFFFFFFFF"),
        ("HEX(-29223372036854775809)", "8000000000000000"),
        ("HEX(10.5)", "B"),
        ("HEX(-10.5)", "FFFFFFFFFFFFFFF5"),
        ("HEX(18446744073709551615.0)", "7FFFFFFFFFFFFFFF"),
        ("HEX(9223372036854775808.0)", "7FFFFFFFFFFFFFFF"),
        ("HEX('10.5')", "31302E35"),
    ]);
}

#[test]
fn conditional_temporals_unify_calendar_kind_and_fractional_precision() {
    pintail_exec::set_session_timestamp_micros(Some(1_593_561_600_000_000));
    let answers = [
        (
            "CONCAT(IFNULL(TIME'00:00:00.567',DATE'2002-01-01'))",
            "2020-07-01 00:00:00.567",
        ),
        (
            "CONCAT(IF(TRUE,TIME'00:00:00.567',DATE'2002-01-01'))",
            "2020-07-01 00:00:00.567",
        ),
        (
            "CONCAT(COALESCE(CAST(NULL AS TIME(3)),DATE'2002-01-01'))",
            "2002-01-01 00:00:00.000",
        ),
        (
            "CONCAT(IFNULL(TIME'01:02:03.1',TIME'02:03:04.123'))",
            "01:02:03.100",
        ),
    ]
    .map(|(query, expected)| (query, expected, scalar(query)));
    pintail_exec::set_session_timestamp_micros(None);
    for (query, expected, answer) in answers {
        assert_eq!(answer, expected, "{query}");
    }
}

#[test]
fn regular_expressions_honor_binary_collation_case_sensitivity() {
    assert_answers(&[
        ("'a' REGEXP 'A' COLLATE utf8mb4_bin", "Boolean(false)"),
        (
            "_latin1'a' REGEXP _latin1'A' COLLATE latin1_bin",
            "Boolean(false)",
        ),
        ("'a' REGEXP 'A' COLLATE utf8mb4_general_ci", "Boolean(true)"),
        (
            "REGEXP_LIKE('a' COLLATE utf8mb4_bin,'A','i')",
            "Boolean(true)",
        ),
    ]);
}

#[test]
fn regex_line_boundaries_preserve_positions_and_captures() {
    for (expression, expected) in [
        (r"REGEXP_LIKE('b\na\n','a$')", "Boolean(true)"),
        (r"REGEXP_LIKE('a\nb\n','(?m)b\\s^')", "Boolean(false)"),
        (r"REGEXP_LIKE('a\n','a$\\n')", "Boolean(true)"),
        (r"REGEXP_LIKE('a\nb','a$')", "Boolean(false)"),
        (r"REGEXP_LIKE('a\n','a\\z')", "Boolean(false)"),
        (r"REGEXP_LIKE('a\n','a\\Z')", "Boolean(true)"),
        (r"REGEXP_SUBSTR('x\nab\n','(a)(b)$')", "ab"),
        (r"REGEXP_INSTR('é\nab\n','ab$')", "3"),
        (r"REGEXP_REPLACE('ab\n','(a)(b)$','$2$1')", "ba\n"),
        (r"REGEXP_LIKE('a\r','a$','u')", "Boolean(false)"),
        (
            "REGEXP_SUBSTR(CONVERT(X'61E280A8' USING utf8mb4),'a$')",
            "a",
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
}
