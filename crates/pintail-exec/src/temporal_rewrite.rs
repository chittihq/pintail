//! Rewrites date-function predicates into plain ranges on the column.
//!
//! `DATE(created_at) BETWEEN '2026-08-01' AND '2026-08-07'` is how most
//! dashboards spell a day range, and it is the worst shape for a columnar
//! scan: a function wraps the column, so segment/block pruning sees no
//! bounds, and the vectorized comparison mask declines the expression, so
//! every row renders its datetime to text, re-parses it, and compares
//! strings. On a 10M-row table that took 15 seconds against 4 milliseconds
//! for the equivalent `created_at >= '2026-08-01 00:00:00' AND created_at
//! < '2026-08-08 00:00:00'`.
//!
//! This pass produces that equivalent form. Every rewrite is exact under
//! `MySQL` semantics, including NULL: each output compares the same column
//! the function wrapped, so the result is NULL exactly when the column is
//! NULL, which is when `DATE(column)` is NULL. Rewrites apply only when
//! the literal is a canonical calendar date (or an integer year for
//! `YEAR`); anything else keeps the original expression, where the runtime
//! already matches `MySQL`.
//!
//! Shapes, with `L` the first instant of the literal's day (or year) and
//! `U` the first instant after it:
//!
//! | written                    | rewritten                     |
//! |----------------------------|-------------------------------|
//! | `DATE(c) = d`              | `c >= L AND c < U`            |
//! | `DATE(c) <> d`             | `c < L OR c >= U`             |
//! | `DATE(c) < d` / `<= d`     | `c < L` / `c < U`             |
//! | `DATE(c) > d` / `>= d`     | `c >= U` / `c >= L`           |
//! | `DATE(c) BETWEEN a AND b`  | `c >= L(a) AND c < U(b)`      |
//! | `DATE(c) NOT BETWEEN ...`  | `c < L(a) OR c >= U(b)`       |
//! | `DATE(c) IN (d1, d2)`      | OR of the equality ranges     |
//! | `DATE(c) NOT IN (d1, d2)`  | AND of the inequality ranges  |
//!
//! `CAST(c AS DATE)` is the same function under another spelling, and
//! `YEAR(c)` uses the same table with year-wide bounds. A datetime literal
//! at exactly midnight names its day, as `MySQL` promotes the DATE to
//! midnight before comparing; any other time of day is left alone. The
//! column must be a `DATETIME` or `DATE`; `DATE()` over text is a real
//! parse of arbitrary input and is left alone.

use pintail_sql::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, DatePart, ScalarFunction, UnaryOp,
};
use pintail_types::{
    DataType, Value, format_date_days, format_datetime_micros, parse_date_days,
    parse_datetime_micros,
};

use crate::LogicalPlan;

/// Rewrites every filter and join predicate in the plan.
pub(crate) fn rewrite_temporal_predicates(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Empty | LogicalPlan::OneRow | LogicalPlan::Scan(_) => plan,
        LogicalPlan::Derived { input, columns } => LogicalPlan::Derived {
            input: Box::new(rewrite_temporal_predicates(*input)),
            columns,
        },
        LogicalPlan::CrossJoin { inputs } => LogicalPlan::CrossJoin {
            inputs: inputs
                .into_iter()
                .map(rewrite_temporal_predicates)
                .collect(),
        },
        LogicalPlan::UnionAll { inputs } => LogicalPlan::UnionAll {
            inputs: inputs
                .into_iter()
                .map(rewrite_temporal_predicates)
                .collect(),
        },
        LogicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => LogicalPlan::SetOp {
            keep_matching,
            all,
            left: Box::new(rewrite_temporal_predicates(*left)),
            right: Box::new(rewrite_temporal_predicates(*right)),
        },
        LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor,
            member,
        } => LogicalPlan::Recursive {
            working_database,
            working_table,
            distinct,
            anchor: Box::new(rewrite_temporal_predicates(*anchor)),
            member: Box::new(rewrite_temporal_predicates(*member)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(rewrite_temporal_predicates(*left)),
            right: Box::new(rewrite_temporal_predicates(*right)),
            kind,
            condition: condition.map(rewrite_predicate),
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(rewrite_temporal_predicates(*input)),
            predicate: rewrite_predicate(predicate),
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(rewrite_temporal_predicates(*input)),
            group_by,
            aggregates,
        },
        LogicalPlan::Window {
            input,
            windows,
            outputs,
        } => LogicalPlan::Window {
            input: Box::new(rewrite_temporal_predicates(*input)),
            windows,
            outputs,
        },
        LogicalPlan::Project { input, expressions } => LogicalPlan::Project {
            input: Box::new(rewrite_temporal_predicates(*input)),
            expressions,
        },
        LogicalPlan::Distinct {
            input,
            key_collations,
        } => LogicalPlan::Distinct {
            input: Box::new(rewrite_temporal_predicates(*input)),
            key_collations,
        },
        LogicalPlan::Sort { input, keys, trim } => LogicalPlan::Sort {
            input: Box::new(rewrite_temporal_predicates(*input)),
            keys,
            trim,
        },
        LogicalPlan::Limit { input, limit } => LogicalPlan::Limit {
            input: Box::new(rewrite_temporal_predicates(*input)),
            limit,
        },
    }
}

