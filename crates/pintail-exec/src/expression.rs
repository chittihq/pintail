use std::cmp::Ordering;

use chrono::{
    Datelike, Duration, Local, Months, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction, UnaryOp};
use pintail_sql::{DatePart, IntervalUnit};
use pintail_types::{DataType, Value};

use crate::array::ValidityMask;
use crate::batch::TypedValues;
use crate::{ExecError, RecordBatch, SelectionMask};

const fn mirror_comparison(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
        other => other,
    }
}

/// Builds a selection mask from a packed-value comparison. `None` when the
/// literal's physical type doesn't match the column's packed type — mixed-type
/// comparisons keep the row-at-a-time semantics of `evaluate_comparison`.
fn typed_comparison_mask(
    typed: &TypedValues,
    validity: &ValidityMask,
    op: BinaryOp,
    literal: &pintail_types::Value,
) -> Option<SelectionMask> {
    fn fill<T: Copy>(
        values: &[T],
        validity: &ValidityMask,
        keep: impl Fn(T) -> bool,
    ) -> SelectionMask {
        let mut mask = SelectionMask::none(values.len());
        if validity.no_nulls() {
            for (row, &value) in values.iter().enumerate() {
                if keep(value) {
                    mask.set(row, true).expect("row within mask bounds");
                }
            }
        } else {
            for (row, &value) in values.iter().enumerate() {
                if validity.is_valid(row) && keep(value) {
                    mask.set(row, true).expect("row within mask bounds");
                }
            }
        }
        mask
    }
    fn ordered<T: Copy + PartialOrd>(
        values: &[T],
        validity: &ValidityMask,
        op: BinaryOp,
        literal: T,
    ) -> Option<SelectionMask> {
        Some(match op {
            BinaryOp::Equal => fill(values, validity, |v| v == literal),
            BinaryOp::NotEqual => fill(values, validity, |v| v != literal),
            BinaryOp::Less => fill(values, validity, |v| v < literal),
            BinaryOp::LessOrEqual => fill(values, validity, |v| v <= literal),
            BinaryOp::Greater => fill(values, validity, |v| v > literal),
            BinaryOp::GreaterOrEqual => fill(values, validity, |v| v >= literal),
            _ => return None,
        })
    }
    use pintail_types::Value;
    match (typed, literal) {
        (TypedValues::Int64(values), Value::Int64(lit)) => ordered(values, validity, op, *lit),
        (TypedValues::UInt64(values), Value::UInt64(lit)) => ordered(values, validity, op, *lit),
        (TypedValues::Float64(values), Value::Float64(lit)) => {
            ordered(values, validity, op, lit.get())
        }
        // Utf8 deliberately falls back: text comparison is collation-aware
        // (utf8mb4 case-insensitive — see
        // text_key_predicates_do_not_use_bytewise_storage_pruning), so a
        // byte-wise kernel would change semantics. The collation-aware string
        // fast path arrives with dictionary-code execution, where casefolding
        // happens once per distinct value instead of per row.
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CompiledExpr {
    Column(usize),
    Literal(Value),
    Unary {
        op: UnaryOp,
        expr: Box<Self>,
        data_type: Option<DataType>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Self>,
        right: Box<Self>,
        data_type: Option<DataType>,
    },
    IsNull {
        expr: Box<Self>,
        negated: bool,
    },
    Scalar {
        function: ScalarFunction,
        args: Vec<Self>,
        data_type: Option<DataType>,
    },
}

impl CompiledExpr {
    pub(crate) const fn column_index(&self) -> Option<usize> {
        match self {
            Self::Column(index) => Some(*index),
            _ => None,
        }
    }

    pub(crate) fn evaluate_predicate_direct(
        &self,
        batch: &RecordBatch,
        row: usize,
    ) -> Result<Option<bool>, ExecError> {
        match self {
            Self::Binary {
                op: BinaryOp::And,
                left,
                right,
                ..
            } => Ok(left
                .evaluate_predicate_direct(batch, row)?
                .zip(right.evaluate_predicate_direct(batch, row)?)
                .map(|(left, right)| left && right)),
            Self::Binary {
                op: BinaryOp::Or,
                left,
                right,
                ..
            } => Ok(left
                .evaluate_predicate_direct(batch, row)?
                .zip(right.evaluate_predicate_direct(batch, row)?)
                .map(|(left, right)| left || right)),
            Self::Binary {
                op:
                    op @ (BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessOrEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterOrEqual),
                left,
                right,
                ..
            } => {
                let Some((left, right)) = left
                    .direct_value(batch, row)
                    .zip(right.direct_value(batch, row))
                else {
                    return Ok(None);
                };
                Ok(Some(predicate_truth(&evaluate_comparison(
                    *op, left, right,
                )?)?))
            }
            Self::Scalar {
                function: ScalarFunction::Between { negated },
                args,
                ..
            } if args.len() == 3 => {
                let Some((value, lower, upper)) = args[0]
                    .direct_value(batch, row)
                    .zip(args[1].direct_value(batch, row))
                    .zip(args[2].direct_value(batch, row))
                    .map(|((value, lower), upper)| (value, lower, upper))
                else {
                    return Ok(None);
                };
                if matches!(value, Value::Null)
                    || matches!(lower, Value::Null)
                    || matches!(upper, Value::Null)
                {
                    return Ok(Some(false));
                }
                let in_range = predicate_truth(&evaluate_comparison(
                    BinaryOp::GreaterOrEqual,
                    value,
                    lower,
                )?)? && predicate_truth(&evaluate_comparison(
                    BinaryOp::LessOrEqual,
                    value,
                    upper,
                )?)?;
                Ok(Some(if *negated { !in_range } else { in_range }))
            }
            _ => Ok(None),
        }
    }

    fn direct_value<'a>(&'a self, batch: &'a RecordBatch, row: usize) -> Option<&'a Value> {
        match self {
            Self::Column(index) => batch.column(*index)?.value(row),
            Self::Literal(value) => Some(value),
            _ => None,
        }
    }

    /// Batch-level typed evaluation for `column <cmp> literal` predicates
    /// (and AND conjunctions of them): one tight loop over packed values
    /// instead of a per-row `Value` walk. Returns `None` when the shape or
    /// physical types don't qualify — the caller falls back to row-at-a-time.
    pub(crate) fn evaluate_filter_mask(
        &self,
        batch: &RecordBatch,
    ) -> Result<Option<crate::SelectionMask>, ExecError> {
        match self {
            Self::Binary {
                op: BinaryOp::And,
                left,
                right,
                ..
            } => {
                let (Some(mut mask), Some(other)) = (
                    left.evaluate_filter_mask(batch)?,
                    right.evaluate_filter_mask(batch)?,
                ) else {
                    return Ok(None);
                };
                mask.intersect(&other)?;
                Ok(Some(mask))
            }
            Self::Binary {
                op:
                    op @ (BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessOrEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterOrEqual),
                left,
                right,
                ..
            } => {
                let (column, literal, op) = match (left.as_ref(), right.as_ref()) {
                    (Self::Column(index), Self::Literal(value)) => (*index, value, *op),
                    (Self::Literal(value), Self::Column(index)) => {
                        (*index, value, mirror_comparison(*op))
                    }
                    _ => return Ok(None),
                };
                let Some(vector) = batch.column(column) else {
                    return Ok(None);
                };
                let Some((typed, validity)) = vector.typed() else {
                    return Ok(None);
                };
                Ok(typed_comparison_mask(typed, validity, op, literal))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn compile(
        expr: &BoundExpr,
        columns: &[pintail_sql::BoundColumn],
    ) -> Result<Self, ExecError> {
        match &expr.kind {
            BoundExprKind::Column(column) => {
                let index = columns
                    .iter()
                    .position(|candidate| {
                        candidate.database_id == column.database_id
                            && candidate.table_id == column.table_id
                            && candidate.column_id == column.column_id
                    })
                    .ok_or_else(|| ExecError::MissingColumn {
                        relation: column.relation_name.clone(),
                        column: column.name.clone(),
                    })?;
                Ok(Self::Column(index))
            }
            BoundExprKind::GroupKey(index) | BoundExprKind::Aggregate(index) => {
                Ok(Self::Column(*index))
            }
            BoundExprKind::Literal(value) => Ok(Self::Literal(value.clone())),
            BoundExprKind::Unary { op, expr: child } => Ok(Self::Unary {
                op: *op,
                expr: Box::new(Self::compile(child, columns)?),
                data_type: expr.data_type,
            }),
            BoundExprKind::Binary { op, left, right } => Ok(Self::Binary {
                op: *op,
                left: Box::new(Self::compile(left, columns)?),
                right: Box::new(Self::compile(right, columns)?),
                data_type: expr.data_type,
            }),
            BoundExprKind::IsNull {
                expr: child,
                negated,
            } => Ok(Self::IsNull {
                expr: Box::new(Self::compile(child, columns)?),
                negated: *negated,
            }),
            BoundExprKind::Scalar { function, args } => Ok(Self::Scalar {
                function: *function,
                args: args
                    .iter()
                    .map(|argument| Self::compile(argument, columns))
                    .collect::<Result<Vec<_>, _>>()?,
                data_type: expr.data_type,
            }),
            BoundExprKind::ScalarSubquery(_) | BoundExprKind::InSubquery { .. } => {
                Err(ExecError::InvalidPhysicalPlan(
                    "unresolved subquery reached expression compilation",
                ))
            }
        }
    }

    pub(crate) fn evaluate(&self, batch: &RecordBatch, row: usize) -> Result<Value, ExecError> {
        match self {
            Self::Column(index) => batch
                .column(*index)
                .and_then(|column| column.value(row))
                .cloned()
                .ok_or(ExecError::InvalidBatch(
                    "compiled column index is outside the input batch",
                )),
            Self::Literal(value) => Ok(value.clone()),
            Self::Unary {
                op,
                expr,
                data_type,
            } => {
                let value = expr.evaluate(batch, row)?;
                evaluate_unary(*op, &value, *data_type)
            }
            Self::Binary {
                op,
                left,
                right,
                data_type,
            } => {
                let left = left.evaluate(batch, row)?;
                let right = right.evaluate(batch, row)?;
                evaluate_binary(*op, &left, &right, *data_type)
            }
            Self::IsNull { expr, negated } => {
                let is_null = matches!(expr.evaluate(batch, row)?, Value::Null);
                Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
            }
            Self::Scalar {
                function,
                args,
                data_type,
            } => {
                if let ScalarFunction::DatePart(part) = function
                    && let [argument] = args.as_slice()
                    && let Some(value) = argument.direct_value(batch, row)
                    && let Some(value) = evaluate_direct_date_part(value, *part)
                {
                    return value;
                }
                evaluate_scalar(*function, args, *data_type, batch, row)
            }
        }
    }

    pub(crate) fn allocation_upper_bound(&self, batch: &RecordBatch, row: usize) -> usize {
        match self {
            Self::Column(index) => batch
                .column(*index)
                .and_then(|column| column.value(row))
                .map_or(0, Value::heap_bytes),
            Self::Literal(value) => value.heap_bytes(),
            Self::Unary {
                expr, data_type, ..
            } => expr.allocation_upper_bound(batch, row).saturating_add(
                if data_type.is_some_and(|data_type| data_type == DataType::Utf8) {
                    self.string_value_upper_bound(batch, row)
                } else {
                    0
                },
            ),
            Self::Binary {
                left,
                right,
                data_type,
                ..
            } => left
                .allocation_upper_bound(batch, row)
                .saturating_add(right.allocation_upper_bound(batch, row))
                .saturating_add(
                    if data_type.is_some_and(|data_type| data_type == DataType::Utf8) {
                        self.string_value_upper_bound(batch, row)
                    } else {
                        0
                    },
                ),
            Self::IsNull { expr, .. } => expr.allocation_upper_bound(batch, row),
            Self::Scalar {
                function,
                args,
                data_type: _,
            } => {
                let string_arguments = args
                    .iter()
                    .map(|argument| argument.string_value_upper_bound(batch, row))
                    .fold(0_usize, usize::saturating_add);
                let argument_memory = args
                    .iter()
                    .map(|argument| argument.allocation_upper_bound(batch, row))
                    .fold(0_usize, usize::saturating_add)
                    .saturating_add(args.len().saturating_mul(std::mem::size_of::<Value>()))
                    .saturating_add(string_arguments)
                    .saturating_add(args.len().saturating_mul(std::mem::size_of::<String>()));
                let string = |index: usize| {
                    args.get(index)
                        .map_or(0, |argument| argument.string_value_upper_bound(batch, row))
                };
                let first = string(0);
                let output = match function {
                    ScalarFunction::Concat => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add),
                    ScalarFunction::Substring
                    | ScalarFunction::Trim
                    | ScalarFunction::Left
                    | ScalarFunction::Right
                    | ScalarFunction::NullIf
                    | ScalarFunction::Cast(_) => first,
                    ScalarFunction::Lower | ScalarFunction::Upper => first.saturating_mul(12),
                    ScalarFunction::Locate => string_arguments.saturating_mul(12),
                    ScalarFunction::Replace => {
                        first.saturating_add(first.saturating_add(1).saturating_mul(string(2)))
                    }
                    ScalarFunction::If => string(1).max(string(2)),
                    ScalarFunction::Coalesce => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .max()
                        .unwrap_or(0),
                    ScalarFunction::Now
                    | ScalarFunction::CurrentDate
                    | ScalarFunction::Date
                    | ScalarFunction::DateInterval { .. }
                    | ScalarFunction::FromUnixTime => 64,
                    ScalarFunction::DateFormat => string(1).saturating_mul(64),
                    ScalarFunction::Like { .. } => args
                        .iter()
                        .take(2)
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add)
                        .saturating_mul(12),
                    ScalarFunction::Length
                    | ScalarFunction::CharLength
                    | ScalarFunction::InList { .. }
                    | ScalarFunction::Between { .. }
                    | ScalarFunction::DatePart(_)
                    | ScalarFunction::DateDiff
                    | ScalarFunction::UnixTimestamp
                    | ScalarFunction::Round => 0,
                };
                argument_memory.saturating_add(output)
            }
        }
    }

    fn string_value_upper_bound(&self, batch: &RecordBatch, row: usize) -> usize {
        match self {
            Self::Column(index) => batch
                .column(*index)
                .and_then(|column| column.value(row))
                .map_or(0, scalar_string_upper_bound),
            Self::Literal(value) => scalar_string_upper_bound(value),
            Self::Unary { data_type, .. } | Self::Binary { data_type, .. } => {
                if data_type.is_some_and(|data_type| data_type == DataType::Utf8) {
                    64
                } else {
                    24
                }
            }
            Self::IsNull { .. } => 1,
            Self::Scalar { function, args, .. } => {
                let bound = |index: usize| {
                    args.get(index)
                        .map_or(0, |argument| argument.string_value_upper_bound(batch, row))
                };
                let first = bound(0);
                match function {
                    ScalarFunction::Concat => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add),
                    ScalarFunction::Substring
                    | ScalarFunction::Trim
                    | ScalarFunction::Left
                    | ScalarFunction::Right
                    | ScalarFunction::NullIf
                    | ScalarFunction::Cast(_) => first,
                    ScalarFunction::Lower | ScalarFunction::Upper => first.saturating_mul(12),
                    ScalarFunction::Replace => {
                        first.saturating_add(first.saturating_add(1).saturating_mul(bound(2)))
                    }
                    ScalarFunction::If => bound(1).max(bound(2)),
                    ScalarFunction::Coalesce => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .max()
                        .unwrap_or(0),
                    ScalarFunction::Now
                    | ScalarFunction::CurrentDate
                    | ScalarFunction::Date
                    | ScalarFunction::DateInterval { .. }
                    | ScalarFunction::DateFormat
                    | ScalarFunction::FromUnixTime => 64,
                    ScalarFunction::Length
                    | ScalarFunction::CharLength
                    | ScalarFunction::Locate
                    | ScalarFunction::Like { .. }
                    | ScalarFunction::InList { .. }
                    | ScalarFunction::Between { .. }
                    | ScalarFunction::DatePart(_)
                    | ScalarFunction::DateDiff
                    | ScalarFunction::UnixTimestamp
                    | ScalarFunction::Round => 24,
                }
            }
        }
    }
}

fn scalar_string_upper_bound(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int64(_) | Value::UInt64(_) | Value::Float64(_) => 24,
        Value::Utf8(value) => value.len(),
        Value::Binary(value) => value.len(),
    }
}

fn evaluate_direct_date_part(value: &Value, part: DatePart) -> Option<Result<Value, ExecError>> {
    if matches!(value, Value::Null) {
        return Some(Ok(Value::Null));
    }
    let Value::Utf8(value) = value else {
        return None;
    };
    let bytes = value.as_bytes();
    if bytes.len() < 10 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let year = ascii_decimal(bytes.get(0..4)?)?;
    let month = ascii_decimal(bytes.get(5..7)?)?;
    let day = ascii_decimal(bytes.get(8..10)?)?;
    let year = i32::try_from(year).ok()?;
    let month = u32::try_from(month).ok()?;
    let day = u32::try_from(day).ok()?;
    if NaiveDate::from_ymd_opt(year, month, day).is_none() {
        return Some(Err(ExecError::InvalidDateTime));
    }
    let date_value = match part {
        DatePart::Year => u64::try_from(year).unwrap_or(0),
        DatePart::Month => u64::from(month),
        DatePart::Day => u64::from(day),
        DatePart::Hour | DatePart::Minute | DatePart::Second => {
            if bytes.len() < 19
                || !matches!(bytes.get(10), Some(b' ' | b'T'))
                || bytes.get(13) != Some(&b':')
                || bytes.get(16) != Some(&b':')
            {
                return None;
            }
            let hour = ascii_decimal(bytes.get(11..13)?)?;
            let minute = ascii_decimal(bytes.get(14..16)?)?;
            let second = ascii_decimal(bytes.get(17..19)?)?;
            if hour > 23 || minute > 59 || second > 59 {
                return Some(Err(ExecError::InvalidDateTime));
            }
            match part {
                DatePart::Hour => hour,
                DatePart::Minute => minute,
                DatePart::Second => second,
                DatePart::Year | DatePart::Month | DatePart::Day => unreachable!(),
            }
        }
    };
    Some(Ok(Value::UInt64(date_value)))
}

fn ascii_decimal(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + u64::from(digit - b'0'))
    })
}

