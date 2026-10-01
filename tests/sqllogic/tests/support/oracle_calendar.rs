//! Dates no calendar holds, stored as `MySQL` stores them when the session
//! allows it: zero dates, a zero month or day, a day past its month's
//! end, and the year zero. `DATE_FORMAT` prints such a date's own fields
//! and counts weekdays, weeks and days of the year from the day `MySQL`'s
//! day numbering gives it (`2024-02-00` is January 31st); the week and day
//! functions refuse a zero part. Every directive is checked against each
//! column, and against literals written with zero parts.

use super::{Column, DataType, KeyPart, OracleCase, PrimaryKey, StoredRow, TableSchema, Value};

pub const TABLE: &str = "calendar_edges";

const ROWS: [(i64, [Option<&str>; 3]); 12] = [
    (
        1,
        [
            Some("0000-00-00"),
            Some("0000-00-00 00:00:00"),
            Some("0000-00-00 00:00:00.000"),
        ],
    ),
    (
        2,
        [
            Some("2024-00-15"),
            Some("2024-00-15 13:04:05"),
            Some("2024-00-15 13:04:05.250"),
        ],
    ),
    (
        3,
        [
            Some("2024-02-00"),
            Some("2024-02-00 00:30:00"),
            Some("2024-02-00 23:59:59.999"),
        ],
    ),
    (
        4,
        [
            Some("2024-00-00"),
            Some("2024-00-00 12:00:00"),
            Some("2024-00-00 01:02:03.004"),
        ],
    ),
    (
        5,
        [
            Some("0000-01-01"),
            Some("0000-01-01 00:00:00"),
            Some("0000-03-00 00:00:00.000"),
        ],
    ),
    (
        6,
        [
            Some("2023-01-00"),
            Some("2023-01-00 08:00:00"),
            Some("2021-01-00 08:00:00.500"),
        ],
    ),
    (
        7,
        [
            Some("2024-12-00"),
            Some("2024-12-00 18:00:00"),
            Some("2025-01-00 18:00:00.000"),
        ],
    ),
    (
        8,
        [
            Some("2024-02-29"),
            Some("2024-02-29 11:59:59"),
            Some("2024-02-29 11:59:59.999"),
        ],
    ),
    (
        9,
        [
            Some("0000-00-05"),
            Some("0000-00-05 00:00:00"),
            Some("0001-00-00 00:00:00.000"),
        ],
    ),
    (
        10,
        [
            Some("2024-02-30"),
            Some("2024-02-30 10:00:00"),
            Some("2024-04-31 10:00:00.000"),
        ],
    ),
    (11, [None, None, None]),
    (
        12,
        [
            Some("0000-03-01"),
            Some("0000-02-28 23:00:00"),
            Some("0001-01-01 00:00:00.000"),
        ],
    ),
];

/// The `MySQL` fixture: written under `ALLOW_INVALID_DATES` alone, which
/// keeps zero parts and a day past its month's end as written.
pub fn sql() -> String {
    let rows = ROWS
        .iter()
        .map(|(id, values)| {
            let values = values
                .iter()
                .map(|value| value.map_or_else(|| "NULL".to_owned(), |value| format!("'{value}'")))
                .collect::<Vec<_>>()
                .join(",");
            format!("({id},{values})")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SET SESSION sql_mode='ALLOW_INVALID_DATES'; \
         CREATE TABLE {TABLE} (id BIGINT PRIMARY KEY, d DATE NULL, dt DATETIME NULL, \
         dt3 DATETIME(3) NULL); INSERT INTO {TABLE} VALUES {rows};"
    )
}

pub fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "d", DataType::Date32, true),
            Column::new(3, "dt", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(4, "dt3", DataType::DateTime64 { fsp: 3 }, true),
        ],
    )
    .expect("calendar schema")
}

pub fn rows() -> Vec<StoredRow> {
    ROWS.iter()
        .map(|(id, values)| {
            let mut row = vec![Value::Int64(*id)];
            row.extend(
                values
                    .iter()
                    .map(|value| value.map_or(Value::Null, |value| Value::Utf8(value.to_owned()))),
            );
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::Int64(*id)]).expect("key"),
                row,
                u64::try_from(*id).expect("positive id"),
                false,
            )
        })
        .collect()
}