/// Rewrites the comparison leaves of a truth-valued expression, descending
/// through `AND`, `OR` and `NOT` only: a date function nested inside
/// arithmetic or another function is not a range predicate.
pub(crate) fn rewrite_predicate(expr: BoundExpr) -> BoundExpr {
    match expr.kind {
        BoundExprKind::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            left,
            right,
        } => BoundExpr {
            kind: BoundExprKind::Binary {
                op,
                left: Box::new(rewrite_predicate(*left)),
                right: Box::new(rewrite_predicate(*right)),
            },
            ..expr
        },
        BoundExprKind::Unary {
            op: UnaryOp::Not,
            expr: inner,
        } => BoundExpr {
            kind: BoundExprKind::Unary {
                op: UnaryOp::Not,
                expr: Box::new(rewrite_predicate(*inner)),
            },
            ..expr
        },
        kind => {
            let expr = BoundExpr { kind, ..expr };
            let expr = rewrite_session_reading(&expr).unwrap_or(expr);
            rewrite_comparison(&expr).unwrap_or(expr)
        }
    }
}

/// The column and fixed offset of a session-zone `TIMESTAMP` reading:
/// `SessionTimestamp(column, '+05:30')`. A named zone is not a single
/// shift - across a daylight-saving change it is two - so only an offset
/// is taken.
fn session_reading(expr: &BoundExpr) -> Option<(&BoundColumn, i64)> {
    let BoundExprKind::Scalar {
        function: ScalarFunction::SessionTimestamp,
        args,
    } = &expr.kind
    else {
        return None;
    };
    let [source, zone] = args.as_slice() else {
        return None;
    };
    let BoundExprKind::Column(column) = &source.kind else {
        return None;
    };
    let BoundExprKind::Literal(Value::Utf8(zone)) = &zone.kind else {
        return None;
    };
    Some((column, fixed_offset_micros(zone)?))
}

/// `+HH:MM` or `-HH:MM` in microseconds; `None` for any other zone.
fn fixed_offset_micros(zone: &str) -> Option<i64> {
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = zone[1..].split_once(':')?;
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if hours > 14 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3_600_000_000 + minutes * 60_000_000))
}

/// The literal shifted out of the session's zone, as the stored column
/// holds it.
fn shifted_literal(value: &Value, offset: i64) -> Option<BoundExpr> {
    let Value::Utf8(text) = value else {
        return None;
    };
    let micros = parse_datetime_micros(text)?;
    let fsp = if text.contains('.') { 6 } else { 0 };
    let shifted = format_datetime_micros(micros.checked_sub(offset)?, fsp)?;
    Some(BoundExpr {
        data_type: Some(DataType::Utf8),
        nullable: false,
        kind: BoundExprKind::Literal(Value::Utf8(shifted)),
    })
}

