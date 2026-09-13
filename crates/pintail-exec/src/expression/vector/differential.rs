//! Typed kernels against row evaluation, over generated expressions and data.
//!
//! Every kernel is an optimisation of an answer row evaluation already gives,
//! so row evaluation is the reference: wherever a kernel returns a column, each
//! selected row must hold exactly the value `evaluate` produces for it, and a
//! kernel must never produce a value where row evaluation refuses. Expressions
//! are generated as SQL and typed by the real binder, so the trees are the
//! ones production compiles; the data covers each type's edges, NULLs and a
//! partial selection.
//!
//! `PINTAIL_KERNEL_DIFF_CASES` widens a run; `PINTAIL_KERNEL_DIFF_SEED`
//! chooses the seed.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_sql::{Binder, parse_statement};
use pintail_types::{
    Column, DataType, Float64, TableSchema, Value, format_date_days, format_datetime_micros,
    format_decimal_scaled, format_time_micros,
};

use crate::batch::{ColumnVector, RecordBatch, SelectionMask};
use crate::collation::Collation;
use crate::expression::CompiledExpr;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).expect("bound fits usize")
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// The generated table: one column per type family the kernels specialise.
fn columns() -> Vec<Column> {
    vec![
        Column::new(1, "i", DataType::Int64, true),
        Column::new(2, "j", DataType::Int64, true),
        Column::new(3, "u", DataType::UInt64, true),
        Column::new(4, "f", DataType::Float64, true),
        Column::new(
            5,
            "d",
            DataType::Decimal {
                precision: 18,
                scale: 4,
            },
            true,
        ),
        Column::new(
            6,
            "w",
            DataType::Decimal {
                precision: 30,
                scale: 2,
            },
            true,
        ),
        Column::new(7, "dt", DataType::Date32, true),
        Column::new(8, "ts", DataType::DateTime64 { fsp: 3 }, true),
        Column::new(9, "tm", DataType::Time64 { fsp: 2 }, true),
        Column::new(10, "s", DataType::Utf8, true)
            .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
        Column::new(11, "g", DataType::Utf8, true)
            .with_collation(Some("utf8mb4_general_ci".to_owned())),
    ]
}

fn value(rng: &mut Rng, data_type: DataType) -> Value {
    if rng.chance(12) {
        return Value::Null;
    }
    match data_type {
        DataType::Int64 => Value::Int64(*rng.pick(&[
            0,
            1,
            -1,
            7,
            -13,
            100,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
            i64::MAX,
            i64::MIN,
            i64::MAX - 1,
            9_007_199_254_740_993,
        ])),
        DataType::UInt64 => {
            Value::UInt64(*rng.pick(&[0, 1, 2, 255, u64::MAX, 1 << 63, (1 << 63) - 1, 1_000_000]))
        }
        DataType::Float64 => Value::Float64(Float64::new(*rng.pick(&[
            0.0,
            -0.0,
            1.5,
            -2.25,
            0.1,
            1e-300,
            1e300,
            -1e15,
            123_456.789,
            0.5,
            2.5,
        ]))),
        DataType::Decimal { precision, scale } => {
            let limit = 10_i128.pow(u32::from(precision)) - 1;
            let units = match rng.below(6) {
                0 => 0,
                1 => limit,
                2 => -limit,
                3 => 5 * 10_i128.pow(u32::from(scale.saturating_sub(1))),
                4 => -(i128::try_from(rng.below(100_000)).expect("small")),
                _ => {
                    i128::try_from(rng.below(1_000_000_000)).expect("small")
                        * if rng.chance(50) { -1 } else { 1 }
                }
            };
            Value::Utf8(format_decimal_scaled(units, scale))
        }
        DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. } => {
            temporal_value(rng, data_type)
        }
        _ => Value::Utf8(
            (*rng.pick(&[
                "",
                "a",
                "A",
                "ä",
                "Å",
                "abc",
                "ABC ",
                " x",
                "ß",
                "ss",
                "10",
                "9",
                "-1.5e3",
                "2020-01-01",
                "日本",
                "😀",
            ]))
            .to_owned(),
        ),
    }
}

fn temporal_value(rng: &mut Rng, data_type: DataType) -> Value {
    match data_type {
        DataType::Date32 => {
            let days = *rng.pick(&[-719_162_i64 + 365, 0, 10_957, 18_321, 2_932_896, -1, 11_016]);
            Value::Utf8(format_date_days(days).unwrap_or_else(|| "2000-01-01".to_owned()))
        }
        DataType::DateTime64 { fsp } => {
            let micros = *rng.pick(&[
                0_i64,
                1_000,
                946_684_799_999_000,
                951_782_400_000_000,
                253_402_300_799_999_000,
                -30_610_224_000_000_000,
                1_700_000_000_123_000,
            ]);
            Value::Utf8(
                format_datetime_micros(micros, fsp)
                    .unwrap_or_else(|| "2000-01-01 00:00:00.000".to_owned()),
            )
        }
        DataType::Time64 { fsp } => {
            let micros = *rng.pick(&[
                0_i64,
                -10_000,
                10_000,
                -1_500_000,
                3_599_990_000,
                -3_020_399_000_000,
                3_020_399_000_000,
                86_400_000_000,
                -86_400_010_000,
            ]);
            Value::Utf8(format_time_micros(micros, fsp))
        }
        _ => Value::Null,
    }
}

