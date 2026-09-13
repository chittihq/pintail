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
        ("_ucs2 X'0420'", "Р"),
        ("_utf16 'ab'", "慢"),
        ("HEX(_binary 'ab')", "6162"),
    ]);
    // Reading these bytes as UTF-8 would answer different text.
    for refused in ["_ucs2 X'D800'", "_utf16 X'D800'", "_latin1 'caf\u{e9}'"] {
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