fn evaluate_scalar(
    function: ScalarFunction,
    args: &[CompiledExpr],
    data_type: Option<DataType>,
    batch: &RecordBatch,
    row: usize,
) -> Result<Value, ExecError> {
    match function {
        ScalarFunction::If => {
            let condition = args[0].evaluate(batch, row)?;
            let branch = if mysql_truth(&condition)?.unwrap_or(false) {
                &args[1]
            } else {
                &args[2]
            };
            let value = branch.evaluate(batch, row)?;
            cast_scalar(&value, data_type)
        }
        ScalarFunction::Coalesce => {
            for argument in args {
                let value = argument.evaluate(batch, row)?;
                if !matches!(value, Value::Null) {
                    return cast_scalar(&value, data_type);
                }
            }
            Ok(Value::Null)
        }
        ScalarFunction::NullIf => {
            let left = args[0].evaluate(batch, row)?;
            let right = args[1].evaluate(batch, row)?;
            if matches!(
                evaluate_comparison(BinaryOp::Equal, &left, &right)?,
                Value::Boolean(true)
            ) {
                Ok(Value::Null)
            } else {
                cast_scalar(&left, data_type)
            }
        }
        _ => {
            let values = args
                .iter()
                .map(|argument| argument.evaluate(batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            evaluate_eager_scalar(function, &values, data_type)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn evaluate_eager_scalar(
    function: ScalarFunction,
    values: &[Value],
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    if values.iter().any(|value| matches!(value, Value::Null))
        && !matches!(
            function,
            ScalarFunction::InList { .. } | ScalarFunction::NullIf
        )
    {
        return Ok(Value::Null);
    }
    match function {
        ScalarFunction::Concat => Ok(Value::Utf8(
            values
                .iter()
                .map(scalar_string)
                .collect::<Result<Vec<_>, _>>()?
                .concat(),
        )),
        ScalarFunction::Substring => {
            let value = scalar_string(&values[0])?;
            let start = mysql_i64(&values[1])?;
            let length = values
                .get(2)
                .map(mysql_i64)
                .transpose()?
                .unwrap_or(i64::MAX);
            Ok(Value::Utf8(mysql_substring(&value, start, length)))
        }
        ScalarFunction::Lower => Ok(Value::Utf8(scalar_string(&values[0])?.to_lowercase())),
        ScalarFunction::Upper => Ok(Value::Utf8(scalar_string(&values[0])?.to_uppercase())),
        ScalarFunction::Trim => Ok(Value::Utf8(scalar_string(&values[0])?.trim().to_owned())),
        ScalarFunction::Length => Ok(Value::UInt64(
            u64::try_from(scalar_string(&values[0])?.len())
                .map_err(|_| ExecError::NumericOverflow)?,
        )),
        ScalarFunction::CharLength => Ok(Value::UInt64(
            u64::try_from(scalar_string(&values[0])?.chars().count())
                .map_err(|_| ExecError::NumericOverflow)?,
        )),
        ScalarFunction::Replace => Ok(Value::Utf8(
            scalar_string(&values[0])?
                .replace(&scalar_string(&values[1])?, &scalar_string(&values[2])?),
        )),
        ScalarFunction::Left => {
            let count = mysql_i64(&values[1])?.max(0);
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            Ok(Value::Utf8(
                scalar_string(&values[0])?.chars().take(count).collect(),
            ))
        }
        ScalarFunction::Right => {
            let value = scalar_string(&values[0])?;
            let count = usize::try_from(mysql_i64(&values[1])?.max(0)).unwrap_or(usize::MAX);
            let skip = value.chars().count().saturating_sub(count);
            Ok(Value::Utf8(value.chars().skip(skip).collect()))
        }
        ScalarFunction::Locate => {
            let needle = scalar_string(&values[0])?.to_lowercase();
            let haystack = scalar_string(&values[1])?;
            let start = values.get(2).map(mysql_i64).transpose()?.unwrap_or(1);
            Ok(Value::UInt64(locate(&needle, &haystack, start)))
        }
        ScalarFunction::Like { negated, escape } => {
            let value = scalar_string(&values[0])?.to_lowercase();
            let pattern = scalar_string(&values[1])?.to_lowercase();
            let matched = like_matches(&value, &pattern, escape);
            Ok(Value::Boolean(if negated { !matched } else { matched }))
        }
        ScalarFunction::InList { negated } => evaluate_in_list(values, negated),
        ScalarFunction::Between { negated } => evaluate_between(values, negated),
        ScalarFunction::Cast(target) => cast_scalar(&values[0], Some(target)),
        ScalarFunction::Round => {
            let value = mysql_f64(&values[0])?;
            let decimals = values.get(1).map(mysql_i64).transpose()?.unwrap_or(0);
            let decimals =
                i32::try_from(decimals.clamp(-308, 308)).map_err(|_| ExecError::NumericOverflow)?;
            let rounded = if decimals >= 0 {
                let factor = 10_f64.powi(decimals);
                let scaled = value * factor;
                if scaled.is_finite() {
                    scaled.round() / factor
                } else {
                    value
                }
            } else {
                let factor = 10_f64.powi(-decimals);
                (value / factor).round() * factor
            };
            if rounded.is_finite() {
                Ok(Value::float64(rounded))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Now => Ok(Value::Utf8(
            Local::now()
                .naive_local()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        )),
        ScalarFunction::CurrentDate => Ok(Value::Utf8(Local::now().format("%Y-%m-%d").to_string())),
        ScalarFunction::Date => Ok(Value::Utf8(
            parse_mysql_datetime(&scalar_string(&values[0])?)?
                .date()
                .format("%Y-%m-%d")
                .to_string(),
        )),
        ScalarFunction::DatePart(part) => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            Ok(Value::UInt64(date_part(value, part)))
        }
        ScalarFunction::DateFormat => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            let format = mysql_date_format(&scalar_string(&values[1])?);
            Ok(Value::Utf8(value.format(&format).to_string()))
        }
        ScalarFunction::DateInterval { unit, subtract } => {
            let input = scalar_string(&values[0])?;
            let value = parse_mysql_datetime(&input)?;
            let amount = mysql_i64(&values[1])?;
            let value = apply_interval(value, amount, unit, subtract)?;
            let date_only = input.len() <= 10
                && matches!(
                    unit,
                    IntervalUnit::Year | IntervalUnit::Month | IntervalUnit::Day
                );
            Ok(Value::Utf8(
                value
                    .format(if date_only {
                        "%Y-%m-%d"
                    } else {
                        "%Y-%m-%d %H:%M:%S"
                    })
                    .to_string(),
            ))
        }
        ScalarFunction::DateDiff => {
            let left = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            let right = parse_mysql_datetime(&scalar_string(&values[1])?)?;
            Ok(Value::Int64(
                left.date().signed_duration_since(right.date()).num_days(),
            ))
        }
        ScalarFunction::UnixTimestamp => {
            let timestamp = if values.is_empty() {
                Utc::now().timestamp()
            } else {
                let value = parse_mysql_datetime(&scalar_string(&values[0])?)?;
                Local
                    .from_local_datetime(&value)
                    .single()
                    .ok_or(ExecError::InvalidDateTime)?
                    .timestamp()
            };
            Ok(Value::UInt64(u64::try_from(timestamp).unwrap_or(0)))
        }
        ScalarFunction::FromUnixTime => {
            let timestamp = mysql_i64(&values[0])?;
            let value = Local
                .timestamp_opt(timestamp, 0)
                .single()
                .ok_or(ExecError::InvalidDateTime)?;
            Ok(Value::Utf8(value.format("%Y-%m-%d %H:%M:%S").to_string()))
        }
        ScalarFunction::If | ScalarFunction::Coalesce | ScalarFunction::NullIf => {
            Err(ExecError::InvalidExpressionType)
        }
    }
    .and_then(|value| cast_scalar(&value, data_type))
}

fn parse_mysql_datetime(value: &str) -> Result<NaiveDateTime, ExecError> {
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(value);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or(ExecError::InvalidDateTime)
}

fn date_part(value: NaiveDateTime, part: DatePart) -> u64 {
    match part {
        DatePart::Year => u64::try_from(value.year()).unwrap_or(0),
        DatePart::Month => u64::from(value.month()),
        DatePart::Day => u64::from(value.day()),
        DatePart::Hour => u64::from(value.hour()),
        DatePart::Minute => u64::from(value.minute()),
        DatePart::Second => u64::from(value.second()),
    }
}

fn apply_interval(
    value: NaiveDateTime,
    amount: i64,
    unit: IntervalUnit,
    subtract: bool,
) -> Result<NaiveDateTime, ExecError> {
    let amount = if subtract {
        amount.checked_neg().ok_or(ExecError::NumericOverflow)?
    } else {
        amount
    };
    match unit {
        IntervalUnit::Year | IntervalUnit::Month => {
            let months = if unit == IntervalUnit::Year {
                amount.checked_mul(12).ok_or(ExecError::NumericOverflow)?
            } else {
                amount
            };
            let magnitude =
                u32::try_from(months.unsigned_abs()).map_err(|_| ExecError::NumericOverflow)?;
            if months < 0 {
                value.checked_sub_months(Months::new(magnitude))
            } else {
                value.checked_add_months(Months::new(magnitude))
            }
        }
        IntervalUnit::Day => value.checked_add_signed(Duration::days(amount)),
        IntervalUnit::Hour => value.checked_add_signed(Duration::hours(amount)),
        IntervalUnit::Minute => value.checked_add_signed(Duration::minutes(amount)),
        IntervalUnit::Second => value.checked_add_signed(Duration::seconds(amount)),
    }
    .ok_or(ExecError::InvalidDateTime)
}

fn mysql_date_format(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            output.push('%');
            break;
        };
        output.push_str(match specifier {
            'c' => "%-m",
            'e' => "%-d",
            'M' => "%B",
            'k' => "%-H",
            'l' => "%-I",
            'i' => "%M",
            's' => "%S",
            'f' => "%6f",
            '%' => "%%",
            other => {
                output.push('%');
                output.push(other);
                continue;
            }
        });
    }
    output
}

fn scalar_string(value: &Value) -> Result<String, ExecError> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Boolean(value) => Ok(if *value { "1" } else { "0" }.to_owned()),
        Value::Int64(value) => Ok(value.to_string()),
        Value::UInt64(value) => Ok(value.to_string()),
        Value::Float64(value) => Ok(value.get().to_string()),
        Value::Utf8(value) => Ok(value.clone()),
        Value::Binary(value) => {
            String::from_utf8(value.clone()).map_err(|_| ExecError::InvalidUtf8Number)
        }
    }
}

fn cast_scalar(value: &Value, data_type: Option<DataType>) -> Result<Value, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    match data_type.map(DataType::storage_type) {
        None => Ok(Value::Null),
        Some(DataType::Boolean) => Ok(mysql_truth(value)?.map_or(Value::Null, Value::Boolean)),
        Some(DataType::Int64) => Ok(Value::Int64(mysql_i64(value)?)),
        Some(DataType::UInt64) => Ok(Value::UInt64(mysql_u64(value)?)),
        Some(DataType::Float64) => Ok(Value::float64(mysql_f64(value)?)),
        Some(DataType::Utf8) => Ok(Value::Utf8(scalar_string(value)?)),
        Some(DataType::Binary) => Ok(Value::Binary(scalar_string(value)?.into_bytes())),
        Some(_) => unreachable!("storage_type returns a physical scalar type"),
    }
}