const INTEGERS: &[&str] = &[
    "i",
    "j",
    "u",
    "0",
    "1",
    "-1",
    "7",
    "9223372036854775807",
    "18446744073709551615",
];
const EXACTS: &[&str] = &["d", "w", "1.5", "-0.0001", "99999999999999.9999"];
const APPROXIMATES: &[&str] = &["f", "1e3", "-2.5e-3"];
const TEXTS: &[&str] = &["s", "g", "'a'", "'A'", "'ä'", "'abc'", "''", "'10'"];
const TEMPORALS: &[&str] = &[
    "dt",
    "ts",
    "tm",
    "'2020-01-01'",
    "'2000-02-29 12:00:00'",
    "'-01:00:00'",
];

fn numeric(rng: &mut Rng, depth: usize) -> String {
    if depth == 0 || rng.chance(30) {
        return match rng.below(3) {
            0 => rng.pick(INTEGERS).to_string(),
            1 => rng.pick(EXACTS).to_string(),
            _ => rng.pick(APPROXIMATES).to_string(),
        };
    }
    let a = numeric(rng, depth - 1);
    let b = numeric(rng, depth - 1);
    match rng.below(18) {
        0 => format!("({a} + {b})"),
        1 => format!("({a} - {b})"),
        2 => format!("({a} * {b})"),
        3 => format!("({a} / {b})"),
        4 => format!("({a} DIV {b})"),
        5 => format!("({a} % {b})"),
        6 => format!("(-{a})"),
        7 => format!("ABS({a})"),
        8 => format!(
            "ROUND({a}, {})",
            i64::try_from(rng.below(5)).expect("small") - 2
        ),
        9 => format!("TRUNCATE({a}, {})", rng.below(4)),
        10 => format!("FLOOR({a})"),
        11 => format!("CEILING({a})"),
        12 => format!("COALESCE({a}, {b})"),
        13 => format!("IF({}, {a}, {b})", predicate(rng, depth - 1)),
        14 => format!(
            "CASE WHEN {} THEN {a} ELSE {b} END",
            predicate(rng, depth - 1)
        ),
        15 => format!("LENGTH({})", text(rng, depth - 1)),
        16 => format!(
            "{}({})",
            rng.pick(&[
                "YEAR",
                "MONTH",
                "DAYOFMONTH",
                "DAYOFWEEK",
                "DAYOFYEAR",
                "QUARTER"
            ]),
            rng.pick(&["dt", "ts"])
        ),
        _ => format!(
            "{}({})",
            rng.pick(&["HOUR", "MINUTE", "SECOND", "TIME_TO_SEC"]),
            rng.pick(&["tm", "ts"])
        ),
    }
}

fn text(rng: &mut Rng, depth: usize) -> String {
    if depth == 0 || rng.chance(40) {
        return rng.pick(TEXTS).to_string();
    }
    let a = text(rng, depth - 1);
    match rng.below(9) {
        0 => format!("UPPER({a})"),
        1 => format!("LOWER({a})"),
        2 => format!("CONCAT({a}, {})", text(rng, depth - 1)),
        3 => format!("SUBSTRING({a}, {}, {})", rng.below(3) + 1, rng.below(3)),
        4 => format!("TRIM({a})"),
        5 => format!("REPLACE({a}, 'a', 'b')"),
        6 => format!(
            "DATE_FORMAT({}, '{}')",
            rng.pick(&["dt", "ts"]),
            rng.pick(&["%Y-%m-%d", "%H:%i:%s.%f", "%W %M %e %j", "%x-%v %U"])
        ),
        7 => format!("CAST({} AS CHAR)", numeric(rng, depth - 1)),
        _ => format!("COALESCE({a}, {})", text(rng, depth - 1)),
    }
}

fn predicate(rng: &mut Rng, depth: usize) -> String {
    let comparison = rng
        .pick(&["=", "<>", "<", "<=", ">", ">=", "<=>"])
        .to_string();
    let base = match rng.below(9) {
        0 | 1 => format!(
            "{} {comparison} {}",
            numeric(rng, depth.saturating_sub(1)),
            numeric(rng, depth.saturating_sub(1))
        ),
        2 => format!(
            "{} {comparison} {}",
            text(rng, depth.saturating_sub(1)),
            text(rng, depth.saturating_sub(1))
        ),
        3 => format!(
            "{} {comparison} {}",
            rng.pick(TEMPORALS),
            rng.pick(TEMPORALS)
        ),
        4 => format!(
            "{} BETWEEN {} AND {}",
            numeric(rng, 0),
            numeric(rng, 0),
            numeric(rng, 0)
        ),
        5 => format!(
            "{} IN ({}, {}, {})",
            rng.pick(&["i", "u", "d", "s", "g"]),
            rng.pick(INTEGERS),
            rng.pick(EXACTS),
            rng.pick(TEXTS)
        ),
        6 => format!(
            "{} IS {}NULL",
            rng.pick(&["i", "d", "s", "ts", "tm"]),
            if rng.chance(50) { "NOT " } else { "" }
        ),
        7 => format!(
            "{} LIKE '{}'",
            text(rng, 0),
            rng.pick(&["a%", "%B%", "_", "", "ä%"])
        ),
        _ => format!(
            "{} {comparison} {}",
            rng.pick(&["i", "u", "f", "d", "w"]),
            rng.pick(&["s", "'10'", "dt"])
        ),
    };
    if depth == 0 || rng.chance(60) {
        return base;
    }
    match rng.below(3) {
        0 => format!("({base} AND {})", predicate(rng, depth - 1)),
        1 => format!("({base} OR {})", predicate(rng, depth - 1)),
        _ => format!("(NOT {base})"),
    }
}

