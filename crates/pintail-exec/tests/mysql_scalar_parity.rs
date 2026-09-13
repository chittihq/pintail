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
    TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).expect("schema")
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
    let directory = tempfile::tempdir().expect("tempdir");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("open");
    table
        .bulk_ingest_snapshot(vec![StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
            vec![Value::UInt64(1)],
            1,
            false,
        )])
        .expect("ingest");
    let snapshot = table.snapshot();
    let database_id = DatabaseId::new(1);
    let table_id = TableId::new(1);
    let entry = TableEntry::new(
        table_id,
        "one",
        schema(),
        TableStatistics::with_row_count(1),
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
    ]);
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
        ("HEX(_binary 'ab')", "6162"),
    ]);
    // Reading these bytes as UTF-8 would answer different text.
    for refused in ["_ucs2 X'0420'", "_utf16 'ab'", "_latin1 'caf\u{e9}'"] {
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