fn mysql_substring(value: &str, start: i64, length: i64) -> String {
    if start == 0 || length <= 0 {
        return String::new();
    }
    let character_count = value.chars().count();
    let start_character = if start > 0 {
        usize::try_from(start - 1).unwrap_or(usize::MAX)
    } else {
        character_count.saturating_sub(usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX))
    };
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    value.chars().skip(start_character).take(length).collect()
}

fn locate(needle: &str, haystack: &str, start: i64) -> u64 {
    if start <= 0 {
        return 0;
    }
    let start = usize::try_from(start - 1).unwrap_or(usize::MAX);
    let haystack_lower = haystack.to_lowercase();
    let Some(start_byte) = haystack_lower
        .char_indices()
        .nth(start)
        .map(|(index, _)| index)
    else {
        return 0;
    };
    let suffix = &haystack_lower[start_byte..];
    let Some(byte_position) = suffix.find(needle) else {
        return 0;
    };
    let character_position = suffix[..byte_position].chars().count();
    u64::try_from(start.saturating_add(character_position).saturating_add(1)).unwrap_or(u64::MAX)
}

fn like_matches(value: &str, pattern: &str, escape: Option<char>) -> bool {
    let value = value.chars().collect::<Vec<_>>();
    let mut tokens = Vec::with_capacity(pattern.chars().count());
    let mut pattern = pattern.chars();
    while let Some(character) = pattern.next() {
        if Some(character) == escape {
            let Some(literal) = pattern.next() else {
                return false;
            };
            tokens.push(LikeToken::Literal(literal));
        } else {
            tokens.push(match character {
                '%' => LikeToken::AnyMany,
                '_' => LikeToken::AnyOne,
                literal => LikeToken::Literal(literal),
            });
        }
    }

    let mut value_index = 0;
    let mut token_index = 0;
    let mut wildcard = None;
    let mut wildcard_value = 0;
    while value_index < value.len() {
        match tokens.get(token_index) {
            Some(LikeToken::Literal(literal)) if value[value_index] == *literal => {
                value_index += 1;
                token_index += 1;
            }
            Some(LikeToken::AnyOne) => {
                value_index += 1;
                token_index += 1;
            }
            Some(LikeToken::AnyMany) => {
                wildcard = Some(token_index);
                token_index += 1;
                wildcard_value = value_index;
            }
            _ => {
                let Some(wildcard_index) = wildcard else {
                    return false;
                };
                wildcard_value += 1;
                value_index = wildcard_value;
                token_index = wildcard_index + 1;
            }
        }
    }
    while matches!(tokens.get(token_index), Some(LikeToken::AnyMany)) {
        token_index += 1;
    }
    token_index == tokens.len()
}

