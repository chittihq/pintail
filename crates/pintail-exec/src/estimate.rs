//! Row estimates from the per-column statistics a table's storage keeps.
//!
//! Every figure here is a guess that orders joins or picks which side of a
//! hash join is read first. None of them removes a row or a check, so a
//! wrong guess costs time and never an answer. Where a relation carries no
//! statistics these functions answer `None` and the caller keeps its fixed
//! guesses.

use pintail_catalog::{ColumnFacts, ColumnStatistics, RangeDomain};
use pintail_sql::{BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundJoinKind, ScalarFunction};
use pintail_types::Value;

use crate::{LogicalPlan, Scan};

/// Share of rows a range over a column without a usable span keeps.
const UNKNOWN_RANGE: f64 = 1.0 / 3.0;

/// A scan's rows once its own predicates apply, from its table's column
/// statistics. `None` when the table has none.
#[must_use]
pub(crate) fn scan_rows(scan: &Scan) -> Option<u64> {
    let statistics = scan.table.column_statistics.as_ref()?.get();
    let rows = scan.table.estimated_rows.or(scan.table.row_count)?;
    let selectivity = scan
        .predicates
        .iter()
        .map(|predicate| {
            predicate_selectivity(predicate, &|column| facts(scan, statistics, column))
        })
        .product::<f64>();
    let estimate = scaled(rows, selectivity);
    Some(scan.limit.map_or(estimate, |limit| estimate.min(limit)))
}

/// Estimated distinct values of a scan's column, when its statistics hold
/// a sketch for it. A whole single-column key is as distinct as the rows.
#[must_use]
pub(crate) fn scan_distinct(scan: &Scan, column: &BoundColumn) -> Option<u64> {
    if !belongs(scan, column) {
        return None;
    }
    if scan.table.key_column_ids.as_slice() == [column.column_id] {
        return scan.table.estimated_rows.or(scan.table.row_count);
    }
    let statistics = scan.table.column_statistics.as_ref()?.get();
    statistics.column(column.column_id)?.distinct
}

/// A plan's expected output rows, from column statistics, where every
/// relation it reads carries them and its shape is one these estimates
/// understand. Unlike [`LogicalPlan::estimated_rows`], which is an upper
/// bound for guards, this is a best guess and may be far below the truth.
#[must_use]
pub(crate) fn expected_rows(plan: &LogicalPlan) -> Option<u64> {
    match plan {
        LogicalPlan::Empty => Some(0),
        LogicalPlan::OneRow => Some(1),
        LogicalPlan::Scan(scan) => scan_rows(scan),
        LogicalPlan::Filter { input, predicate } => {
            let rows = expected_rows(input)?;
            let selectivity = conjuncts(predicate)
                .into_iter()
                .map(|conjunct| {
                    predicate_selectivity(conjunct, &|column| plan_facts(input, column))
                })
                .product::<f64>();
            Some(scaled(rows, selectivity))
        }
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Derived { input, .. }
        | LogicalPlan::JsonTable { input, .. } => expected_rows(input),
        LogicalPlan::Limit { input, limit } => Some(
            expected_rows(input)?
                .saturating_sub(limit.offset)
                .min(limit.count),
        ),
        LogicalPlan::Join {
            left,
            right,
            kind,
            condition,
        } => {
            let left_rows = expected_rows(left)?;
            match kind {
                BoundJoinKind::Semi | BoundJoinKind::Anti | BoundJoinKind::Scalar => {
                    Some(left_rows)
                }
                BoundJoinKind::Inner | BoundJoinKind::Left => {
                    let right_rows = expected_rows(right)?;
                    let joined =
                        joined_rows(left, left_rows, right, right_rows, condition.as_ref()?)?;
                    Some(if *kind == BoundJoinKind::Left {
                        joined.max(left_rows)
                    } else {
                        joined
                    })
                }
                BoundJoinKind::Cross => left_rows.checked_mul(expected_rows(right)?),
            }
        }
        _ => None,
    }
}