fn catalog() -> CatalogSnapshot {
    let schema = TableSchema::new(1, columns()).expect("schema");
    let entry = TableEntry::new(
        TableId::new(2),
        "t",
        schema,
        TableStatistics::with_row_count(64),
    )
    .expect("table");
    let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
    CatalogSnapshot::new([database]).expect("catalog")
}

/// Binds `SELECT <expression> FROM t` and compiles the projection as the
/// planner would, or `None` when the binder refuses the expression.
fn compile(catalog: &CatalogSnapshot, expression: &str) -> Option<(CompiledExpr, DataType)> {
    let statement = parse_statement(&format!("SELECT {expression} FROM t")).ok()?;
    let query = Binder::new(catalog, Some("app")).bind(&statement).ok()?;
    let projection = query.projection.first()?;
    let data_type = projection.expr.data_type?;
    let columns = &query.tables.first()?.columns;
    let compiled = CompiledExpr::compile(&projection.expr, columns, Collation::default()).ok()?;
    Some((compiled, data_type))
}

fn batch(rng: &mut Rng, rows: usize) -> RecordBatch {
    let vectors = columns()
        .iter()
        .map(|column| {
            let values = (0..rows).map(|_| value(rng, column.data_type())).collect();
            ColumnVector::new(column.data_type(), values).expect("column")
        })
        .collect();
    let mut batch = RecordBatch::new(rows, vectors).expect("batch");
    let mut selection = SelectionMask::all(rows);
    for row in 0..rows {
        if rng.chance(20) {
            selection.set(row, false).expect("row");
        }
    }
    batch.set_selection(selection).expect("selection");
    batch
}

#[derive(Default)]
struct Tally {
    bound: usize,
    kernel_columns: usize,
    compared_values: usize,
    failures: Vec<String>,
}

fn check(
    tally: &mut Tally,
    sql: &str,
    expression: &CompiledExpr,
    data_type: DataType,
    batch: &RecordBatch,
) {
    tally.bound += 1;
    let Some(column) = expression.evaluate_column(batch, Some(data_type)) else {
        let _ = crate::execution::take_session_division_warnings();
        return;
    };
    let _ = crate::execution::take_session_division_warnings();
    tally.kernel_columns += 1;
    for row in batch.selection().selected_rows() {
        let expected = expression.evaluate(batch, row);
        let actual = column.value(row);
        tally.compared_values += 1;
        let agrees = match &expected {
            Ok(value) => actual == Some(value),
            Err(_) => false,
        };
        if !agrees && tally.failures.len() < 12 {
            let inputs = (0..batch.columns().len())
                .map(|index| format!("{:?}", batch.column(index).and_then(|c| c.value(row))))
                .collect::<Vec<_>>()
                .join(", ");
            tally.failures.push(format!(
                "{sql}\n    row {row} [{inputs}]\n    kernel {actual:?}\n    row evaluation {expected:?}"
            ));
        }
    }
    let _ = crate::execution::take_session_division_warnings();
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

#[test]
fn every_kernel_answer_is_row_evaluations_answer() {
    let seed = env_number("PINTAIL_KERNEL_DIFF_SEED", 0x5eed_cafe);
    let cases = env_number("PINTAIL_KERNEL_DIFF_CASES", 3_000);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let catalog = catalog();
    let mut tally = Tally::default();
    for case in 0..cases {
        let sql = match case % 3 {
            0 => numeric(&mut rng, 3),
            1 => text(&mut rng, 3),
            _ => predicate(&mut rng, 3),
        };
        let Some((expression, data_type)) = compile(&catalog, &sql) else {
            continue;
        };
        let batch = batch(&mut rng, 48);
        check(&mut tally, &sql, &expression, data_type, &batch);
    }
    assert!(
        tally.failures.is_empty(),
        "seed {seed:#x}: {} kernel answers differed from row evaluation (showing up to 12):\n{}",
        tally.failures.len(),
        tally.failures.join("\n")
    );
    // A generator that stops reaching kernels proves nothing.
    assert!(
        tally.kernel_columns * 4 >= tally.bound,
        "only {} of {} bound expressions reached a kernel",
        tally.kernel_columns,
        tally.bound
    );
    eprintln!(
        "kernel differential: {} bound, {} kernel columns, {} values compared",
        tally.bound, tally.kernel_columns, tally.compared_values
    );
}