#[derive(Clone, Copy)]
enum LikeToken {
    Literal(char),
    AnyOne,
    AnyMany,
}

fn evaluate_in_list(values: &[Value], negated: bool) -> Result<Value, ExecError> {
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for candidate in &values[1..] {
        match evaluate_comparison(BinaryOp::Equal, &values[0], candidate)? {
            Value::Boolean(true) => return Ok(Value::Boolean(!negated)),
            Value::Null => saw_null = true,
            Value::Boolean(false) => {}
            _ => return Err(ExecError::InvalidExpressionType),
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Boolean(negated))
    }
}

fn evaluate_between(values: &[Value], negated: bool) -> Result<Value, ExecError> {
    let lower = evaluate_comparison(BinaryOp::GreaterOrEqual, &values[0], &values[1])?;
    let upper = evaluate_comparison(BinaryOp::LessOrEqual, &values[0], &values[2])?;
    let result = evaluate_logic(BinaryOp::And, &lower, &upper)?;
    match result {
        Value::Boolean(value) => Ok(Value::Boolean(if negated { !value } else { value })),
        Value::Null => Ok(Value::Null),
        _ => Err(ExecError::InvalidExpressionType),
    }
}

pub(crate) fn predicate_truth(value: &Value) -> Result<bool, ExecError> {
    Ok(mysql_truth(value)?.unwrap_or(false))
}