/// Rows an equality join keeps: the product of the two sides divided by
/// the larger distinct count of the most selective key pair, the usual
/// assumption that the smaller side's key values all appear in the larger
/// side. `None` without an equality whose two columns both have counts.
fn joined_rows(
    left: &LogicalPlan,
    left_rows: u64,
    right: &LogicalPlan,
    right_rows: u64,
    condition: &BoundExpr,
) -> Option<u64> {
    let divisor = conjuncts(condition)
        .into_iter()
        .filter_map(|conjunct| {
            let BoundExprKind::Binary {
                op: BinaryOp::Equal,
                left: first,
                right: second,
            } = &conjunct.kind
            else {
                return None;
            };
            let (BoundExprKind::Column(first), BoundExprKind::Column(second)) =
                (&first.kind, &second.kind)
            else {
                return None;
            };
            let (left_column, right_column) =
                if plan_distinct(left, first).is_some() && plan_distinct(right, second).is_some() {
                    (first, second)
                } else {
                    (second, first)
                };
            let left_distinct = plan_distinct(left, left_column)?.min(left_rows.max(1));
            let right_distinct = plan_distinct(right, right_column)?.min(right_rows.max(1));
            Some(left_distinct.max(right_distinct).max(1))
        })
        .max()?;
    Some(
        (u128::from(left_rows) * u128::from(right_rows) / u128::from(divisor))
            .try_into()
            .unwrap_or(u64::MAX),
    )
}

/// Distinct values of a column read somewhere beneath `plan`.
fn plan_distinct(plan: &LogicalPlan, column: &BoundColumn) -> Option<u64> {
    match plan {
        LogicalPlan::Scan(scan) => scan_distinct(scan, column),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => plan_distinct(input, column),
        LogicalPlan::Join { left, right, .. } => {
            plan_distinct(left, column).or_else(|| plan_distinct(right, column))
        }
        _ => None,
    }
}

/// A column's facts and its table's rows, from the scan beneath `plan`
/// that reads it.
fn plan_facts(plan: &LogicalPlan, column: &BoundColumn) -> Option<(ColumnFacts, u64)> {
    match plan {
        LogicalPlan::Scan(scan) => {
            facts(scan, scan.table.column_statistics.as_ref()?.get(), column)
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => plan_facts(input, column),
        LogicalPlan::Join { left, right, .. } => {
            plan_facts(left, column).or_else(|| plan_facts(right, column))
        }
        _ => None,
    }
}

fn belongs(scan: &Scan, column: &BoundColumn) -> bool {
    !column.outer
        && column.database_id == scan.table.database_id
        && column.table_id == scan.table.table_id
        && column.relation_name == scan.table.relation_name
}

fn facts(
    scan: &Scan,
    statistics: &ColumnStatistics,
    column: &BoundColumn,
) -> Option<(ColumnFacts, u64)> {
    if !belongs(scan, column) {
        return None;
    }
    let mut facts = *statistics.column(column.column_id)?;
    if scan.table.key_column_ids.as_slice() == [column.column_id] {
        facts.distinct = Some(facts.non_null.max(statistics.rows));
    }
    Some((facts, statistics.rows))
}

/// Share of rows one conjunct keeps, from the statistics of the column it
/// tests. Shapes these estimates do not model keep every row.
fn predicate_selectivity(
    predicate: &BoundExpr,
    lookup: &dyn Fn(&BoundColumn) -> Option<(ColumnFacts, u64)>,
) -> f64 {
    let column_of = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::Column(column) if !column.outer => lookup(column),
        _ => None,
    };
    let literal = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::Literal(value) if !matches!(value, Value::Null) => Some(value.clone()),
        _ => None,
    };
    let present = |facts: &ColumnFacts, rows: u64| {
        if rows == 0 {
            1.0
        } else {
            ratio(facts.non_null, rows)
        }
    };
    let equal = |facts: &ColumnFacts, rows: u64, listed: u64| {
        let distinct = facts.distinct.filter(|distinct| *distinct > 0);
        let share = distinct.map_or(0.1, |distinct| ratio(listed, distinct).min(1.0));
        present(facts, rows) * share
    };
    match &predicate.kind {
        BoundExprKind::Binary { op, left, right } => {
            let (facts, value, op) = match (column_of(left.as_ref()), literal(right.as_ref())) {
                (Some(facts), Some(value)) => (facts, value, *op),
                _ => match (literal(left.as_ref()), column_of(right.as_ref())) {
                    (Some(value), Some(facts)) => (facts, value, flipped(*op)),
                    _ => return 1.0,
                },
            };
            let (facts, rows) = facts;
            match op {
                BinaryOp::Equal => equal(&facts, rows, 1),
                BinaryOp::NotEqual => present(&facts, rows) - equal(&facts, rows, 1),
                BinaryOp::Less
                | BinaryOp::LessOrEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterOrEqual => {
                    present(&facts, rows) * range_share(&facts, op, &value).unwrap_or(UNKNOWN_RANGE)
                }
                _ => 1.0,
            }
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::InList { negated: false },
            args,
        } if args.len() > 1 && args[1..].iter().all(|arg| literal(arg).is_some()) => {
            column_of(&args[0]).map_or(1.0, |(facts, rows)| {
                equal(
                    &facts,
                    rows,
                    u64::try_from(args.len() - 1).unwrap_or(u64::MAX),
                )
            })
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::Between { negated: false },
            args,
        } if args.len() == 3 => {
            let (Some((facts, rows)), Some(low), Some(high)) =
                (column_of(&args[0]), literal(&args[1]), literal(&args[2]))
            else {
                return 1.0;
            };
            let share = match (
                range_share(&facts, BinaryOp::GreaterOrEqual, &low),
                range_share(&facts, BinaryOp::LessOrEqual, &high),
            ) {
                (Some(above), Some(below)) => (above + below - 1.0).max(0.0),
                _ => UNKNOWN_RANGE,
            };
            present(&facts, rows) * share
        }
        BoundExprKind::IsNull { expr, negated } => {
            column_of(expr.as_ref()).map_or(1.0, |(facts, rows)| {
                if *negated {
                    present(&facts, rows)
                } else {
                    1.0 - present(&facts, rows)
                }
            })
        }
        _ => 1.0,
    }
}