pub fn row_count() -> u64 {
    u64::try_from(ROWS.len()).expect("few rows")
}

#[allow(clippy::too_many_lines)] // one table of shapes
pub fn cases() -> Vec<OracleCase> {
    let mut cases = Vec::new();
    let mut push = |sql_mode: &'static str, family: &'static str, sql: String| {
        cases.push(OracleCase {
            sql_mode,
            family,
            sql,
            ordered: true,
        });
    };
    for directive in [
        "a", "b", "c", "D", "d", "e", "f", "H", "h", "I", "i", "j", "k", "l", "M", "m", "p", "r",
        "S", "s", "T", "U", "u", "V", "v", "W", "w", "X", "x", "Y", "y", "%",
    ] {
        push(
            "",
            "calendar edges date_format columns",
            format!(
                "SELECT id, DATE_FORMAT(d, '%{directive}'), DATE_FORMAT(dt, '%{directive}'), \
                 DATE_FORMAT(dt3, '%{directive}') FROM {TABLE} ORDER BY id"
            ),
        );
        // A date written as text reads under the session's zero-date
        // policy, so both sides are given the mode by name: the default
        // refuses a zero date and a day past its month's end, and
        // ALLOW_INVALID_DATES alone takes both.
        for sql_mode in [super::oracle_transport::DEFAULT_MODE, "ALLOW_INVALID_DATES"] {
            push(
                sql_mode,
                "calendar edges date_format literals",
                format!(
                    "SELECT DATE_FORMAT('2024-02-00', '%{directive}'), \
                     DATE_FORMAT('2024-00-15 10:11:12.5', '%{directive}'), \
                     DATE_FORMAT('0000-00-00', '%{directive}'), \
                     DATE_FORMAT('0000-01-00', '%{directive}'), \
                     DATE_FORMAT(20240200, '%{directive}'), \
                     DATE_FORMAT('2024-02-30', '%{directive}')"
                ),
            );
        }
    }
    push(
        "",
        "calendar edges date_format columns",
        format!(
            "SELECT DATE_FORMAT(dt, '%x-%v %a') AS week, COUNT(*) FROM {TABLE} \
             GROUP BY week ORDER BY week"
        ),
    );
    let mut functions = vec![
        "WEEK(@)".to_owned(),
        "YEARWEEK(@)".to_owned(),
        "WEEKOFYEAR(@)".to_owned(),
        "DAYNAME(@)".to_owned(),
        "DAYOFWEEK(@)".to_owned(),
        "WEEKDAY(@)".to_owned(),
        "DAYOFYEAR(@)".to_owned(),
        "MONTHNAME(@)".to_owned(),
        "TO_DAYS(@)".to_owned(),
    ];
    for mode in 0..8 {
        functions.push(format!("WEEK(@, {mode})"));
        functions.push(format!("YEARWEEK(@, {mode})"));
    }
    for function in functions {
        let column = |name: &str| function.replace('@', name);
        push(
            "",
            "calendar edges week and day functions",
            format!(
                "SELECT id, {}, {}, {} FROM {TABLE} ORDER BY id",
                column("d"),
                column("dt"),
                column("dt3")
            ),
        );
        // A day past its month's end written as text is a date only
        // under ALLOW_INVALID_DATES, where it counts as the day it runs
        // into; a zero day is refused by these functions in either mode.
        for sql_mode in [super::oracle_transport::DEFAULT_MODE, "ALLOW_INVALID_DATES"] {
            push(
                sql_mode,
                "calendar edges week and day literals",
                format!(
                    "SELECT {}, {}, {}, {}",
                    column("'2024-02-30'"),
                    column("'2024-04-31 10:00:00'"),
                    column("'2023-02-29'"),
                    column("'2024-02-00'")
                ),
            );
        }
    }
    // The calendar parts of a date with a zero month or day are its fields
    // as written, in a column, as text or as a packed number; a zero date
    // written as text is refused under NO_ZERO_DATE. LAST_DAY needs only
    // the month, TO_SECONDS and DATEDIFF count days as TO_DAYS does.
    for function in [
        "YEAR(@)",
        "MONTH(@)",
        "DAY(@)",
        "DAYOFMONTH(@)",
        "QUARTER(@)",
        "LAST_DAY(@)",
        "TO_SECONDS(@)",
        "DATEDIFF(@, '2024-01-01')",
        "DATEDIFF('2024-03-05', @)",
    ] {
        let of = |argument: &str| function.replace('@', argument);
        push(
            "",
            "calendar edges date parts",
            format!(
                "SELECT id, {}, {}, {} FROM {TABLE} ORDER BY id",
                of("d"),
                of("dt"),
                of("dt3")
            ),
        );
        for sql_mode in [super::oracle_transport::DEFAULT_MODE, "ALLOW_INVALID_DATES"] {
            push(
                sql_mode,
                "calendar edges date part literals",
                format!(
                    "SELECT {}, {}, {}, {}, {}, {}, {}",
                    of("'2024-02-00'"),
                    of("'2024-00-15 10:11:12.5'"),
                    of("'0000-00-00'"),
                    of("'0000-01-00'"),
                    of("20240200"),
                    of("0"),
                    of("'2024-02-30'")
                ),
            );
        }
    }
    interval_cases(&mut push);
    cases
}