pub(crate) fn evaluate_unary(
    op: UnaryOp,
    value: &Value,
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        UnaryOp::Not => Ok(mysql_truth(value)?.map_or(Value::Null, |value| Value::Boolean(!value))),
        UnaryOp::Plus => cast_numeric(value, data_type),
        UnaryOp::Minus => match data_type.map(DataType::storage_type) {
            Some(DataType::Float64) => Ok(Value::float64(-mysql_f64(value)?)),
            Some(DataType::Int64) => mysql_i64(value)?
                .checked_neg()
                .map(Value::Int64)
                .ok_or(ExecError::NumericOverflow),
            Some(DataType::UInt64) => Err(ExecError::NumericOverflow),
            None => Ok(Value::Null),
            Some(DataType::Boolean | DataType::Utf8 | DataType::Binary) => {
                Err(ExecError::InvalidExpressionType)
            }
            Some(_) => unreachable!("storage_type returns a physical scalar type"),
        },
    }
}

pub(crate) fn evaluate_binary(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    match op {
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => evaluate_logic(op, left, right),
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessOrEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterOrEqual => evaluate_comparison(op, left, right),
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::IntegerDivide
        | BinaryOp::Modulo => evaluate_arithmetic(op, left, right, data_type),
    }
}

fn evaluate_logic(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    let left = mysql_truth(left)?;
    let right = mysql_truth(right)?;
    let result = match op {
        BinaryOp::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOp::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        BinaryOp::Xor => left.zip(right).map(|(left, right)| left ^ right),
        _ => return Err(ExecError::InvalidExpressionType),
    };
    Ok(result.map_or(Value::Null, Value::Boolean))
}