/// Share of a column's non-NULL values `column op value` keeps, assuming
/// values spread evenly over the column's span.
#[allow(clippy::cast_precision_loss)]
fn range_share(facts: &ColumnFacts, op: BinaryOp, value: &Value) -> Option<f64> {
    let range = facts.range?;
    let point = domain_value(range.domain, value)?;
    let span = (range.high - range.low + 1).max(1) as f64;
    let below = match op {
        BinaryOp::Less => point - range.low,
        BinaryOp::LessOrEqual => point - range.low + 1,
        BinaryOp::Greater => range.high - point,
        BinaryOp::GreaterOrEqual => range.high - point + 1,
        _ => return None,
    };
    Some((below as f64 / span).clamp(0.0, 1.0))
}

/// A literal in a range's unit.
fn domain_value(domain: RangeDomain, value: &Value) -> Option<i128> {
    const MICROS_PER_DAY: i128 = 86_400_000_000;
    match (domain, value) {
        (RangeDomain::Int | RangeDomain::UInt, Value::Int64(value)) => Some(i128::from(*value)),
        (RangeDomain::Int | RangeDomain::UInt, Value::UInt64(value)) => Some(i128::from(*value)),
        (RangeDomain::Decimal { scale }, Value::Int64(value)) => {
            i128::from(*value).checked_mul(10_i128.checked_pow(u32::from(scale))?)
        }
        (RangeDomain::Decimal { scale }, Value::UInt64(value)) => {
            i128::from(*value).checked_mul(10_i128.checked_pow(u32::from(scale))?)
        }
        (RangeDomain::Decimal { scale }, Value::Utf8(text)) => {
            pintail_types::parse_decimal_scaled(text, scale)
        }
        (RangeDomain::Date, Value::Utf8(text)) => {
            pintail_types::parse_date_days(text).map(i128::from)
        }
        (RangeDomain::DateTime, Value::Utf8(text)) => pintail_types::parse_datetime_micros(text)
            .map(i128::from)
            .or_else(|| {
                pintail_types::parse_date_days(text).map(|days| i128::from(days) * MICROS_PER_DAY)
            }),
        _ => None,
    }
}

const fn flipped(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
        other => other,
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        1.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0)
    }
}

/// `rows` times a share, never below one row for a non-empty input.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn scaled(rows: u64, share: f64) -> u64 {
    if rows == 0 {
        return 0;
    }
    ((rows as f64 * share.clamp(0.0, 1.0)).round() as u64).clamp(1, rows)
}

fn conjuncts(predicate: &BoundExpr) -> Vec<&BoundExpr> {
    let mut found = Vec::new();
    let mut pending = vec![predicate];
    while let Some(expr) = pending.pop() {
        if let BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } = &expr.kind
        {
            pending.push(left);
            pending.push(right);
        } else {
            found.push(expr);
        }
    }
    found
}