/// `reading(c) <op> literal` as `c <op> literal shifted out of the zone`.
///
/// A session zone shifts every stored value by the same amount, so shifting
/// the literal the other way compares the same rows - and compares the
/// stored column itself, which storage prunes segments, blocks and key
/// ranges by. Wrapped in the reading, the column is a function's argument
/// and every such filter reads the whole table.
fn rewrite_session_reading(expr: &BoundExpr) -> Option<BoundExpr> {
    match &expr.kind {
        BoundExprKind::Binary {
            op:
                op @ (BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessOrEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterOrEqual),
            left,
            right,
        } => {
            let (column, offset, literal, op) =
                match (session_reading(left), session_reading(right)) {
                    (Some((column, offset)), None) => (column, offset, right.as_ref(), *op),
                    (None, Some((column, offset))) => (column, offset, left.as_ref(), mirror(*op)),
                    _ => return None,
                };
            let BoundExprKind::Literal(value) = &literal.kind else {
                return None;
            };
            Some(BoundExpr {
                data_type: Some(DataType::Boolean),
                nullable: expr.nullable,
                kind: BoundExprKind::Binary {
                    op,
                    left: Box::new(column_expr(column)),
                    right: Box::new(shifted_literal(value, offset)?),
                },
            })
        }
        BoundExprKind::Scalar {
            function: function @ ScalarFunction::Between { .. },
            args,
        } => {
            let [subject, low, high] = args.as_slice() else {
                return None;
            };
            let (column, offset) = session_reading(subject)?;
            let (BoundExprKind::Literal(low), BoundExprKind::Literal(high)) =
                (&low.kind, &high.kind)
            else {
                return None;
            };
            Some(BoundExpr {
                data_type: Some(DataType::Boolean),
                nullable: expr.nullable,
                kind: BoundExprKind::Scalar {
                    function: *function,
                    args: vec![
                        column_expr(column),
                        shifted_literal(low, offset)?,
                        shifted_literal(high, offset)?,
                    ],
                },
            })
        }
        _ => None,
    }
}

/// Which calendar unit a wrapped column is reduced to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unit {
    /// `DATE(c)` and `CAST(c AS DATE)`.
    Day,
    /// `YEAR(c)`.
    Year,
}

/// A column wrapped by a rewritable date function.
struct Subject<'a> {
    column: &'a BoundColumn,
    unit: Unit,
}

fn subject(expr: &BoundExpr) -> Option<Subject<'_>> {
    let BoundExprKind::Scalar { function, args } = &expr.kind else {
        return None;
    };
    let [argument] = args.as_slice() else {
        return None;
    };
    let BoundExprKind::Column(column) = &argument.kind else {
        return None;
    };
    if !matches!(
        column.data_type,
        DataType::DateTime64 { .. } | DataType::Date32
    ) {
        return None;
    }
    let unit = match function {
        ScalarFunction::Date
        | ScalarFunction::Cast(DataType::Date32)
        | ScalarFunction::DeclaredCast {
            target: DataType::Date32,
            ..
        } => Unit::Day,
        ScalarFunction::DatePart(DatePart::Year) => Unit::Year,
        _ => return None,
    };
    Some(Subject { column, unit })
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The half-open day range `[lower, upper)` a literal denotes for the unit,
/// in days since the epoch. `None` for a literal `MySQL` would not treat as
/// that unit's exact value.
fn literal_days(unit: Unit, value: &Value) -> Option<(i64, i64)> {
    match (unit, value) {
        (Unit::Day, Value::Utf8(text)) => {
            // `MySQL` compares a DATE against a datetime literal by promoting
            // the date to midnight, so a literal at exactly midnight names
            // that calendar day. Any other time of day is not a day and keeps
            // the original evaluation.
            let day = parse_date_days(text).or_else(|| {
                let micros = parse_datetime_micros(text)?;
                (micros.rem_euclid(MICROS_PER_DAY) == 0).then(|| micros.div_euclid(MICROS_PER_DAY))
            })?;
            Some((day, day.checked_add(1)?))
        }
        (Unit::Year, Value::Int64(year)) => year_days(*year),
        (Unit::Year, Value::UInt64(year)) => year_days(i64::try_from(*year).ok()?),
        _ => None,
    }
}

fn year_days(year: i64) -> Option<(i64, i64)> {
    // Four-digit years only: the next year must also be representable, so
    // 9999 keeps its original evaluation.
    if !(0..=9998).contains(&year) {
        return None;
    }
    let lower = parse_date_days(&format!("{year:04}-01-01"))?;
    let upper = parse_date_days(&format!("{:04}-01-01", year + 1))?;
    Some((lower, upper))
}