fn evaluate_comparison(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    let ordering = compare_mysql(left, right)?;
    let result = match op {
        BinaryOp::Equal => ordering == Ordering::Equal,
        BinaryOp::NotEqual => ordering != Ordering::Equal,
        BinaryOp::Less => ordering == Ordering::Less,
        BinaryOp::LessOrEqual => ordering != Ordering::Greater,
        BinaryOp::Greater => ordering == Ordering::Greater,
        BinaryOp::GreaterOrEqual => ordering != Ordering::Less,
        _ => return Err(ExecError::InvalidExpressionType),
    };
    Ok(Value::Boolean(result))
}

pub(crate) fn compare_mysql(left: &Value, right: &Value) -> Result<Ordering, ExecError> {
    match (left, right) {
        (Value::Utf8(left), Value::Utf8(right)) => Ok(compare_utf8_mysql(left, right)),
        (Value::Binary(left), Value::Binary(right)) => Ok(left.cmp(right)),
        (Value::Boolean(left), Value::Boolean(right)) => Ok(left.cmp(right)),
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::UInt64(left), Value::UInt64(right)) => Ok(left.cmp(right)),
        (Value::Int64(left), Value::UInt64(right)) => {
            if *left < 0 {
                Ok(Ordering::Less)
            } else {
                Ok(u64::try_from(*left)
                    .expect("nonnegative i64 fits u64")
                    .cmp(right))
            }
        }
        (Value::UInt64(left), Value::Int64(right)) => {
            if *right < 0 {
                Ok(Ordering::Greater)
            } else {
                Ok(left.cmp(&u64::try_from(*right).expect("nonnegative i64 fits u64")))
            }
        }
        (Value::Float64(left), Value::Float64(right)) => left
            .get()
            .partial_cmp(&right.get())
            .ok_or(ExecError::InvalidExpressionType),
        _ => mysql_f64(left)?
            .partial_cmp(&mysql_f64(right)?)
            .ok_or(ExecError::InvalidExpressionType),
    }
}