/// Interval arithmetic and week extraction at the edges: the year zero,
/// which `MySQL`'s day numbering gives no date of its own; the last day a
/// date can hold; days past a month's end; and a TIME read as a date.
#[allow(clippy::too_many_lines)] // one table of shapes
fn interval_cases(push: &mut impl FnMut(&'static str, &'static str, String)) {
    let modes = [super::oracle_transport::DEFAULT_MODE, "ALLOW_INVALID_DATES"];
    for function in [
        "EXTRACT(WEEK FROM @)",
        "DATE_ADD(@, INTERVAL 1 DAY)",
        "DATE_SUB(@, INTERVAL 1 DAY)",
        "DATE_ADD(@, INTERVAL 1 SECOND)",
        "DATE_SUB(@, INTERVAL 1 MONTH)",
        "DATE_ADD(@, INTERVAL 1 YEAR)",
        "ADDDATE(@, 1)",
        "SUBDATE(@, INTERVAL 1 MONTH)",
        "TIMESTAMPADD(DAY, 1, @)",
        "TIMESTAMPDIFF(DAY, @, '2024-03-05')",
        "TIMESTAMPDIFF(MONTH, '2023-01-01', @)",
        "TIME_TO_SEC(@)",
    ] {
        let of = |argument: &str| function.replace('@', argument);
        push(
            "",
            "calendar edges interval arithmetic",
            format!(
                "SELECT id, {}, {}, {} FROM {TABLE} ORDER BY id",
                of("d"),
                of("dt"),
                of("dt3")
            ),
        );
        // TIME_TO_SEC reads text as a time, not as a date: columns only.
        if function.starts_with("TIME_TO_SEC") {
            continue;
        }
        for sql_mode in modes {
            push(
                sql_mode,
                "calendar edges interval literals",
                format!(
                    "SELECT {}, {}, {}, {}, {}, {}, {}, {}",
                    of("'0000-01-01'"),
                    of("'0000-12-31 23:59:59'"),
                    of("'0001-01-01'"),
                    of("'9999-12-31 23:59:59'"),
                    of("'2024-02-30'"),
                    of("'2024-04-31 10:00:00.5'"),
                    of("'2023-02-29'"),
                    of("'0000-06-15'")
                ),
            );
        }
    }
    push(
        "",
        "calendar edges interval literals",
        "SELECT DATE_ADD('0000-12-31', INTERVAL 1 DAY), DATE_SUB('0001-01-01', INTERVAL 1 DAY), \
         DATE_SUB('0001-01-01 00:00:00', INTERVAL 1 SECOND), \
         DATE_ADD('9999-12-31', INTERVAL 1 DAY), DATE_ADD('9999-12-01', INTERVAL 1 MONTH), \
         DATE_SUB('0000-03-01', INTERVAL 1 DAY), DATE_ADD('0000-02-28', INTERVAL 1 DAY), \
         DATE_ADD('0000-01-01', INTERVAL 1 YEAR), DATE_SUB('0001-06-15', INTERVAL 1 YEAR), \
         DATE_SUB('0000-01-01', INTERVAL 1 DAY), DATE_SUB('0000-01-01', INTERVAL 2 DAY), \
         DATE_ADD('0001-01-01', INTERVAL -1 MONTH), DATE_ADD('0000-06-15', INTERVAL 400 DAY), \
         DATE_ADD('0000-02-28', INTERVAL 400 DAY), DATE_ADD('0000-06-15', INTERVAL 1 HOUR), \
         DATE_ADD('9998-12-31', INTERVAL 1 YEAR)"
            .to_owned(),
    );
    push(
        "",
        "calendar edges interval literals",
        "SELECT FROM_DAYS(0), FROM_DAYS(365), FROM_DAYS(366), FROM_DAYS(3652424), \
         FROM_DAYS(3652425), FROM_DAYS(3652499), FROM_DAYS(3652500), FROM_DAYS(4000000), \
         FROM_DAYS(-1), \
         UNIX_TIMESTAMP('2024-02-29 11:59:59.1234567') - UNIX_TIMESTAMP('2024-02-29 11:59:59'), \
         UNIX_TIMESTAMP('2024-02-29 11:59:59.1234564') - UNIX_TIMESTAMP('2024-02-29 11:59:59')"
            .to_owned(),
    );
    // TIME_FORMAT prints an hour past 23 as it is and reads %h and %p within
    // its day; a directive that needs a date is NULL. ADDTIME and SUBTIME
    // count a zero month or day from the day MySQL's numbering gives it,
    // and answer NULL outside the years 1 to 9999.
    push(
        "",
        "time formats and arithmetic at the edges",
        "SELECT TIME_FORMAT('25:00:00', '%H %k %h %I %l %p %r %T %i %s %f'), \
         TIME_FORMAT('838:59:59', '%H %k %h %I %l %p %r %T'), \
         TIME_FORMAT('-25:30:15.5', '%H %k %h %I %l %p %r %T %f'), \
         TIME_FORMAT('100:00:00', '%H:%i'), TIME_FORMAT('25:00:00', '%Y %m %d %a %j'), \
         TIME_FORMAT(TIME'24:00:00', '%H %h %p'), TIME_FORMAT('12:00:00', '%h %p %r'), \
         TIME_FORMAT('00:00:00', '%h %l %p'), TIME_FORMAT('2024-02-29 13:00:00', '%H %h')"
            .to_owned(),
    );
    push(
        "",
        "time formats and arithmetic at the edges",
        "SELECT id, TIME_FORMAT(clock, '%H:%i:%s.%f %p %h %k %l') FROM bounds ORDER BY id"
            .to_owned(),
    );
    for function in [
        "TIME_FORMAT(@, '%H:%i:%s %p %h %k %l')",
        "ADDTIME(@, '01:00:00')",
        "SUBTIME(@, '01:00:00')",
    ] {
        let of = |argument: &str| function.replace('@', argument);
        push(
            "",
            "time formats and arithmetic at the edges",
            format!(
                "SELECT id, {}, {}, {} FROM {TABLE} ORDER BY id",
                of("d"),
                of("dt"),
                of("dt3")
            ),
        );
    }
    // A TIME where a date is read is the statement's date at that time;
    // measured against CURDATE() the answer does not depend on the day.
    push(
        "",
        "time read as a date",
        "SELECT id, TO_DAYS(clock) - TO_DAYS(CURDATE()), DATEDIFF(clock, CURDATE()), \
         TIMESTAMPDIFF(MINUTE, CURDATE(), clock), TO_SECONDS(clock) - TO_SECONDS(CURDATE()) \
         FROM bounds ORDER BY id"
            .to_owned(),
    );
}