/// The literal the column compares against for one epoch day boundary.
fn boundary(column: &BoundColumn, days: i64) -> Option<BoundExpr> {
    let date = format_date_days(days)?;
    let text = match column.data_type {
        DataType::DateTime64 { .. } => format!("{date} 00:00:00"),
        DataType::Date32 => date,
        _ => return None,
    };
    Some(BoundExpr {
        data_type: Some(DataType::Utf8),
        nullable: false,
        kind: BoundExprKind::Literal(Value::Utf8(text)),
    })
}

fn column_expr(column: &BoundColumn) -> BoundExpr {
    BoundExpr {
        data_type: Some(column.data_type),
        nullable: column.nullable,
        kind: BoundExprKind::Column(column.clone()),
    }
}

fn compare(column: &BoundColumn, op: BinaryOp, days: i64) -> Option<BoundExpr> {
    Some(BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: column.nullable,
        kind: BoundExprKind::Binary {
            op,
            left: Box::new(column_expr(column)),
            right: Box::new(boundary(column, days)?),
        },
    })
}

fn logical(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: left.nullable || right.nullable,
        kind: BoundExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

/// `c >= L AND c < U`: the column's value falls inside the literal's unit.
fn inside(column: &BoundColumn, (lower, upper): (i64, i64)) -> Option<BoundExpr> {
    Some(logical(
        BinaryOp::And,
        compare(column, BinaryOp::GreaterOrEqual, lower)?,
        compare(column, BinaryOp::Less, upper)?,
    ))
}

/// `c < L OR c >= U`: the column's value falls outside the literal's unit.
fn outside(column: &BoundColumn, (lower, upper): (i64, i64)) -> Option<BoundExpr> {
    Some(logical(
        BinaryOp::Or,
        compare(column, BinaryOp::Less, lower)?,
        compare(column, BinaryOp::GreaterOrEqual, upper)?,
    ))
}

fn mirror(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
        other => other,
    }
}