pub(crate) fn compare_utf8_mysql(left: &str, right: &str) -> Ordering {
    if left.is_ascii() && right.is_ascii() {
        return left
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()));
    }
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
}

fn evaluate_arithmetic(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    match data_type.map(DataType::storage_type) {
        Some(DataType::Float64) => {
            let left = mysql_f64(left)?;
            let right = mysql_f64(right)?;
            let value = match op {
                BinaryOp::Add => left + right,
                BinaryOp::Subtract => left - right,
                BinaryOp::Multiply => left * right,
                BinaryOp::Divide => {
                    if right == 0.0 {
                        return Ok(Value::Null);
                    }
                    left / right
                }
                BinaryOp::IntegerDivide => {
                    if right == 0.0 {
                        return Ok(Value::Null);
                    }
                    (left / right).trunc()
                }
                BinaryOp::Modulo => {
                    if right == 0.0 {
                        return Ok(Value::Null);
                    }
                    left % right
                }
                _ => return Err(ExecError::InvalidExpressionType),
            };
            if value.is_finite() {
                Ok(Value::float64(value))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        Some(DataType::UInt64) => {
            let left = mysql_u64(left)?;
            let right = mysql_u64(right)?;
            let result = match op {
                BinaryOp::Add => left.checked_add(right),
                BinaryOp::Subtract => left.checked_sub(right),
                BinaryOp::Multiply => left.checked_mul(right),
                BinaryOp::IntegerDivide if right != 0 => Some(left / right),
                BinaryOp::Modulo if right != 0 => Some(left % right),
                BinaryOp::Divide if right == 0 => return Ok(Value::Null),
                BinaryOp::IntegerDivide | BinaryOp::Modulo if right == 0 => {
                    return Ok(Value::Null);
                }
                _ => None,
            };
            result.map(Value::UInt64).ok_or(ExecError::NumericOverflow)
        }
        Some(DataType::Int64) => {
            let left = mysql_i64(left)?;
            let right = mysql_i64(right)?;
            let result = match op {
                BinaryOp::Add => left.checked_add(right),
                BinaryOp::Subtract => left.checked_sub(right),
                BinaryOp::Multiply => left.checked_mul(right),
                BinaryOp::IntegerDivide if right != 0 => left.checked_div(right),
                BinaryOp::Modulo if right != 0 => left.checked_rem(right),
                BinaryOp::Divide if right == 0 => return Ok(Value::Null),
                BinaryOp::IntegerDivide | BinaryOp::Modulo if right == 0 => {
                    return Ok(Value::Null);
                }
                _ => None,
            };
            result.map(Value::Int64).ok_or(ExecError::NumericOverflow)
        }
        None => Ok(Value::Null),
        Some(DataType::Boolean | DataType::Utf8 | DataType::Binary) => {
            Err(ExecError::InvalidExpressionType)
        }
        Some(_) => unreachable!("storage_type returns a physical scalar type"),
    }
}

fn cast_numeric(value: &Value, data_type: Option<DataType>) -> Result<Value, ExecError> {
    match data_type.map(DataType::storage_type) {
        Some(DataType::Float64) => Ok(Value::float64(mysql_f64(value)?)),
        Some(DataType::Int64) => Ok(Value::Int64(mysql_i64(value)?)),
        Some(DataType::UInt64) => Ok(Value::UInt64(mysql_u64(value)?)),
        None => Ok(Value::Null),
        Some(DataType::Boolean | DataType::Utf8 | DataType::Binary) => {
            Err(ExecError::InvalidExpressionType)
        }
        Some(_) => unreachable!("storage_type returns a physical scalar type"),
    }
}

pub(crate) fn mysql_truth(value: &Value) -> Result<Option<bool>, ExecError> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(value) => Ok(Some(*value)),
        Value::Int64(value) => Ok(Some(*value != 0)),
        Value::UInt64(value) => Ok(Some(*value != 0)),
        Value::Float64(value) => Ok(Some(value.get() != 0.0)),
        Value::Utf8(value) => Ok(Some(parse_mysql_number(value) != 0.0)),
        Value::Binary(value) => {
            let value = std::str::from_utf8(value).map_err(|_| ExecError::InvalidUtf8Number)?;
            Ok(Some(parse_mysql_number(value) != 0.0))
        }
    }
}

pub(crate) fn mysql_f64(value: &Value) -> Result<f64, ExecError> {
    match value {
        Value::Boolean(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::Int64(value) => value
            .to_string()
            .parse()
            .map_err(|_| ExecError::NumericOverflow),
        Value::UInt64(value) => value
            .to_string()
            .parse()
            .map_err(|_| ExecError::NumericOverflow),
        Value::Float64(value) => Ok(value.get()),
        Value::Utf8(value) => Ok(parse_mysql_number(value)),
        Value::Binary(value) => {
            let value = std::str::from_utf8(value).map_err(|_| ExecError::InvalidUtf8Number)?;
            Ok(parse_mysql_number(value))
        }
        Value::Null => Err(ExecError::InvalidExpressionType),
    }
}

pub(crate) fn mysql_i64(value: &Value) -> Result<i64, ExecError> {
    match value {
        Value::Boolean(value) => Ok(i64::from(*value)),
        Value::Int64(value) => Ok(*value),
        Value::UInt64(value) => i64::try_from(*value).map_err(|_| ExecError::NumericOverflow),
        Value::Float64(value) => float_to_i64(value.get()),
        Value::Utf8(value) => float_to_i64(parse_mysql_number(value)),
        Value::Binary(value) => {
            let value = std::str::from_utf8(value).map_err(|_| ExecError::InvalidUtf8Number)?;
            float_to_i64(parse_mysql_number(value))
        }
        Value::Null => Err(ExecError::InvalidExpressionType),
    }
}

pub(crate) fn mysql_u64(value: &Value) -> Result<u64, ExecError> {
    match value {
        Value::Boolean(value) => Ok(u64::from(*value)),
        Value::Int64(value) => u64::try_from(*value).map_err(|_| ExecError::NumericOverflow),
        Value::UInt64(value) => Ok(*value),
        Value::Float64(value) => float_to_u64(value.get()),
        Value::Utf8(value) => float_to_u64(parse_mysql_number(value)),
        Value::Binary(value) => {
            let value = std::str::from_utf8(value).map_err(|_| ExecError::InvalidUtf8Number)?;
            float_to_u64(parse_mysql_number(value))
        }
        Value::Null => Err(ExecError::InvalidExpressionType),
    }
}

fn float_to_i64(value: f64) -> Result<i64, ExecError> {
    if !value.is_finite() {
        return Err(ExecError::NumericOverflow);
    }
    format!("{:.0}", value.trunc())
        .parse()
        .map_err(|_| ExecError::NumericOverflow)
}

fn float_to_u64(value: f64) -> Result<u64, ExecError> {
    if !value.is_finite() || value < 0.0 {
        return Err(ExecError::NumericOverflow);
    }
    format!("{:.0}", value.trunc())
        .parse()
        .map_err(|_| ExecError::NumericOverflow)
}

fn parse_mysql_number(value: &str) -> f64 {
    let value = value.trim_start();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_decimal = false;
    let mut seen_exponent = false;
    let mut exponent_needs_digit = false;

    for (index, character) in value.char_indices() {
        let accepted = match character {
            '+' | '-' if index == 0 || exponent_needs_digit => true,
            '0'..='9' => {
                seen_digit = true;
                exponent_needs_digit = false;
                true
            }
            '.' if !seen_decimal && !seen_exponent => {
                seen_decimal = true;
                true
            }
            'e' | 'E' if seen_digit && !seen_exponent => {
                seen_exponent = true;
                exponent_needs_digit = true;
                true
            }
            _ => false,
        };
        if !accepted {
            break;
        }
        end = index + character.len_utf8();
    }

    if !seen_digit {
        return 0.0;
    }
    if exponent_needs_digit && let Some(exponent) = value[..end].rfind(['e', 'E']) {
        end = exponent;
    }
    value[..end].parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    // Expression behavior is exercised through physical operator tests. Keep
    // the MySQL numeric-prefix parser covered directly because its edge cases
    // do not require a catalog.
    use std::cmp::Ordering;

    use pintail_sql::{BinaryOp, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::{CompiledExpr, compare_mysql, parse_mysql_number};
    use crate::{ColumnVector, RecordBatch};

    #[test]
    fn parses_mysql_numeric_prefixes() {
        assert!((parse_mysql_number("  -12.5xyz") - -12.5).abs() < f64::EPSILON);
        assert!((parse_mysql_number("1.25e2 trailing") - 125.0).abs() < f64::EPSILON);
        assert!(parse_mysql_number("not a number").abs() < f64::EPSILON);
        assert!((parse_mysql_number("1e") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compares_mixed_bigints_without_float_precision_loss() {
        assert_eq!(
            compare_mysql(
                &Value::UInt64(9_007_199_254_740_993),
                &Value::Int64(9_007_199_254_740_992),
            ),
            Ok(Ordering::Greater)
        );
        assert_eq!(
            compare_mysql(&Value::Int64(-1), &Value::UInt64(0)),
            Ok(Ordering::Less)
        );
    }

    #[test]
    fn evaluates_direct_comparison_and_between_predicates() {
        let batch = RecordBatch::new(
            3,
            vec![
                ColumnVector::new(
                    DataType::Utf8,
                    vec![
                        Value::Utf8("2023-06-01".to_owned()),
                        Value::Utf8("2024-01-01".to_owned()),
                        Value::Null,
                    ],
                )
                .expect("date values"),
            ],
        )
        .expect("date batch");
        let comparison = CompiledExpr::Binary {
            op: BinaryOp::Less,
            left: Box::new(CompiledExpr::Column(0)),
            right: Box::new(CompiledExpr::Literal(Value::Utf8("2024-01-01".to_owned()))),
            data_type: Some(DataType::Boolean),
        };
        let between = CompiledExpr::Scalar {
            function: ScalarFunction::Between { negated: false },
            args: vec![
                CompiledExpr::Column(0),
                CompiledExpr::Literal(Value::Utf8("2023-01-01".to_owned())),
                CompiledExpr::Literal(Value::Utf8("2023-12-31".to_owned())),
            ],
            data_type: Some(DataType::Boolean),
        };

        assert_eq!(
            comparison.evaluate_predicate_direct(&batch, 0),
            Ok(Some(true))
        );
        assert_eq!(
            comparison.evaluate_predicate_direct(&batch, 1),
            Ok(Some(false))
        );
        assert_eq!(
            comparison.evaluate_predicate_direct(&batch, 2),
            Ok(Some(false))
        );
        assert_eq!(between.evaluate_predicate_direct(&batch, 0), Ok(Some(true)));
        assert_eq!(
            between.evaluate_predicate_direct(&batch, 1),
            Ok(Some(false))
        );
    }
}