fn rewrite_comparison(expr: &BoundExpr) -> Option<BoundExpr> {
    match &expr.kind {
        BoundExprKind::Binary { op, left, right } => {
            let (subject, literal, op) = match (subject(left), subject(right)) {
                (Some(subject), None) => (subject, right.as_ref(), *op),
                (None, Some(subject)) => (subject, left.as_ref(), mirror(*op)),
                _ => return None,
            };
            let BoundExprKind::Literal(value) = &literal.kind else {
                return None;
            };
            let range @ (lower, upper) = literal_days(subject.unit, value)?;
            let column = subject.column;
            match op {
                BinaryOp::Equal => inside(column, range),
                BinaryOp::NotEqual => outside(column, range),
                BinaryOp::Less => compare(column, BinaryOp::Less, lower),
                BinaryOp::LessOrEqual => compare(column, BinaryOp::Less, upper),
                BinaryOp::Greater => compare(column, BinaryOp::GreaterOrEqual, upper),
                BinaryOp::GreaterOrEqual => compare(column, BinaryOp::GreaterOrEqual, lower),
                _ => None,
            }
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::Between { negated },
            args,
        } => {
            let [target, low, high] = args.as_slice() else {
                return None;
            };
            let subject = subject(target)?;
            let (BoundExprKind::Literal(low), BoundExprKind::Literal(high)) =
                (&low.kind, &high.kind)
            else {
                return None;
            };
            let (lower, _) = literal_days(subject.unit, low)?;
            let (_, upper) = literal_days(subject.unit, high)?;
            if *negated {
                outside(subject.column, (lower, upper))
            } else {
                inside(subject.column, (lower, upper))
            }
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::InList { negated },
            args,
        } => {
            let (target, members) = args.split_first()?;
            let subject = subject(target)?;
            let mut ranges = Vec::with_capacity(members.len());
            for member in members {
                let BoundExprKind::Literal(value) = &member.kind else {
                    return None;
                };
                ranges.push(literal_days(subject.unit, value)?);
            }
            let column = subject.column;
            let mut terms = ranges.into_iter().map(|range| {
                if *negated {
                    outside(column, range)
                } else {
                    inside(column, range)
                }
            });
            let first = terms.next()??;
            let join = if *negated {
                BinaryOp::And
            } else {
                BinaryOp::Or
            };
            terms.try_fold(first, |acc, term| Some(logical(join, acc, term?)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{DatabaseId, TableId};
    use pintail_sql::{
        BinaryOp, BoundColumn, BoundExpr, BoundExprKind, DatePart, ScalarFunction, UnaryOp,
    };
    use pintail_types::{DataType, Value};

    use super::rewrite_predicate;

    fn column(data_type: DataType) -> BoundColumn {
        BoundColumn {
            database_id: DatabaseId::new(1),
            table_id: TableId::new(1),
            column_id: 2,
            relation_name: "t".into(),
            name: "created_at".into(),
            data_type,
            nullable: true,
            collation: None,
            enum_labels: None,
            geometry: false,
            timestamp: false,
            outer: false,
            using_shadowed: false,
        }
    }

    fn column_expr(data_type: DataType) -> BoundExpr {
        BoundExpr {
            data_type: Some(data_type),
            nullable: true,
            kind: BoundExprKind::Column(column(data_type)),
        }
    }

    fn literal(value: Value) -> BoundExpr {
        BoundExpr {
            data_type: value.data_type(),
            nullable: false,
            kind: BoundExprKind::Literal(value),
        }
    }

    fn scalar(function: ScalarFunction, args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr {
            data_type: Some(DataType::Date32),
            nullable: true,
            kind: BoundExprKind::Scalar { function, args },
        }
    }

    /// A boolean-valued scalar call, as `BETWEEN` binds.
    fn scalar_boolean(function: ScalarFunction, args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr {
            data_type: Some(DataType::Boolean),
            nullable: true,
            kind: BoundExprKind::Scalar { function, args },
        }
    }

    fn binary(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
        BoundExpr {
            data_type: Some(DataType::Boolean),
            nullable: true,
            kind: BoundExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
        }
    }

    fn date_of(data_type: DataType) -> BoundExpr {
        scalar(ScalarFunction::Date, vec![column_expr(data_type)])
    }

    /// Renders a predicate in a compact infix form for assertions.
    fn render(expr: &BoundExpr) -> String {
        match &expr.kind {
            BoundExprKind::Column(column) => column.name.clone(),
            BoundExprKind::Literal(Value::Utf8(text)) => format!("'{text}'"),
            BoundExprKind::Literal(Value::Int64(value)) => value.to_string(),
            BoundExprKind::Literal(value) => format!("{value:?}"),
            BoundExprKind::Binary { op, left, right } => {
                format!("({} {op:?} {})", render(left), render(right))
            }
            BoundExprKind::Unary { op, expr } => format!("({op:?} {})", render(expr)),
            BoundExprKind::Scalar { function, args } => format!(
                "{function:?}({})",
                args.iter().map(render).collect::<Vec<_>>().join(", ")
            ),
            other => format!("{other:?}"),
        }
    }

    fn datetime() -> DataType {
        DataType::DateTime64 { fsp: 0 }
    }

    /// A session-zone reading of a TIMESTAMP column: what
    /// `BoundExpr::column` builds when a fixed offset is installed.
    fn reading(zone: &str) -> BoundExpr {
        BoundExpr {
            data_type: Some(datetime()),
            nullable: true,
            kind: BoundExprKind::Scalar {
                function: ScalarFunction::SessionTimestamp,
                args: vec![
                    column_expr(datetime()),
                    literal(Value::Utf8(zone.to_owned())),
                ],
            },
        }
    }

    #[test]
    fn a_fixed_offset_reading_moves_its_shift_onto_the_literal() {
        let moment = || literal(Value::Utf8("2026-08-01 05:30:00".into()));
        assert_eq!(
            render(&rewrite_predicate(binary(
                BinaryOp::GreaterOrEqual,
                reading("+05:30"),
                moment()
            ))),
            "(created_at GreaterOrEqual '2026-08-01 00:00:00')"
        );
        // Written the other way round, the operator turns with it.
        assert_eq!(
            render(&rewrite_predicate(binary(
                BinaryOp::Less,
                moment(),
                reading("-02:00")
            ))),
            "(created_at Greater '2026-08-01 07:30:00')"
        );
        assert_eq!(
            render(&rewrite_predicate(scalar_boolean(
                ScalarFunction::Between { negated: false },
                vec![
                    reading("+05:30"),
                    literal(Value::Utf8("2026-08-01 05:30:00".into())),
                    literal(Value::Utf8("2026-08-02 05:30:00".into())),
                ],
            ))),
            "Between { negated: false }(created_at, '2026-08-01 00:00:00', '2026-08-02 00:00:00')"
        );
    }

    /// A named zone holds two offsets across a daylight-saving change, so
    /// one shift cannot answer for it and the predicate stays as written.
    #[test]
    fn a_named_zone_reading_is_left_alone() {
        let expr = binary(
            BinaryOp::GreaterOrEqual,
            reading("Europe/Paris"),
            literal(Value::Utf8("2026-08-01 02:00:00".into())),
        );
        assert_eq!(render(&rewrite_predicate(expr.clone())), render(&expr));
    }

    #[test]
    fn date_comparisons_become_half_open_ranges_on_the_column() {
        let day = || literal(Value::Utf8("2026-08-01".into()));
        for (op, expected) in [
            (
                BinaryOp::Equal,
                "((created_at GreaterOrEqual '2026-08-01 00:00:00') And (created_at Less '2026-08-02 00:00:00'))",
            ),
            (
                BinaryOp::NotEqual,
                "((created_at Less '2026-08-01 00:00:00') Or (created_at GreaterOrEqual '2026-08-02 00:00:00'))",
            ),
            (BinaryOp::Less, "(created_at Less '2026-08-01 00:00:00')"),
            (
                BinaryOp::LessOrEqual,
                "(created_at Less '2026-08-02 00:00:00')",
            ),
            (
                BinaryOp::Greater,
                "(created_at GreaterOrEqual '2026-08-02 00:00:00')",
            ),
            (
                BinaryOp::GreaterOrEqual,
                "(created_at GreaterOrEqual '2026-08-01 00:00:00')",
            ),
        ] {
            let rewritten = rewrite_predicate(binary(op, date_of(datetime()), day()));
            assert_eq!(render(&rewritten), expected, "{op:?}");
        }
        // A literal on the left mirrors the operator.
        let rewritten = rewrite_predicate(binary(BinaryOp::Less, day(), date_of(datetime())));
        assert_eq!(
            render(&rewritten),
            "(created_at GreaterOrEqual '2026-08-02 00:00:00')"
        );
    }

    #[test]
    fn date_columns_compare_against_plain_dates_and_cast_is_the_same_function() {
        let rewritten = rewrite_predicate(binary(
            BinaryOp::Equal,
            scalar(
                ScalarFunction::Cast(DataType::Date32),
                vec![column_expr(DataType::Date32)],
            ),
            literal(Value::Utf8("2024-02-29".into())),
        ));
        assert_eq!(
            render(&rewritten),
            "((created_at GreaterOrEqual '2024-02-29') And (created_at Less '2024-03-01'))"
        );
    }

    #[test]
    fn between_in_and_their_negations_rewrite_and_nest_under_and_or_not() {
        let between = scalar(
            ScalarFunction::Between { negated: false },
            vec![
                date_of(datetime()),
                literal(Value::Utf8("2026-08-01".into())),
                literal(Value::Utf8("2026-08-07".into())),
            ],
        );
        assert_eq!(
            render(&rewrite_predicate(between.clone())),
            "((created_at GreaterOrEqual '2026-08-01 00:00:00') And (created_at Less '2026-08-08 00:00:00'))"
        );
        let mut not_between = between.clone();
        not_between.kind = match not_between.kind {
            BoundExprKind::Scalar { args, .. } => BoundExprKind::Scalar {
                function: ScalarFunction::Between { negated: true },
                args,
            },
            other => other,
        };
        assert_eq!(
            render(&rewrite_predicate(not_between)),
            "((created_at Less '2026-08-01 00:00:00') Or (created_at GreaterOrEqual '2026-08-08 00:00:00'))"
        );
        let in_list = scalar(
            ScalarFunction::InList { negated: false },
            vec![
                date_of(datetime()),
                literal(Value::Utf8("2026-08-01".into())),
                literal(Value::Utf8("2026-08-03".into())),
            ],
        );
        assert_eq!(
            render(&rewrite_predicate(in_list)),
            "(((created_at GreaterOrEqual '2026-08-01 00:00:00') And (created_at Less '2026-08-02 00:00:00')) Or ((created_at GreaterOrEqual '2026-08-03 00:00:00') And (created_at Less '2026-08-04 00:00:00')))"
        );
        let not_in = scalar(
            ScalarFunction::InList { negated: true },
            vec![
                date_of(datetime()),
                literal(Value::Utf8("2026-08-01".into())),
                literal(Value::Utf8("2026-08-03".into())),
            ],
        );
        assert_eq!(
            render(&rewrite_predicate(not_in)),
            "(((created_at Less '2026-08-01 00:00:00') Or (created_at GreaterOrEqual '2026-08-02 00:00:00')) And ((created_at Less '2026-08-03 00:00:00') Or (created_at GreaterOrEqual '2026-08-04 00:00:00')))"
        );
        let nested = BoundExpr {
            data_type: Some(DataType::Boolean),
            nullable: true,
            kind: BoundExprKind::Unary {
                op: UnaryOp::Not,
                expr: Box::new(binary(
                    BinaryOp::And,
                    binary(
                        BinaryOp::Equal,
                        literal(Value::Int64(1)),
                        literal(Value::Int64(1)),
                    ),
                    between,
                )),
            },
        };
        assert_eq!(
            render(&rewrite_predicate(nested)),
            "(Not ((1 Equal 1) And ((created_at GreaterOrEqual '2026-08-01 00:00:00') And (created_at Less '2026-08-08 00:00:00'))))"
        );
    }

    #[test]
    fn year_uses_calendar_year_bounds() {
        let year = scalar(
            ScalarFunction::DatePart(DatePart::Year),
            vec![column_expr(datetime())],
        );
        let rewritten = rewrite_predicate(binary(
            BinaryOp::Equal,
            year.clone(),
            literal(Value::Int64(2026)),
        ));
        assert_eq!(
            render(&rewritten),
            "((created_at GreaterOrEqual '2026-01-01 00:00:00') And (created_at Less '2027-01-01 00:00:00'))"
        );
        let rewritten =
            rewrite_predicate(binary(BinaryOp::Greater, year, literal(Value::Int64(2024))));
        assert_eq!(
            render(&rewritten),
            "(created_at GreaterOrEqual '2025-01-01 00:00:00')"
        );
    }

    #[test]
    fn shapes_mysql_would_evaluate_differently_are_left_alone() {
        let unchanged = |expr: BoundExpr| {
            let before = render(&expr);
            assert_eq!(render(&rewrite_predicate(expr)), before);
        };
        // A literal at midnight names its day; any other time is not a day.
        let midnight = rewrite_predicate(binary(
            BinaryOp::Equal,
            date_of(datetime()),
            literal(Value::Utf8("2026-08-01 00:00:00".into())),
        ));
        assert_eq!(
            render(&midnight),
            "((created_at GreaterOrEqual '2026-08-01 00:00:00') And (created_at Less '2026-08-02 00:00:00'))"
        );
        unchanged(binary(
            BinaryOp::Equal,
            date_of(datetime()),
            literal(Value::Utf8("2026-08-01 10:00:00".into())),
        ));
        // An impossible date never rewrites.
        unchanged(binary(
            BinaryOp::Equal,
            date_of(datetime()),
            literal(Value::Utf8("2026-02-30".into())),
        ));
        // DATE() over text is a parse, not a projection of a stored date.
        unchanged(binary(
            BinaryOp::Equal,
            date_of(DataType::Utf8),
            literal(Value::Utf8("2026-08-01".into())),
        ));
        // Two columns, no literal.
        unchanged(binary(
            BinaryOp::Equal,
            date_of(datetime()),
            date_of(datetime()),
        ));
        // YEAR against a non-integer, or the last representable year.
        let year = || {
            scalar(
                ScalarFunction::DatePart(DatePart::Year),
                vec![column_expr(datetime())],
            )
        };
        unchanged(binary(
            BinaryOp::Equal,
            year(),
            literal(Value::Utf8("2026".into())),
        ));
        unchanged(binary(BinaryOp::Equal, year(), literal(Value::Int64(9999))));
        // A function nested in arithmetic is not a range predicate.
        unchanged(binary(
            BinaryOp::Equal,
            binary(BinaryOp::Add, date_of(datetime()), literal(Value::Int64(1))),
            literal(Value::Utf8("2026-08-01".into())),
        ));
    }
}
