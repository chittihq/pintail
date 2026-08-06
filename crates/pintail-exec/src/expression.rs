use std::cmp::Ordering;

use chrono::{
    Datelike, Duration, FixedOffset, Local, LocalResult, Months, NaiveDate, NaiveDateTime,
    TimeZone, Timelike, Utc,
};
use md5::{Digest as _, Md5};
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
#[allow(clippy::too_many_lines)]
fn typed_comparison_mask(
    typed: &TypedValues,
    validity: &ValidityMask,
    logical_type: DataType,
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
        // Packed temporal units compare as integers when the literal is
        // canonical AND (for DateTime64) carries exactly the column's fsp
        // fraction digits — otherwise byte semantics could diverge from unit
        // semantics, so anything else falls back to the row path.
        (TypedValues::Temporal { units, .. }, Value::Utf8(text)) => {
            let literal = match logical_type {
                DataType::Date32 => crate::batch::parse_date_days(text),
                DataType::DateTime64 { fsp } => {
                    let expected_len = if fsp == 0 { 19 } else { 20 + usize::from(fsp) };
                    (text.len() == expected_len)
                        .then(|| crate::batch::parse_datetime_micros(text))
                        .flatten()
                }
                _ => None,
            }?;
            ordered(units, validity, op, literal)
        }
        // Temporal types ride the Utf8 carrier in canonical fixed-width form,
        // where byte order IS chronological order and collation cannot apply
        // (digits, dashes, colons only) — byte-wise kernels are exact.
        (TypedValues::Utf8(column), Value::Utf8(text))
            if matches!(
                logical_type,
                DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. }
            ) =>
        {
            let needle = text.as_bytes();
            let views = column.views();
            let heap = column.heap();
            let mut mask = SelectionMask::none(views.len());
            for (row, view) in views.iter().enumerate() {
                if validity.is_valid(row) {
                    let keep = view.with_bytes(heap, |bytes| match op {
                        BinaryOp::Equal => bytes == needle,
                        BinaryOp::NotEqual => bytes != needle,
                        BinaryOp::Less => bytes < needle,
                        BinaryOp::LessOrEqual => bytes <= needle,
                        BinaryOp::Greater => bytes > needle,
                        _ => bytes >= needle,
                    });
                    if keep {
                        mask.set(row, true).expect("row within mask bounds");
                    }
                }
            }
            Some(mask)
        }
        // Collation-correct string equality over low-cardinality columns:
        // dedup the batch's distinct views byte-exactly, casefold each
        // distinct ONCE against the needle (the same compare_utf8_mysql the
        // row path uses), then match rows by 16-byte view comparison against
        // the matching distincts. Ordering comparisons and high-cardinality
        // batches fall back to the row path.
        (TypedValues::Utf8(column), Value::Utf8(text))
            if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
        {
            // Dictionary fast path: casefold each distinct value once,
            // then one code lookup per row (the Q2 profile spent ~25% of
            // the query in per-row view comparisons here).
            if let Some((codes, values)) = column.dictionary() {
                let matching: Vec<bool> = values
                    .iter()
                    .map(|value| compare_utf8_mysql(value, text) == Ordering::Equal)
                    .collect();
                let want = op == BinaryOp::Equal;
                let mut mask = SelectionMask::none(codes.len());
                for (row, code) in codes.iter().enumerate() {
                    if validity.is_valid(row)
                        && matching[usize::try_from(*code).expect("dict code fits usize")] == want
                    {
                        mask.set(row, true).expect("row within mask bounds");
                    }
                }
                return Some(mask);
            }
            #[allow(clippy::items_after_statements)]
            const MAX_DISTINCT: usize = 16;
            let views = column.views();
            let heap = column.heap();
            let mut distinct: Vec<crate::array::StrView> = Vec::new();
            for (row, view) in views.iter().enumerate() {
                if !validity.is_valid(row) {
                    continue;
                }
                if !distinct.iter().any(|seen| seen.same_bytes(view, heap)) {
                    if distinct.len() >= MAX_DISTINCT {
                        return None;
                    }
                    distinct.push(*view);
                }
            }
            let matching: Vec<crate::array::StrView> = distinct
                .iter()
                .filter(|view| {
                    view.with_bytes(heap, |bytes| {
                        std::str::from_utf8(bytes).is_ok_and(|candidate| {
                            compare_utf8_mysql(candidate, text) == Ordering::Equal
                        })
                    })
                })
                .copied()
                .collect();
            let want = op == BinaryOp::Equal;
            let mut mask = SelectionMask::none(views.len());
            for (row, view) in views.iter().enumerate() {
                if validity.is_valid(row) {
                    let hit = matching.iter().any(|m| m.same_bytes(view, heap));
                    if hit == want {
                        mask.set(row, true).expect("row within mask bounds");
                    }
                }
            }
            Some(mask)
        }
        // Remaining Utf8 shapes (ordering, high cardinality) fall back to the
        // collation-aware row path.
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
        argument_types: Vec<Option<DataType>>,
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
                Ok(typed_comparison_mask(
                    typed,
                    validity,
                    vector.data_type(),
                    op,
                    literal,
                ))
            }
            Self::Scalar {
                function: ScalarFunction::Between { negated: false },
                args,
                ..
            } if args.len() == 3 => {
                let (Self::Column(column), Self::Literal(lower), Self::Literal(upper)) =
                    (&args[0], &args[1], &args[2])
                else {
                    return Ok(None);
                };
                let Some(vector) = batch.column(*column) else {
                    return Ok(None);
                };
                let Some((typed, validity)) = vector.typed() else {
                    return Ok(None);
                };
                let (Some(mut mask), Some(other)) = (
                    typed_comparison_mask(
                        typed,
                        validity,
                        vector.data_type(),
                        BinaryOp::GreaterOrEqual,
                        lower,
                    ),
                    typed_comparison_mask(
                        typed,
                        validity,
                        vector.data_type(),
                        BinaryOp::LessOrEqual,
                        upper,
                    ),
                ) else {
                    return Ok(None);
                };
                mask.intersect(&other)?;
                Ok(Some(mask))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn compile(
        expr: &BoundExpr,
        columns: &[pintail_sql::BoundColumn],
    ) -> Result<Self, ExecError> {
        match &expr.kind {
            BoundExprKind::Window(_) => Err(ExecError::InvalidPhysicalPlan(
                "window expressions must be lowered before compilation",
            )),
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
                argument_types: args.iter().map(|argument| argument.data_type).collect(),
                args: args
                    .iter()
                    .map(|argument| Self::compile(argument, columns))
                    .collect::<Result<Vec<_>, _>>()?,
                data_type: expr.data_type,
            }),
            BoundExprKind::ScalarSubquery(_)
            | BoundExprKind::InSubquery { .. }
            | BoundExprKind::ExistsSubquery { .. } => Err(ExecError::InvalidPhysicalPlan(
                "unresolved subquery reached expression compilation",
            )),
        }
    }

    /// A stable textual identity for this expression, or `None` when it
    /// could evaluate differently on identical data (volatile functions).
    /// Column identity is projection-relative; the settled aggregate memo
    /// combines this with the scan's projected column ids.
    pub(crate) fn deterministic_signature(&self) -> Option<String> {
        match self {
            Self::Column(index) => Some(format!("c{index}")),
            Self::Literal(value) => Some(format!("l{value:?}")),
            Self::Unary {
                op,
                expr,
                data_type,
            } => Some(format!(
                "u{op:?}({}){data_type:?}",
                expr.deterministic_signature()?
            )),
            Self::Binary {
                op,
                left,
                right,
                data_type,
            } => Some(format!(
                "b{op:?}({},{}){data_type:?}",
                left.deterministic_signature()?,
                right.deterministic_signature()?
            )),
            Self::IsNull { expr, negated } => {
                Some(format!("n{negated}({})", expr.deterministic_signature()?))
            }
            Self::Scalar {
                function,
                args,
                argument_types,
                data_type,
            } => {
                if matches!(
                    function,
                    ScalarFunction::Now
                        | ScalarFunction::UnixTimestamp
                        | ScalarFunction::Curtime
                        | ScalarFunction::Rand
                ) {
                    return None;
                }
                let mut inner = String::new();
                for arg in args {
                    inner.push_str(&arg.deterministic_signature()?);
                    inner.push(',');
                }
                Some(format!(
                    "s{function:?}({inner}){argument_types:?}{data_type:?}"
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
                argument_types,
                data_type,
            } => {
                if let ScalarFunction::DatePart(part) = function
                    && let [argument] = args.as_slice()
                {
                    // Packed temporal units first: extracting from i64
                    // units never touches the column's lazy text (the
                    // 2026-08-02 phase-0 profile put ~half of Q5 in
                    // formatting 20M dates only to reparse them here).
                    if let Self::Column(index) = argument
                        && let Some(value) = evaluate_units_date_part(batch, *index, row, *part)
                    {
                        return value;
                    }
                    if let Some(value) = argument.direct_value(batch, row)
                        && let Some(value) = evaluate_direct_date_part(value, *part)
                    {
                        return value;
                    }
                }
                evaluate_scalar(*function, args, argument_types, *data_type, batch, row)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
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
                argument_types: _,
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
                    ScalarFunction::Concat | ScalarFunction::ConcatWs => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add),
                    // Member text plus quotes/separators/braces per argument.
                    ScalarFunction::JsonObject | ScalarFunction::JsonArray => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add)
                        .saturating_add(args.len().saturating_mul(8).saturating_add(2)),
                    ScalarFunction::Substring
                    | ScalarFunction::Trim
                    | ScalarFunction::Left
                    | ScalarFunction::Right
                    | ScalarFunction::NullIf
                    | ScalarFunction::Reverse
                    | ScalarFunction::Unhex
                    | ScalarFunction::FromBase64
                    | ScalarFunction::RegexpSubstr
                    | ScalarFunction::JsonUnquote
                    // JSON_KEYS is bounded by the document it reads.
                    | ScalarFunction::JsonKeys
                    // JSON_SEARCH renders paths, JSON_VALUE a member: both
                    // are bounded by the document they read.
                    | ScalarFunction::JsonSearch
                    | ScalarFunction::JsonValue
                    | ScalarFunction::SubstringIndex
                    | ScalarFunction::Cast(_) => first,
                    ScalarFunction::JsonExtract { .. } => first
                        .saturating_mul(args.len().saturating_sub(1))
                        .saturating_add(args.len().saturating_mul(2)),
                    // JSON_TYPE returns one of a handful of fixed names.
                    ScalarFunction::JsonType => 16,
                    ScalarFunction::Lower | ScalarFunction::Upper => first.saturating_mul(12),
                    ScalarFunction::Locate => string_arguments.saturating_mul(12),
                    ScalarFunction::Replace | ScalarFunction::RegexpReplace => {
                        first.saturating_add(first.saturating_add(1).saturating_mul(string(2)))
                    }
                    ScalarFunction::If => string(1).max(string(2)),
                    ScalarFunction::Coalesce | ScalarFunction::Elt => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .max()
                        .unwrap_or(0),
                    ScalarFunction::Now
                    | ScalarFunction::CurrentDate
                    | ScalarFunction::Date
                    | ScalarFunction::DateInterval { .. }
                    | ScalarFunction::FromUnixTime
                    | ScalarFunction::Abs { .. }
                    | ScalarFunction::Greatest { .. }
                    | ScalarFunction::Least { .. }
                    | ScalarFunction::Format
                    | ScalarFunction::DayName
                    | ScalarFunction::MonthName
                    | ScalarFunction::LastDay
                    | ScalarFunction::FromDays
                    | ScalarFunction::SecToTime
                    | ScalarFunction::MakeDate
                    | ScalarFunction::Curtime
                    | ScalarFunction::StrToDate
                    | ScalarFunction::ConvertTz
                    | ScalarFunction::Char
                    | ScalarFunction::Rand
                    // CONV and MAKETIME render short fixed-width strings.
                    | ScalarFunction::Conv
                    | ScalarFunction::MakeTime => 64,
                    ScalarFunction::DateFormat => string(1).saturating_mul(64),
                    ScalarFunction::Like { .. } => args
                        .iter()
                        .take(2)
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add)
                        .saturating_mul(12),
                    ScalarFunction::Length
                    | ScalarFunction::CharLength
                    | ScalarFunction::DecimalComparison { .. }
                    | ScalarFunction::InList { .. }
                    | ScalarFunction::Between { .. }
                    | ScalarFunction::DatePart(_)
                    | ScalarFunction::DateDiff
                    | ScalarFunction::UnixTimestamp
                    | ScalarFunction::Round { .. }
                    | ScalarFunction::Ceil { .. }
                    | ScalarFunction::Floor { .. }
                    | ScalarFunction::Sign
                    | ScalarFunction::Power
                    | ScalarFunction::Sqrt
                    | ScalarFunction::Exp
                    | ScalarFunction::Ln
                    | ScalarFunction::LogBase
                    | ScalarFunction::Log2
                    | ScalarFunction::Log10
                    | ScalarFunction::Truncate { .. }
                    | ScalarFunction::Instr
                    | ScalarFunction::FindInSet
                    | ScalarFunction::Ascii
                    | ScalarFunction::Ord
                    | ScalarFunction::Field
                    | ScalarFunction::ToDays
                    | ScalarFunction::YearWeek
                    | ScalarFunction::TimeToSec
                    | ScalarFunction::RegexpLike { .. }
                    | ScalarFunction::RegexpInstr
                    | ScalarFunction::TimestampDiff { .. }
                    // The JSON predicates answer numerically and retain
                    // nothing on the heap.
                    | ScalarFunction::JsonValid
                    | ScalarFunction::JsonLength
                    | ScalarFunction::JsonContains
                    | ScalarFunction::JsonContainsPath => 0,
                    ScalarFunction::Repeat
                    | ScalarFunction::Space
                    | ScalarFunction::Lpad
                    | ScalarFunction::Rpad => STRING_BUILD_CAP,
                    ScalarFunction::Md5 => 32,
                    ScalarFunction::Hex | ScalarFunction::ToBase64 => {
                        first.saturating_mul(2).saturating_add(24)
                    }
                };
                argument_memory.saturating_add(output)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // a flat per-function bound table reads best unsplit
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
                    ScalarFunction::Concat | ScalarFunction::ConcatWs => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add),
                    // Member text plus quotes/separators/braces per argument.
                    ScalarFunction::JsonObject | ScalarFunction::JsonArray => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .fold(0_usize, usize::saturating_add)
                        .saturating_add(args.len().saturating_mul(8).saturating_add(2)),
                    ScalarFunction::Substring
                    | ScalarFunction::Trim
                    | ScalarFunction::Left
                    | ScalarFunction::Right
                    | ScalarFunction::NullIf
                    | ScalarFunction::Reverse
                    | ScalarFunction::Unhex
                    | ScalarFunction::FromBase64
                    | ScalarFunction::RegexpSubstr
                    | ScalarFunction::JsonUnquote
                    // JSON_KEYS is bounded by the document it reads.
                    | ScalarFunction::JsonKeys
                    // JSON_SEARCH renders paths, JSON_VALUE a member: both
                    // are bounded by the document they read.
                    | ScalarFunction::JsonSearch
                    | ScalarFunction::JsonValue
                    | ScalarFunction::SubstringIndex
                    | ScalarFunction::Cast(_) => first,
                    ScalarFunction::JsonExtract { .. } => first
                        .saturating_mul(args.len().saturating_sub(1))
                        .saturating_add(args.len().saturating_mul(2)),
                    // JSON_TYPE returns one of a handful of fixed names.
                    ScalarFunction::JsonType => 16,
                    ScalarFunction::Lower | ScalarFunction::Upper => first.saturating_mul(12),
                    ScalarFunction::Replace | ScalarFunction::RegexpReplace => {
                        first.saturating_add(first.saturating_add(1).saturating_mul(bound(2)))
                    }
                    ScalarFunction::If => bound(1).max(bound(2)),
                    ScalarFunction::Coalesce | ScalarFunction::Elt => args
                        .iter()
                        .map(|argument| argument.string_value_upper_bound(batch, row))
                        .max()
                        .unwrap_or(0),
                    ScalarFunction::Now
                    | ScalarFunction::CurrentDate
                    | ScalarFunction::Date
                    | ScalarFunction::DateInterval { .. }
                    | ScalarFunction::DateFormat
                    | ScalarFunction::FromUnixTime
                    | ScalarFunction::Abs { .. }
                    | ScalarFunction::Greatest { .. }
                    | ScalarFunction::Least { .. }
                    | ScalarFunction::Format
                    | ScalarFunction::DayName
                    | ScalarFunction::MonthName
                    | ScalarFunction::LastDay
                    | ScalarFunction::FromDays
                    | ScalarFunction::SecToTime
                    | ScalarFunction::MakeDate
                    | ScalarFunction::Curtime
                    | ScalarFunction::StrToDate
                    | ScalarFunction::ConvertTz
                    | ScalarFunction::Char
                    | ScalarFunction::Rand
                    // CONV and MAKETIME render short fixed-width strings.
                    | ScalarFunction::Conv
                    | ScalarFunction::MakeTime => 64,
                    ScalarFunction::Length
                    | ScalarFunction::CharLength
                    | ScalarFunction::DecimalComparison { .. }
                    | ScalarFunction::Locate
                    | ScalarFunction::Like { .. }
                    | ScalarFunction::InList { .. }
                    | ScalarFunction::Between { .. }
                    | ScalarFunction::DatePart(_)
                    | ScalarFunction::DateDiff
                    | ScalarFunction::UnixTimestamp
                    | ScalarFunction::Round { .. }
                    | ScalarFunction::Ceil { .. }
                    | ScalarFunction::Floor { .. }
                    | ScalarFunction::Sign
                    | ScalarFunction::Power
                    | ScalarFunction::Sqrt
                    | ScalarFunction::Exp
                    | ScalarFunction::Ln
                    | ScalarFunction::LogBase
                    | ScalarFunction::Log2
                    | ScalarFunction::Log10
                    | ScalarFunction::Truncate { .. }
                    | ScalarFunction::Instr
                    | ScalarFunction::FindInSet
                    | ScalarFunction::Ascii
                    | ScalarFunction::Ord
                    | ScalarFunction::Field
                    | ScalarFunction::ToDays
                    | ScalarFunction::YearWeek
                    | ScalarFunction::TimeToSec
                    | ScalarFunction::RegexpLike { .. }
                    | ScalarFunction::RegexpInstr
                    | ScalarFunction::TimestampDiff { .. }
                    // The JSON predicates answer numerically; the same
                    // scalar bound covers them.
                    | ScalarFunction::JsonValid
                    | ScalarFunction::JsonLength
                    | ScalarFunction::JsonContains
                    | ScalarFunction::JsonContainsPath => 24,
                    ScalarFunction::Repeat
                    | ScalarFunction::Space
                    | ScalarFunction::Lpad
                    | ScalarFunction::Rpad => STRING_BUILD_CAP,
                    ScalarFunction::Md5 => 32,
                    ScalarFunction::Hex | ScalarFunction::ToBase64 => {
                        first.saturating_mul(2).saturating_add(24)
                    }
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

/// Date-part extraction straight from packed temporal units. Returns
/// `None` when the column does not carry units (the caller falls back to
/// the text paths).
pub(crate) fn evaluate_units_date_part(
    batch: &RecordBatch,
    column: usize,
    row: usize,
    part: DatePart,
) -> Option<Result<Value, ExecError>> {
    use crate::batch::TypedValues;
    let vector = batch.column(column)?;
    let (typed, validity) = vector.typed()?;
    let TypedValues::Temporal { units, .. } = typed else {
        return None;
    };
    if !validity.is_valid(row) {
        return Some(Ok(Value::Null));
    }
    let unit = *units.get(row)?;
    let (days, second_of_day) = match vector.data_type() {
        DataType::Date32 => (unit, None),
        DataType::DateTime64 { .. } => {
            const MICROS_PER_DAY: i64 = 86_400_000_000;
            (
                unit.div_euclid(MICROS_PER_DAY),
                Some(unit.rem_euclid(MICROS_PER_DAY) / 1_000_000),
            )
        }
        _ => return None,
    };
    let value = match part {
        DatePart::Year | DatePart::Month | DatePart::Day => {
            let (year, month, day) = pintail_types::civil_from_days(days);
            match part {
                DatePart::Year => u64::try_from(year).unwrap_or(0),
                DatePart::Month => u64::try_from(month).unwrap_or(0),
                DatePart::Day => u64::try_from(day).unwrap_or(0),
                _ => unreachable!(),
            }
        }
        DatePart::Hour | DatePart::Minute | DatePart::Second => {
            // MySQL's HOUR/MINUTE/SECOND of a plain DATE are 0.
            let second_of_day = second_of_day.unwrap_or(0);
            let value = match part {
                DatePart::Hour => second_of_day / 3600,
                DatePart::Minute => second_of_day / 60 % 60,
                DatePart::Second => second_of_day % 60,
                _ => unreachable!(),
            };
            u64::try_from(value).unwrap_or(0)
        }
        // Calendar-topology parts take the generic parse path.
        DatePart::Quarter
        | DatePart::DayOfWeek
        | DatePart::WeekDay
        | DatePart::DayOfYear
        | DatePart::Week
        | DatePart::IsoWeek
        | DatePart::WeekMode(_) => return None,
    };
    Some(Ok(Value::UInt64(value)))
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
        DatePart::Quarter
        | DatePart::DayOfWeek
        | DatePart::WeekDay
        | DatePart::DayOfYear
        | DatePart::Week
        | DatePart::IsoWeek
        | DatePart::WeekMode(_) => return None,
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
                _ => unreachable!(),
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
    argument_types: &[Option<DataType>],
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
            evaluate_eager_scalar_typed(function, &values, argument_types, data_type)
        }
    }
}

#[cfg(test)]
fn evaluate_eager_scalar(
    function: ScalarFunction,
    values: &[Value],
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    let argument_types = vec![None; values.len()];
    evaluate_eager_scalar_typed(function, values, &argument_types, data_type)
}

#[allow(clippy::too_many_lines)]
fn evaluate_eager_scalar_typed(
    function: ScalarFunction,
    values: &[Value],
    argument_types: &[Option<DataType>],
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    if values.iter().any(|value| matches!(value, Value::Null))
        && !matches!(
            function,
            ScalarFunction::InList { .. }
                | ScalarFunction::NullIf
                // NULL arguments are data, not poison, for these: CONCAT_WS
                // skips them, ELT/FIELD treat them positionally, CHAR drops
                // them, and the JSON constructors encode them as JSON null.
                | ScalarFunction::ConcatWs
                | ScalarFunction::Elt
                | ScalarFunction::Field
                | ScalarFunction::Char
                | ScalarFunction::JsonObject
                | ScalarFunction::JsonArray
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
        // MySQL's LOWER/UPPER are ineffective on a binary string: case is a
        // property of a character set, and a binary value has none. Folding
        // it anyway rewrote bytes the caller asked to keep exact.
        ScalarFunction::Lower | ScalarFunction::Upper => {
            if let Value::Binary(bytes) = &values[0] {
                return Ok(Value::Binary(bytes.clone()));
            }
            let text = scalar_string(&values[0])?;
            Ok(Value::Utf8(if function == ScalarFunction::Lower {
                text.to_lowercase()
            } else {
                text.to_uppercase()
            }))
        }
        ScalarFunction::Trim => Ok(Value::Utf8(
            // MySQL's default TRIM removes the space character only. Rust's
            // trim() removes the whole Unicode whitespace class, so a
            // leading tab used to disappear: HEX(TRIM(CHAR(9))) answered ''
            // where MySQL answers '09'.
            scalar_string(&values[0])?.trim_matches(' ').to_owned(),
        )),
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
            // A binary operand makes the comparison case-sensitive, because
            // there is no collation to fold under.
            let binary = binary_operand(&values[0..2]);
            let needle = fold_unless_binary(&scalar_string(&values[0])?, binary);
            let haystack = fold_unless_binary(&scalar_string(&values[1])?, binary);
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
        ScalarFunction::DecimalComparison { op } => {
            let ordering = compare_decimal_values(&values[0], &values[1])?;
            Ok(Value::Boolean(match op {
                BinaryOp::Equal => ordering == Ordering::Equal,
                BinaryOp::NotEqual => ordering != Ordering::Equal,
                BinaryOp::Less => ordering == Ordering::Less,
                BinaryOp::LessOrEqual => ordering != Ordering::Greater,
                BinaryOp::Greater => ordering == Ordering::Greater,
                BinaryOp::GreaterOrEqual => ordering != Ordering::Less,
                _ => return Err(ExecError::InvalidExpressionType),
            }))
        }
        ScalarFunction::Cast(target) => cast_scalar(&values[0], Some(target)),
        ScalarFunction::Abs { decimal } => match &values[0] {
            Value::Int64(signed) => signed
                .checked_abs()
                .map(Value::Int64)
                .ok_or(ExecError::NumericOverflow),
            Value::UInt64(unsigned) => Ok(Value::UInt64(*unsigned)),
            Value::Utf8(text) if decimal => Ok(Value::Utf8(
                text.strip_prefix('-').unwrap_or(text).to_owned(),
            )),
            value => Ok(Value::float64(mysql_f64(value)?.abs())),
        },
        ScalarFunction::Sign => {
            let value = mysql_f64(&values[0])?;
            Ok(Value::Int64(if value > 0.0 {
                1
            } else if value < 0.0 {
                -1
            } else {
                0
            }))
        }
        ScalarFunction::Power => {
            let result = mysql_f64(&values[0])?.powf(mysql_f64(&values[1])?);
            if result.is_finite() {
                Ok(Value::float64(result))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Sqrt => {
            let value = mysql_f64(&values[0])?;
            // MySQL returns NULL outside the domain instead of erroring.
            if value < 0.0 {
                Ok(Value::Null)
            } else {
                Ok(Value::float64(value.sqrt()))
            }
        }
        ScalarFunction::Exp => {
            let result = mysql_f64(&values[0])?.exp();
            if result.is_finite() {
                Ok(Value::float64(result))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Ln | ScalarFunction::Log2 | ScalarFunction::Log10 => {
            let value = mysql_f64(&values[0])?;
            if value <= 0.0 {
                return Ok(Value::Null);
            }
            Ok(Value::float64(match function {
                ScalarFunction::Ln => value.ln(),
                ScalarFunction::Log2 => value.log2(),
                _ => value.log10(),
            }))
        }
        ScalarFunction::LogBase => {
            let base = mysql_f64(&values[0])?;
            let value = mysql_f64(&values[1])?;
            if base <= 0.0 || (base - 1.0).abs() < f64::EPSILON || value <= 0.0 {
                return Ok(Value::Null);
            }
            Ok(Value::float64(value.log(base)))
        }
        ScalarFunction::Truncate { decimal } => {
            if decimal && let Value::Utf8(text) = &values[0] {
                let digits = mysql_i64(&values[1])?;
                let input_scale = i64::try_from(
                    text.rsplit_once('.')
                        .map_or(0, |(_, fraction)| fraction.len()),
                )
                .map_err(|_| ExecError::NumericOverflow)?;
                let render_scale = u8::try_from(digits.clamp(0, input_scale))
                    .map_err(|_| ExecError::NumericOverflow)?;
                let units = pintail_types::parse_decimal_rounded(
                    text,
                    u8::try_from(input_scale).unwrap_or(30),
                )
                .ok_or(ExecError::NumericOverflow)?;
                let drop = u32::try_from(input_scale - i64::from(render_scale))
                    .unwrap_or(0)
                    .min(38);
                let factor = 10_i128
                    .checked_pow(drop)
                    .ok_or(ExecError::NumericOverflow)?;
                // i128 division truncates toward zero — TRUNCATE's contract.
                let mut units = units / factor;
                if digits < 0 {
                    let zeroed = u32::try_from((-digits).min(38)).unwrap_or(38);
                    let zero_factor = 10_i128
                        .checked_pow(zeroed)
                        .ok_or(ExecError::NumericOverflow)?;
                    units = units / zero_factor * zero_factor;
                }
                return Ok(Value::Utf8(pintail_types::format_decimal_scaled(
                    units,
                    render_scale,
                )));
            }
            let value = mysql_f64(&values[0])?;
            let digits = mysql_i64(&values[1])?.clamp(-30, 30);
            let factor = 10_f64.powi(i32::try_from(digits).expect("clamped to i32 range"));
            let truncated = (value * factor).trunc() / factor;
            if truncated.is_finite() {
                Ok(Value::float64(truncated))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Greatest { decimal } | ScalarFunction::Least { decimal } => {
            let greatest = matches!(function, ScalarFunction::Greatest { .. });
            let mut best: Option<&Value> = None;
            for value in values {
                if matches!(value, Value::Null) {
                    // MySQL: NULL poisons GREATEST/LEAST.
                    return Ok(Value::Null);
                }
                best = Some(match best {
                    None => value,
                    Some(current) => {
                        let ordering = if decimal {
                            compare_decimal_values(value, current)?
                        } else {
                            compare_mysql(value, current)?
                        };
                        if (ordering == Ordering::Greater) == greatest {
                            value
                        } else {
                            current
                        }
                    }
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
        ScalarFunction::ConcatWs => {
            if matches!(values[0], Value::Null) {
                return Ok(Value::Null);
            }
            let separator = scalar_string(&values[0])?;
            let parts = values[1..]
                .iter()
                .filter(|value| !matches!(value, Value::Null))
                .map(scalar_string)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Utf8(parts.join(&separator)))
        }
        ScalarFunction::Reverse => Ok(Value::Utf8(
            scalar_string(&values[0])?.chars().rev().collect(),
        )),
        ScalarFunction::Repeat => {
            let text = scalar_string(&values[0])?;
            let count = mysql_i64(&values[1])?;
            if count <= 0 {
                return Ok(Value::Utf8(String::new()));
            }
            repeat_capped(&text, count)
        }
        ScalarFunction::Space => {
            let count = mysql_i64(&values[0])?;
            if count <= 0 {
                return Ok(Value::Utf8(String::new()));
            }
            repeat_capped(" ", count)
        }
        ScalarFunction::Lpad | ScalarFunction::Rpad => {
            let text = scalar_string(&values[0])?;
            let target = mysql_i64(&values[1])?;
            let pad = scalar_string(&values[2])?;
            mysql_pad(
                &text,
                target,
                &pad,
                matches!(function, ScalarFunction::Lpad),
            )
        }
        ScalarFunction::Instr => {
            let binary = binary_operand(&values[0..2]);
            let haystack = fold_unless_binary(&scalar_string(&values[0])?, binary);
            let needle = fold_unless_binary(&scalar_string(&values[1])?, binary);
            Ok(Value::UInt64(locate(&needle, &haystack, 1)))
        }
        ScalarFunction::FindInSet => {
            let needle = scalar_string(&values[0])?;
            let list = scalar_string(&values[1])?;
            if needle.contains(',') {
                return Ok(Value::UInt64(0));
            }
            let position = list
                .split(',')
                .position(|entry| compare_utf8_mysql(entry, &needle) == Ordering::Equal)
                .map_or(0, |index| index as u64 + 1);
            Ok(Value::UInt64(if list.is_empty() { 0 } else { position }))
        }
        ScalarFunction::Ascii => Ok(Value::UInt64(
            scalar_string(&values[0])?
                .bytes()
                .next()
                .map_or(0, u64::from),
        )),
        ScalarFunction::Ord => {
            let text = scalar_string(&values[0])?;
            let Some(first) = text.chars().next() else {
                return Ok(Value::UInt64(0));
            };
            // MySQL: the leading character's bytes read big-endian.
            let mut buffer = [0_u8; 4];
            let encoded = first.encode_utf8(&mut buffer).as_bytes();
            Ok(Value::UInt64(encoded.iter().fold(0_u64, |total, byte| {
                total.wrapping_mul(256).wrapping_add(u64::from(*byte))
            })))
        }
        ScalarFunction::Hex => match &values[0] {
            Value::Int64(signed) => Ok(Value::Utf8(format!("{signed:X}"))),
            Value::UInt64(unsigned) => Ok(Value::Utf8(format!("{unsigned:X}"))),
            Value::Binary(bytes) => Ok(Value::Utf8(hex_upper(bytes))),
            value => Ok(Value::Utf8(hex_upper(scalar_string(value)?.as_bytes()))),
        },
        ScalarFunction::Md5 => {
            let digest = match &values[0] {
                Value::Binary(bytes) => Md5::digest(bytes),
                value => Md5::digest(scalar_string(value)?.as_bytes()),
            };
            Ok(Value::Utf8(format!("{digest:x}")))
        }
        ScalarFunction::Unhex => {
            let text = scalar_string(&values[0])?;
            Ok(unhex(&text).map_or(Value::Null, Value::Binary))
        }
        ScalarFunction::Elt => {
            let index = mysql_i64(&values[0])?;
            if index < 1 {
                return Ok(Value::Null);
            }
            match values.get(usize::try_from(index).unwrap_or(usize::MAX)) {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(value) => Ok(Value::Utf8(scalar_string(value)?)),
            }
        }
        ScalarFunction::Field => {
            if matches!(values[0], Value::Null) {
                return Ok(Value::UInt64(0));
            }
            let needle = scalar_string(&values[0])?;
            for (index, value) in values[1..].iter().enumerate() {
                if !matches!(value, Value::Null)
                    && compare_utf8_mysql(&scalar_string(value)?, &needle) == Ordering::Equal
                {
                    return Ok(Value::UInt64(index as u64 + 1));
                }
            }
            Ok(Value::UInt64(0))
        }
        ScalarFunction::Format => {
            let value = mysql_f64(&values[0])?;
            let digits = mysql_i64(&values[1])?.clamp(0, 30);
            Ok(Value::Utf8(format_grouped(
                value,
                usize::try_from(digits).expect("clamped to usize range"),
            )))
        }
        ScalarFunction::ToBase64 => {
            let text = scalar_string(&values[0])?;
            // MySQL breaks the encoding every 76 characters, so a 58-byte
            // subject encodes to 81 characters rather than 80.
            let encoded = base64_encode(text.as_bytes());
            let mut wrapped = String::with_capacity(encoded.len() + encoded.len() / 76);
            for (index, chunk) in encoded.as_bytes().chunks(76).enumerate() {
                if index > 0 {
                    wrapped.push('\n');
                }
                wrapped.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            }
            Ok(Value::Utf8(wrapped))
        }
        ScalarFunction::FromBase64 => {
            let text = scalar_string(&values[0])?;
            Ok(base64_decode(&text).map_or(Value::Null, Value::Binary))
        }
        ScalarFunction::Round { decimal } => {
            if decimal && let Value::Utf8(text) = &values[0] {
                let digits = values.get(1).map(mysql_i64).transpose()?.unwrap_or(0);
                let input_scale = i64::try_from(
                    text.rsplit_once('.')
                        .map_or(0, |(_, fraction)| fraction.len()),
                )
                .map_err(|_| ExecError::NumericOverflow)?;
                // MySQL keeps min(input scale, digit count) fraction digits;
                // negative digit counts zero whole-number positions.
                let render_scale = u8::try_from(digits.clamp(0, input_scale))
                    .map_err(|_| ExecError::NumericOverflow)?;
                let units = pintail_types::parse_decimal_rounded(text, render_scale)
                    .ok_or(ExecError::NumericOverflow)?;
                let units = if digits < 0 {
                    let zeroed = u32::try_from((-digits).min(38)).unwrap_or(38);
                    let factor = 10_i128
                        .checked_pow(zeroed)
                        .ok_or(ExecError::NumericOverflow)?;
                    let half = factor / 2;
                    let magnitude = units
                        .unsigned_abs()
                        .checked_add(half.unsigned_abs())
                        .ok_or(ExecError::NumericOverflow)?
                        / factor.unsigned_abs()
                        * factor.unsigned_abs();
                    let magnitude =
                        i128::try_from(magnitude).map_err(|_| ExecError::NumericOverflow)?;
                    if units < 0 { -magnitude } else { magnitude }
                } else {
                    units
                };
                return Ok(Value::Utf8(pintail_types::format_decimal_scaled(
                    units,
                    render_scale,
                )));
            }
            let value = mysql_f64(&values[0])?;
            let decimals = values.get(1).map(mysql_i64).transpose()?.unwrap_or(0);
            let decimals =
                i32::try_from(decimals.clamp(-308, 308)).map_err(|_| ExecError::NumericOverflow)?;
            // MySQL rounds an APPROXIMATE operand to nearest-even, deferring
            // to the C library, and only exact operands round half away from
            // zero. Rust's f64::round is half-away-from-zero, so ROUND(25E-1)
            // answered 3 where MySQL answers 2. The exact path above keeps
            // half-away-from-zero, which is why ROUND(2.5) is still 3.
            let rounded = if decimals >= 0 {
                let factor = 10_f64.powi(decimals);
                let scaled = value * factor;
                if scaled.is_finite() {
                    scaled.round_ties_even() / factor
                } else {
                    value
                }
            } else {
                let factor = 10_f64.powi(-decimals);
                (value / factor).round_ties_even() * factor
            };
            if rounded.is_finite() {
                Ok(Value::float64(rounded))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Ceil { decimal } => {
            if decimal && let Value::Utf8(text) = &values[0] {
                return decimal_integer_bound(text, true);
            }
            let value = mysql_f64(&values[0])?.ceil();
            if value.is_finite() {
                Ok(Value::float64(value))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::Floor { decimal } => {
            if decimal && let Value::Utf8(text) = &values[0] {
                return decimal_integer_bound(text, false);
            }
            let value = mysql_f64(&values[0])?.floor();
            if value.is_finite() {
                Ok(Value::float64(value))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        ScalarFunction::TimestampDiff { unit } => {
            let from = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            let to = parse_mysql_datetime(&scalar_string(&values[1])?)?;
            Ok(Value::Int64(timestamp_diff(from, to, unit)))
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
            Ok(Value::Utf8(mysql_date_format(
                value,
                &scalar_string(&values[1])?,
            )))
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
        ScalarFunction::DayName => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            Ok(Value::Utf8(value.format("%A").to_string()))
        }
        ScalarFunction::MonthName => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?;
            Ok(Value::Utf8(value.format("%B").to_string()))
        }
        ScalarFunction::LastDay => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?.date();
            let first_next = if value.month() == 12 {
                chrono::NaiveDate::from_ymd_opt(value.year() + 1, 1, 1)
            } else {
                chrono::NaiveDate::from_ymd_opt(value.year(), value.month() + 1, 1)
            }
            .ok_or(ExecError::InvalidDateTime)?;
            Ok(Value::Utf8(
                first_next
                    .pred_opt()
                    .ok_or(ExecError::InvalidDateTime)?
                    .format("%Y-%m-%d")
                    .to_string(),
            ))
        }
        ScalarFunction::ToDays => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?.date();
            let days = value
                .signed_duration_since(
                    chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch is valid"),
                )
                .num_days()
                + TO_DAYS_EPOCH_OFFSET;
            Ok(Value::UInt64(u64::try_from(days).unwrap_or(0)))
        }
        ScalarFunction::FromDays => {
            let days = mysql_i64(&values[0])? - TO_DAYS_EPOCH_OFFSET;
            let date = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .expect("epoch is valid")
                .checked_add_signed(chrono::Duration::days(days))
                .ok_or(ExecError::InvalidDateTime)?;
            Ok(Value::Utf8(date.format("%Y-%m-%d").to_string()))
        }
        ScalarFunction::YearWeek => {
            let value = parse_mysql_datetime(&scalar_string(&values[0])?)?.date();
            Ok(Value::UInt64(mysql_yearweek(value)))
        }
        ScalarFunction::TimeToSec => {
            let text = scalar_string(&values[0])?;
            let (negative, rest) = text
                .strip_prefix('-')
                .map_or((false, text.as_str()), |rest| (true, rest));
            let mut parts = rest.split(':');
            let hours = parts
                .next()
                .and_then(|part| part.parse::<i64>().ok())
                .ok_or(ExecError::InvalidDateTime)?;
            let minutes = parts
                .next()
                .and_then(|part| part.parse::<i64>().ok())
                .unwrap_or(0);
            let seconds = parts
                .next()
                .and_then(|part| part.split('.').next())
                .and_then(|part| part.parse::<i64>().ok())
                .unwrap_or(0);
            let total = hours * 3600 + minutes * 60 + seconds;
            Ok(Value::Int64(if negative { -total } else { total }))
        }
        ScalarFunction::SecToTime => {
            // MySQL keeps the argument's fractional seconds: SEC_TO_TIME(1.5)
            // is '00:00:01.5', not '00:00:01'. The fraction is rendered from
            // the original text so its digit count survives.
            let fraction = match &values[0] {
                Value::Utf8(text) => text
                    .split_once('.')
                    .map(|(_, digits)| digits.trim_end_matches('0').to_owned())
                    .filter(|digits| !digits.is_empty()),
                Value::Float64(number) => {
                    let rendered = format!("{}", number.get());
                    rendered
                        .split_once('.')
                        .map(|(_, digits)| digits.trim_end_matches('0').to_owned())
                        .filter(|digits| !digits.is_empty())
                }
                _ => None,
            };
            let seconds = mysql_i64(&values[0])?;
            // MySQL clamps TIME to +/- 838:59:59.
            let clamped = seconds.clamp(-3_020_399, 3_020_399);
            let magnitude = clamped.unsigned_abs();
            let sign = if clamped < 0 { "-" } else { "" };
            let suffix = fraction.map_or_else(String::new, |digits| format!(".{digits}"));
            Ok(Value::Utf8(format!(
                "{sign}{:02}:{:02}:{:02}{suffix}",
                magnitude / 3600,
                magnitude / 60 % 60,
                magnitude % 60
            )))
        }
        ScalarFunction::MakeDate => {
            let year = mysql_i64(&values[0])?;
            let day_of_year = mysql_i64(&values[1])?;
            if day_of_year < 1 {
                return Ok(Value::Null);
            }
            let Ok(year) = i32::try_from(year) else {
                return Ok(Value::Null);
            };
            let start = chrono::NaiveDate::from_ymd_opt(year, 1, 1);
            let date = start.and_then(|start| {
                start.checked_add_signed(chrono::Duration::days(day_of_year - 1))
            });
            Ok(date.map_or(Value::Null, |date| {
                Value::Utf8(date.format("%Y-%m-%d").to_string())
            }))
        }
        ScalarFunction::Curtime => Ok(Value::Utf8(
            Local::now().naive_local().format("%H:%M:%S").to_string(),
        )),
        ScalarFunction::StrToDate => {
            let text = scalar_string(&values[0])?;
            let format = chrono_parse_format(&scalar_string(&values[1])?);
            if let Ok(value) = NaiveDateTime::parse_from_str(&text, &format) {
                return Ok(Value::Utf8(value.format("%Y-%m-%d %H:%M:%S").to_string()));
            }
            if let Ok(value) = chrono::NaiveDate::parse_from_str(&text, &format) {
                return Ok(Value::Utf8(value.format("%Y-%m-%d").to_string()));
            }
            Ok(Value::Null)
        }
        ScalarFunction::ConvertTz => {
            let text = scalar_string(&values[0])?;
            let from = scalar_string(&values[1])?;
            let to = scalar_string(&values[2])?;
            Ok(convert_tz(&text, &from, &to).map_or(Value::Null, Value::Utf8))
        }
        ScalarFunction::Char => {
            let mut bytes = Vec::with_capacity(values.len() * 4);
            for value in values {
                if matches!(value, Value::Null) {
                    continue;
                }
                // MySQL wraps each code point to u32 and emits its minimal
                // big-endian bytes; zero is a single 0x00 byte.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let point = mysql_i64(value)? as u32;
                let encoded = point.to_be_bytes();
                let start = encoded
                    .iter()
                    .position(|byte| *byte != 0)
                    .unwrap_or(encoded.len() - 1);
                bytes.extend_from_slice(&encoded[start..]);
            }
            Ok(Value::Binary(bytes))
        }
        ScalarFunction::Rand => Ok(Value::float64(rand::random::<f64>())),
        ScalarFunction::RegexpLike { negated } => {
            let text = scalar_string(&values[0])?;
            let match_type = values.get(2).map(scalar_string).transpose()?;
            let text = if match_type.as_deref().unwrap_or("").contains('u') {
                text
            } else {
                normalize_mysql_regex_line_endings(&text)
            };
            let matched = compiled_regex_with_match_type(
                &scalar_string(&values[1])?,
                match_type.as_deref().unwrap_or(""),
            )?
            .is_match(&text);
            Ok(Value::Boolean(matched != negated))
        }
        ScalarFunction::RegexpSubstr => {
            let text = scalar_string(&values[0])?;
            let found = compiled_regex(&scalar_string(&values[1])?)?
                .find(&text)
                .map(|found| found.as_str().to_owned());
            Ok(found.map_or(Value::Null, Value::Utf8))
        }
        ScalarFunction::RegexpInstr => {
            let text = scalar_string(&values[0])?;
            let position = compiled_regex(&scalar_string(&values[1])?)?
                .find(&text)
                .map_or(0, |found| text[..found.start()].chars().count() as u64 + 1);
            Ok(Value::UInt64(position))
        }
        ScalarFunction::RegexpReplace => {
            let text = scalar_string(&values[0])?;
            let replacement = scalar_string(&values[2])?;
            let replaced = compiled_regex(&scalar_string(&values[1])?)?
                .replace_all(&text, replacement.as_str())
                .into_owned();
            Ok(Value::Utf8(replaced))
        }
        ScalarFunction::JsonExtract { unquote } => {
            let document = scalar_string(&values[0])?;
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&document) else {
                return Err(ExecError::InvalidExpressionType);
            };
            if values.len() > 2 {
                let mut found = Vec::with_capacity(values.len() - 1);
                for path in &values[1..] {
                    if let Some(value) = json_path_lookup(&parsed, &scalar_string(path)?)? {
                        found.push(value.clone());
                    }
                }
                return Ok(if found.is_empty() {
                    Value::Null
                } else {
                    Value::Utf8(mysql_json_text(&serde_json::Value::Array(found)))
                });
            }
            let Some(found) = json_path_lookup(&parsed, &scalar_string(&values[1])?)? else {
                return Ok(Value::Null);
            };
            Ok(Value::Utf8(match found {
                serde_json::Value::String(text) if unquote => text.clone(),
                other => mysql_json_text(other),
            }))
        }
        ScalarFunction::SubstringIndex => {
            let text = scalar_string(&values[0])?;
            let delimiter = scalar_string(&values[1])?;
            let count = mysql_i64(&values[2])?;
            Ok(Value::Utf8(substring_index(&text, &delimiter, count)))
        }
        ScalarFunction::Conv => {
            let subject = scalar_string(&values[0])?;
            let from = mysql_i64(&values[1])?;
            let to = mysql_i64(&values[2])?;
            Ok(conv_base(&subject, from, to).map_or(Value::Null, Value::Utf8))
        }
        ScalarFunction::MakeTime => {
            let hour = mysql_i64(&values[0])?;
            let minute = mysql_i64(&values[1])?;
            // MySQL keeps a fractional second: MAKETIME(12,15,30.5) is
            // 12:15:30.500000. The fraction is read from the argument's own
            // text so its digit count survives the integer conversion.
            let seconds_text = scalar_string(&values[2]).unwrap_or_default();
            let fraction = seconds_text
                .split_once('.')
                .map(|(_, digits)| digits.trim_end_matches('0').to_owned())
                .filter(|digits| !digits.is_empty());
            let second = mysql_i64(&values[2])?;
            Ok(make_time(hour, minute, second)
                .map(|rendered| match fraction {
                    Some(digits) => format!("{rendered}.{digits}"),
                    None => rendered,
                })
                .map_or(Value::Null, Value::Utf8))
        }
        ScalarFunction::JsonValue => {
            // JSON_VALUE extracts and unquotes; a RETURNING type is lowered
            // to a CAST around this, so the extraction stays one job.
            let document = parse_json_argument(&values[0])?;
            let path = scalar_string(&values[1])?;
            Ok(
                json_path_lookup(&document, &path)?.map_or(Value::Null, |found| {
                    Value::Utf8(match found {
                        serde_json::Value::String(text) => text.clone(),
                        other => mysql_json_text(other),
                    })
                }),
            )
        }
        ScalarFunction::JsonSearch => {
            let document = parse_json_argument(&values[0])?;
            let mode = scalar_string(&values[1])?;
            let all = if mode.eq_ignore_ascii_case("all") {
                true
            } else if mode.eq_ignore_ascii_case("one") {
                false
            } else {
                return Err(ExecError::InvalidExpressionType);
            };
            let pattern = scalar_string(&values[2])?;
            let escape = match values.get(3) {
                None | Some(Value::Null) => Some('\\'),
                Some(value) => scalar_string(value)?.chars().next().or(Some('\\')),
            };
            let mut found = Vec::new();
            json_search(&document, &pattern, escape, all, "$", &mut found);
            Ok(match found.len() {
                // MySQL answers NULL when nothing matches, one bare path for
                // a single hit, and an array once there are several.
                0 => Value::Null,
                1 if !all => {
                    Value::Utf8(mysql_json_text(&serde_json::Value::String(found.remove(0))))
                }
                _ => {
                    if !all {
                        found.truncate(1);
                    }
                    let paths = found.into_iter().map(serde_json::Value::String).collect();
                    Value::Utf8(mysql_json_text(&serde_json::Value::Array(paths)))
                }
            })
        }
        ScalarFunction::JsonValid => {
            // JSON_VALID answers 0 or 1 for any non-NULL input rather than
            // raising, which is what makes it usable as a guard ahead of the
            // functions below.
            let text = scalar_string(&values[0])?;
            Ok(Value::Int64(i64::from(
                serde_json::from_str::<serde_json::Value>(&text).is_ok(),
            )))
        }
        ScalarFunction::JsonType => {
            let parsed = parse_json_argument(&values[0])?;
            Ok(Value::Utf8(json_type_name(&parsed).to_owned()))
        }
        ScalarFunction::JsonLength => {
            let parsed = parse_json_argument(&values[0])?;
            let found = match values.get(1) {
                None => Some(&parsed),
                Some(path) => json_path_lookup(&parsed, &scalar_string(path)?)?,
            };
            Ok(found.map_or(Value::Null, |value| {
                // A scalar has length 1; only containers count members.
                Value::UInt64(match value {
                    serde_json::Value::Array(items) => items.len() as u64,
                    serde_json::Value::Object(members) => members.len() as u64,
                    _ => 1,
                })
            }))
        }
        ScalarFunction::JsonKeys => {
            let parsed = parse_json_argument(&values[0])?;
            let found = match values.get(1) {
                None => Some(&parsed),
                Some(path) => json_path_lookup(&parsed, &scalar_string(path)?)?,
            };
            // MySQL answers NULL rather than raising when the target is not
            // an object.
            Ok(match found {
                Some(serde_json::Value::Object(members)) => {
                    // MySQL's binary JSON stores object keys shortest-first,
                    // then bytewise, and JSON_KEYS reports that order —
                    // ["b", "aa"], not ["aa", "b"]. mysql_json_text already
                    // sorts this way when rendering an object; the key
                    // extractor has to agree with it.
                    let mut keys = members.keys().cloned().collect::<Vec<_>>();
                    keys.sort_by(|left, right| {
                        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                    });
                    let keys = keys.into_iter().map(serde_json::Value::String).collect();
                    Value::Utf8(mysql_json_text(&serde_json::Value::Array(keys)))
                }
                _ => Value::Null,
            })
        }
        ScalarFunction::JsonContains => {
            let target = parse_json_argument(&values[0])?;
            let candidate = parse_json_argument(&values[1])?;
            let scoped = match values.get(2) {
                None => Some(&target),
                Some(path) => json_path_lookup(&target, &scalar_string(path)?)?,
            };
            Ok(scoped.map_or(Value::Null, |value| {
                Value::Int64(i64::from(json_contains(value, &candidate)))
            }))
        }
        ScalarFunction::JsonContainsPath => {
            let parsed = parse_json_argument(&values[0])?;
            let mode = scalar_string(&values[1])?;
            let require_all = if mode.eq_ignore_ascii_case("all") {
                true
            } else if mode.eq_ignore_ascii_case("one") {
                false
            } else {
                return Err(ExecError::InvalidExpressionType);
            };
            let mut found = 0_usize;
            let paths = &values[2..];
            for path in paths {
                if json_path_lookup(&parsed, &scalar_string(path)?)?.is_some() {
                    found += 1;
                }
            }
            Ok(Value::Int64(i64::from(if require_all {
                found == paths.len()
            } else {
                found > 0
            })))
        }
        ScalarFunction::JsonObject => {
            let mut members = serde_json::Map::new();
            for (index, pair) in values.chunks_exact(2).enumerate() {
                // MySQL rejects NULL member names, coerces other key types
                // to text, and keeps the LAST occurrence of a duplicate key
                // (verified against 8.4).
                if matches!(pair[0], Value::Null) {
                    return Err(ExecError::InvalidExpressionType);
                }
                members.insert(
                    scalar_string(&pair[0])?,
                    json_value_of_typed(
                        &pair[1],
                        argument_types.get(index * 2 + 1).copied().flatten(),
                    )?,
                );
            }
            Ok(Value::Utf8(mysql_json_text(&serde_json::Value::Object(
                members,
            ))))
        }
        ScalarFunction::JsonArray => Ok(Value::Utf8(mysql_json_text(&serde_json::Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    json_value_of_typed(value, argument_types.get(index).copied().flatten())
                })
                .collect::<Result<Vec<_>, _>>()?,
        )))),
        ScalarFunction::JsonUnquote => {
            let text = scalar_string(&values[0])?;
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::String(inner)) => Ok(Value::Utf8(inner)),
                _ => Ok(Value::Utf8(text)),
            }
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
        DatePart::Quarter => u64::from((value.month() - 1) / 3 + 1),
        // MySQL DAYOFWEEK: 1 = Sunday .. 7 = Saturday.
        DatePart::DayOfWeek => u64::from(value.weekday().num_days_from_sunday() + 1),
        // MySQL WEEKDAY: 0 = Monday .. 6 = Sunday.
        DatePart::WeekDay => u64::from(value.weekday().num_days_from_monday()),
        DatePart::DayOfYear => u64::from(value.ordinal()),
        DatePart::Week => mysql_week_mode0(value.date()),
        DatePart::IsoWeek => u64::from(value.date().iso_week().week()),
        DatePart::WeekMode(mode) => u64::from(mysql_calc_week(value.date(), u32::from(mode)).1),
    }
}

/// `MySQL` `WEEK` default mode 0: Sunday-start weeks numbered 0-53. Week 1
/// begins on the year's first Sunday; days before it are week 0, and a
/// year that starts on Sunday starts in week 1.
fn mysql_week_mode0(date: chrono::NaiveDate) -> u64 {
    let january_first = date.with_ordinal(1).expect("ordinal 1 is valid");
    let offset = u64::from(january_first.weekday().num_days_from_sunday());
    let week = (u64::from(date.ordinal()) - 1 + offset) / 7;
    if offset == 0 { week + 1 } else { week }
}

/// `MySQL` `YEARWEEK` default mode 0: `year * 100 + week`, where dates in
/// week 0 report the final week of the previous year instead.
fn mysql_yearweek(date: chrono::NaiveDate) -> u64 {
    let week = mysql_week_mode0(date);
    if week > 0 {
        return u64::try_from(date.year()).unwrap_or(0) * 100 + week;
    }
    let previous_december =
        chrono::NaiveDate::from_ymd_opt(date.year() - 1, 12, 31).expect("december 31 is valid");
    let january_first = previous_december
        .with_ordinal(1)
        .expect("ordinal 1 is valid");
    let offset = u64::from(january_first.weekday().num_days_from_sunday());
    // Count the date as a continuation of the previous year's weeks.
    let days = u64::from(previous_december.ordinal()) + u64::from(date.ordinal()) - 1;
    let week = (days + offset) / 7 + u64::from(offset == 0);
    u64::try_from(date.year() - 1).unwrap_or(0) * 100 + week
}

/// Days between year 0 and the Unix epoch in `MySQL`'s `TO_DAYS` calendar.
const TO_DAYS_EPOCH_OFFSET: i64 = 719_528;

/// `MySQL` `TIMESTAMPDIFF`: complete units from `from` to `to`, truncated
/// toward zero (negative when `to` precedes `from`). `Chrono`'s duration
/// accessors already truncate toward zero for the clock units.
fn timestamp_diff(from: NaiveDateTime, to: NaiveDateTime, unit: IntervalUnit) -> i64 {
    let elapsed = to.signed_duration_since(from);
    match unit {
        IntervalUnit::Second => elapsed.num_seconds(),
        IntervalUnit::Minute => elapsed.num_minutes(),
        IntervalUnit::Hour => elapsed.num_hours(),
        IntervalUnit::Day => elapsed.num_days(),
        IntervalUnit::Month => complete_months(from, to),
        IntervalUnit::Year => complete_months(from, to) / 12,
    }
}

/// Calendar months fully elapsed between two datetimes: the month delta,
/// minus one when the later day-of-month/time has not yet reached the
/// earlier one (`MySQL`'s boundary rule, e.g. Jan 31 -> Feb 29 is 0 months).
fn complete_months(from: NaiveDateTime, to: NaiveDateTime) -> i64 {
    let (early, late, sign) = if to >= from {
        (from, to, 1)
    } else {
        (to, from, -1)
    };
    let mut months = i64::from(late.year() - early.year()) * 12 + i64::from(late.month())
        - i64::from(early.month());
    if (late.day(), late.time()) < (early.day(), early.time()) {
        months -= 1;
    }
    sign * months
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

/// A `CONVERT_TZ` zone argument: numeric offset or IANA name.
enum ZoneSpec {
    Fixed(FixedOffset),
    Named(chrono_tz::Tz),
}

fn timezone_spec(text: &str) -> Option<ZoneSpec> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_prefix('-'))
    {
        let (hours, minutes) = rest.split_once(':')?;
        let hours: i32 = hours.parse().ok()?;
        let minutes: i32 = minutes.parse().ok()?;
        if !(0..=59).contains(&minutes) {
            return None;
        }
        let mut seconds = (hours * 60 + minutes) * 60;
        if trimmed.starts_with('-') {
            seconds = -seconds;
        }
        // MySQL accepts offsets in [-13:59, +14:00].
        if !((-14 * 3600 + 60)..=(14 * 3600)).contains(&seconds) {
            return None;
        }
        return FixedOffset::east_opt(seconds).map(ZoneSpec::Fixed);
    }
    chrono_tz::Tz::from_str_insensitive(trimmed)
        .ok()
        .map(ZoneSpec::Named)
}

/// `CONVERT_TZ` on the canonical datetime text carrier. Ambiguous local
/// times (DST fall-back) take the earlier offset like `MySQL`; nonexistent
/// local times (spring-forward gap) return None, a documented divergence.
fn convert_tz(text: &str, from: &str, to: &str) -> Option<String> {
    let trimmed = text.trim();
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| {
            NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        })
        .ok()?;
    let fraction_digits = trimmed
        .rsplit_once('.')
        .map_or(0, |(_, fraction)| fraction.len().min(6));
    let from = timezone_spec(from)?;
    let to = timezone_spec(to)?;
    let utc = match from {
        ZoneSpec::Fixed(offset) => match offset.from_local_datetime(&naive) {
            LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                value.with_timezone(&Utc)
            }
            LocalResult::None => return None,
        },
        ZoneSpec::Named(zone) => match zone.from_local_datetime(&naive) {
            LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                value.with_timezone(&Utc)
            }
            LocalResult::None => return None,
        },
    };
    let converted = match to {
        ZoneSpec::Fixed(offset) => utc.with_timezone(&offset).naive_local(),
        ZoneSpec::Named(zone) => utc.with_timezone(&zone).naive_local(),
    };
    let base = converted.format("%Y-%m-%d %H:%M:%S").to_string();
    if fraction_digits == 0 {
        return Some(base);
    }
    let micros = format!("{:06}", converted.and_utc().timestamp_subsec_micros());
    Some(format!("{base}.{}", &micros[..fraction_digits]))
}

/// Translates a `MySQL` format string into a chrono *parse* format for
/// `STR_TO_DATE`.
///
/// This is the direction `DATE_FORMAT` used to share, and it carries the same
/// defect: directives outside the mapped set are forwarded to chrono, whose
/// dialect assigns several of the same letters different meanings, so an
/// unmapped directive parses against the wrong field instead of erroring.
/// Emitting output could be fixed by rendering each directive directly;
/// parsing cannot borrow that fix, because it needs a real parser rather than
/// a renderer. Tracked separately — see the `STR_TO_DATE` note in
/// `docs/limitations.md`.
fn chrono_parse_format(value: &str) -> String {
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

const WEEK_MONDAY_FIRST: u32 = 1;
const WEEK_YEAR: u32 = 2;
const WEEK_FIRST_WEEKDAY: u32 = 4;

/// `MySQL`'s mode-to-flag mapping: a mode without `WEEK_MONDAY_FIRST` flips
/// `WEEK_FIRST_WEEKDAY`, which is why modes 0/2 and 1/3 pair up the way they
/// do.
const fn week_mode(mode: u32) -> u32 {
    let format = mode & 7;
    if format & WEEK_MONDAY_FIRST == 0 {
        format ^ WEEK_FIRST_WEEKDAY
    } else {
        format
    }
}

const fn days_in_year(year: i32) -> i32 {
    if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
        366
    } else {
        365
    }
}

/// `MySQL`'s `calc_week`, returning `(year, week)`.
///
/// Ported rather than approximated. The four modes disagree about both the
/// first day of the week and whether week 1 must contain four days of the new
/// year, and chrono's ISO week matches only mode 3 — so `%U %u %V %v` cannot
/// be served by borrowing another library's week number. The paired year
/// (`%X`, `%x`) is why this returns the year too: a date in early January can
/// belong to the last week of the previous year.
fn mysql_calc_week(date: NaiveDate, mode: u32) -> (i32, u32) {
    let flags = week_mode(mode);
    let monday_first = flags & WEEK_MONDAY_FIRST != 0;
    let mut week_year = flags & WEEK_YEAR != 0;
    let first_weekday = flags & WEEK_FIRST_WEEKDAY != 0;

    let daynr = date.num_days_from_ce();
    let mut year = date.year();
    let first = NaiveDate::from_ymd_opt(year, 1, 1).expect("january 1 is valid");
    let mut first_daynr = first.num_days_from_ce();
    // MySQL's `calc_weekday`: 0 is Sunday under a Sunday-first mode and
    // Monday otherwise.
    let mut weekday = if monday_first {
        first.weekday().num_days_from_monday()
    } else {
        first.weekday().num_days_from_sunday()
    };

    if date.month() == 1 && date.day() <= 7 - weekday {
        if !week_year && ((first_weekday && weekday != 0) || (!first_weekday && weekday >= 4)) {
            return (year, 0);
        }
        week_year = true;
        year -= 1;
        let length = days_in_year(year);
        first_daynr -= length;
        weekday = (weekday + 53 * 7 - u32::try_from(length).unwrap_or(365)) % 7;
    }

    let offset = i32::try_from(weekday).unwrap_or(0);
    let days = if (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4) {
        daynr - (first_daynr + (7 - offset))
    } else {
        daynr - (first_daynr - offset)
    };

    if week_year && days >= 52 * 7 {
        weekday = (weekday + u32::try_from(days_in_year(year)).unwrap_or(365)) % 7;
        if (!first_weekday && weekday < 4) || (first_weekday && weekday == 0) {
            return (year + 1, 1);
        }
    }
    (year, u32::try_from(days / 7 + 1).unwrap_or(0))
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// `MySQL`'s ordinal suffix for `%D`: 11th/12th/13th are the exceptions to
/// the last-digit rule.
const fn ordinal_suffix(day: u32) -> &'static str {
    match (day % 100, day % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    }
}

/// Renders one `MySQL` `DATE_FORMAT` directive inventory.
///
/// This used to translate the format string into a chrono format string and
/// hand it over, mapping nine directives and forwarding the rest unchanged.
/// That silently produced wrong output wherever the two dialects use the same
/// letter differently: `%W` returned a week number rather than a weekday
/// name, `%D` returned `02/29/24` rather than `29th`, and `%v` returned a
/// whole formatted date rather than a week number. None of it errored, which
/// broke the rule that a query fails explicitly rather than returning a
/// plausible incompatible result. Emitting directly is the only way to be
/// sure a directive means what `MySQL` says it means.
///
/// Unknown directives copy the bare character, which is `MySQL`'s documented
/// behaviour — `%q` is `q`, not an error.
fn mysql_date_format(value: NaiveDateTime, format: &str) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(format.len());
    let mut characters = format.chars();
    let hour12 = match value.hour() % 12 {
        0 => 12,
        other => other,
    };
    let meridiem = if value.hour() < 12 { "AM" } else { "PM" };
    // Writing into the buffer rather than building a String per directive
    // keeps a row-loop format free of per-directive allocation.
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            output.push('%');
            break;
        };
        let written = match specifier {
            'a' => {
                output.push_str(&WEEKDAYS[value.weekday().num_days_from_monday() as usize][..3]);
                Ok(())
            }
            'b' => {
                output.push_str(&MONTHS[value.month0() as usize][..3]);
                Ok(())
            }
            'c' => write!(output, "{}", value.month()),
            'D' => write!(output, "{}{}", value.day(), ordinal_suffix(value.day())),
            'd' => write!(output, "{:02}", value.day()),
            'e' => write!(output, "{}", value.day()),
            'f' => write!(output, "{:06}", value.and_utc().timestamp_subsec_micros()),
            'H' => write!(output, "{:02}", value.hour()),
            'h' | 'I' => write!(output, "{hour12:02}"),
            'i' => write!(output, "{:02}", value.minute()),
            'j' => write!(output, "{:03}", value.ordinal()),
            'k' => write!(output, "{}", value.hour()),
            'l' => write!(output, "{hour12}"),
            'M' => {
                output.push_str(MONTHS[value.month0() as usize]);
                Ok(())
            }
            'm' => write!(output, "{:02}", value.month()),
            'p' => {
                output.push_str(meridiem);
                Ok(())
            }
            'r' => write!(
                output,
                "{hour12:02}:{:02}:{:02} {meridiem}",
                value.minute(),
                value.second()
            ),
            'S' | 's' => write!(output, "{:02}", value.second()),
            'T' => write!(
                output,
                "{:02}:{:02}:{:02}",
                value.hour(),
                value.minute(),
                value.second()
            ),
            'U' => write!(output, "{:02}", mysql_calc_week(value.date(), 0).1),
            'u' => write!(output, "{:02}", mysql_calc_week(value.date(), 1).1),
            'V' => write!(output, "{:02}", mysql_calc_week(value.date(), 2).1),
            'v' => write!(output, "{:02}", mysql_calc_week(value.date(), 3).1),
            'W' => {
                output.push_str(WEEKDAYS[value.weekday().num_days_from_monday() as usize]);
                Ok(())
            }
            'w' => write!(output, "{}", value.weekday().num_days_from_sunday()),
            'X' => write!(output, "{:04}", mysql_calc_week(value.date(), 2).0),
            'x' => write!(output, "{:04}", mysql_calc_week(value.date(), 3).0),
            'Y' => write!(output, "{:04}", value.year()),
            'y' => write!(output, "{:02}", value.year().rem_euclid(100)),
            '%' => {
                output.push('%');
                Ok(())
            }
            other => {
                output.push(other);
                Ok(())
            }
        };
        // Writing into a String is infallible; the Result exists only because
        // fmt::Write is generic over sinks that can fail.
        debug_assert!(written.is_ok(), "writing into a String cannot fail");
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

/// Renders a parsed datetime at `MySQL`'s fractional-second precision. A
/// zero `fsp` prints no decimal point at all, which is what `MySQL` does for
/// `CAST(x AS DATETIME)` without an explicit precision.
fn format_with_fraction(value: NaiveDateTime, fsp: u8, pattern: &str) -> String {
    let base = value.format(pattern).to_string();
    if fsp == 0 {
        return base;
    }
    let micros = format!("{:06}", value.and_utc().timestamp_subsec_micros());
    format!("{base}.{}", &micros[..usize::from(fsp).min(6)])
}

fn cast_scalar(value: &Value, data_type: Option<DataType>) -> Result<Value, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    // Exact decimal coercion runs before the storage-type collapse: decimals
    // store as canonical text, so collapsing first would lose the scale.
    if let Some(DataType::Decimal { scale, .. }) = data_type {
        return cast_decimal(value, scale);
    }
    // Temporal targets likewise collapse to the Utf8 carrier, so without
    // this they would pass their input through untouched — `CAST(ts AS DATE)`
    // has to truncate the time, not merely relabel the column. MySQL answers
    // NULL for a value it cannot interpret rather than raising.
    match data_type {
        Some(DataType::Date32) => {
            return Ok(parse_mysql_datetime(&scalar_string(value)?)
                .map_or(Value::Null, |parsed| {
                    Value::Utf8(parsed.date().format("%Y-%m-%d").to_string())
                }));
        }
        Some(DataType::DateTime64 { fsp }) => {
            return Ok(parse_mysql_datetime(&scalar_string(value)?)
                .map_or(Value::Null, |parsed| {
                    Value::Utf8(format_with_fraction(parsed, fsp, "%Y-%m-%d %H:%M:%S"))
                }));
        }
        _ => {}
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

/// Decimal-typed division is exact: scaled i128 units with `MySQL`'s
/// half-away-from-zero rounding at the widened result scale. Operands that
/// cannot carry exact units (floats) fall through to the f64 path formatted
/// at the declared scale, so the result type stays canonical.
fn divide_decimal(left: &Value, right: &Value, target: u8) -> Result<Value, ExecError> {
    if let (Some((left_units, left_scale)), Some((right_units, right_scale))) =
        (decimal_units_of(left), decimal_units_of(right))
    {
        if right_units == 0 {
            return Ok(Value::Null);
        }
        // value = (lu/10^ls) / (ru/10^rs); at the target scale the numerator
        // carries 10^(target + rs - ls), which the binder's scale rule keeps
        // non-negative.
        let exponent = u32::from(target)
            .checked_add(u32::from(right_scale))
            .and_then(|sum| sum.checked_sub(u32::from(left_scale)));
        let exact = exponent
            .and_then(|exponent| 10_i128.checked_pow(exponent))
            .and_then(|factor| left_units.checked_mul(factor))
            .and_then(|numerator| pintail_types::div_decimal_round_half_up(numerator, right_units));
        if let Some(units) = exact {
            return Ok(Value::Utf8(pintail_types::format_decimal_scaled(
                units, target,
            )));
        }
    }
    let dividend = mysql_f64(left)?;
    let divisor = mysql_f64(right)?;
    if divisor == 0.0 {
        return Ok(Value::Null);
    }
    cast_decimal(&Value::float64(dividend / divisor), target)
}

/// Exact decimal addition, subtraction, and multiplication on scaled i128
/// units; falls back to the f64 carrier formatted at the target scale when
/// a value cannot carry exact units or the units overflow.
fn decimal_add_sub_mul(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    target: u8,
) -> Result<Value, ExecError> {
    if let (Some((left_units, left_scale)), Some((right_units, right_scale))) =
        (decimal_units_of(left), decimal_units_of(right))
    {
        let exact = if op == BinaryOp::Multiply {
            // Product scale is the sum of operand scales; the binder set
            // target accordingly (capped), so rescale when it saturated.
            let natural = left_scale.saturating_add(right_scale);
            left_units.checked_mul(right_units).and_then(|product| {
                if natural <= target {
                    10_i128
                        .checked_pow(u32::from(target - natural))
                        .and_then(|factor| product.checked_mul(factor))
                } else {
                    // Cap hit: round down to the target scale.
                    10_i128
                        .checked_pow(u32::from(natural - target))
                        .and_then(|factor| {
                            pintail_types::div_decimal_round_half_up(product, factor)
                        })
                }
            })
        } else {
            let rescale = |units: i128, scale: u8| {
                (scale <= target)
                    .then(|| {
                        10_i128
                            .checked_pow(u32::from(target - scale))
                            .and_then(|factor| units.checked_mul(factor))
                    })
                    .flatten()
            };
            match (
                rescale(left_units, left_scale),
                rescale(right_units, right_scale),
            ) {
                (Some(left), Some(right)) => {
                    if op == BinaryOp::Add {
                        left.checked_add(right)
                    } else {
                        left.checked_sub(right)
                    }
                }
                _ => None,
            }
        };
        if let Some(units) = exact {
            return Ok(Value::Utf8(pintail_types::format_decimal_scaled(
                units, target,
            )));
        }
    }
    let left = mysql_f64(left)?;
    let right = mysql_f64(right)?;
    let value = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        _ => left * right,
    };
    if !value.is_finite() {
        return Err(ExecError::NumericOverflow);
    }
    cast_decimal(&Value::float64(value), target)
}

/// Coerces a value to canonical decimal text at `scale`, rounding half away
/// from zero like `MySQL`. Floats format at the target scale first (their
/// tie-rounding is the platform's, an accepted v1 edge).
fn cast_decimal(value: &Value, scale: u8) -> Result<Value, ExecError> {
    let units = match value {
        Value::Utf8(text) => pintail_types::parse_decimal_rounded(text, scale),
        Value::Boolean(flag) => decimal_units_from_i128(i128::from(*flag), scale),
        Value::Int64(signed) => decimal_units_from_i128(i128::from(*signed), scale),
        Value::UInt64(unsigned) => decimal_units_from_i128(i128::from(*unsigned), scale),
        Value::Float64(_) => {
            let float = mysql_f64(value)?;
            if !float.is_finite() {
                return Err(ExecError::NumericOverflow);
            }
            pintail_types::parse_decimal_rounded(
                &format!(
                    "{float:.precision$}",
                    precision = usize::from(scale).saturating_add(1)
                ),
                scale,
            )
        }
        Value::Binary(bytes) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| pintail_types::parse_decimal_rounded(text, scale)),
        Value::Null => return Ok(Value::Null),
    };
    units
        .map(|units| Value::Utf8(pintail_types::format_decimal_scaled(units, scale)))
        .ok_or(ExecError::NumericOverflow)
}

fn decimal_units_from_i128(value: i128, scale: u8) -> Option<i128> {
    value.checked_mul(10_i128.checked_pow(u32::from(scale))?)
}

/// Compares two decimal-typed operands numerically on scaled units,
/// falling back to the f64 carrier when a value cannot carry exact units.
fn compare_decimal_values(left: &Value, right: &Value) -> Result<Ordering, ExecError> {
    if let (Some((left_units, left_scale)), Some((right_units, right_scale))) =
        (decimal_units_of(left), decimal_units_of(right))
    {
        let common = left_scale.max(right_scale);
        let rescale = |units: i128, scale: u8| {
            10_i128
                .checked_pow(u32::from(common - scale))
                .and_then(|factor| units.checked_mul(factor))
        };
        if let (Some(left), Some(right)) = (
            rescale(left_units, left_scale),
            rescale(right_units, right_scale),
        ) {
            return Ok(left.cmp(&right));
        }
    }
    Ok(mysql_f64(left)?.total_cmp(&mysql_f64(right)?))
}

/// Splits a value into scaled integer units and the scale it naturally
/// carries: canonical decimal text keeps its written fraction width,
/// integers are scale zero. `None` for floats and non-numeric text.
fn decimal_units_of(value: &Value) -> Option<(i128, u8)> {
    match value {
        Value::Utf8(text) => {
            let fraction = text
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len());
            let scale = u8::try_from(fraction).ok()?;
            if scale > 30 {
                return None;
            }
            pintail_types::parse_decimal_scaled(text, scale).map(|units| (units, scale))
        }
        Value::Boolean(flag) => Some((i128::from(*flag), 0)),
        Value::Int64(signed) => Some((i128::from(*signed), 0)),
        Value::UInt64(unsigned) => Some((i128::from(*unsigned), 0)),
        _ => None,
    }
}

/// `REPEAT`/`SPACE`/pad results are capped at 4096 bytes
/// (`docs/limitations.md`); `MySQL`'s cap is `max_allowed_packet`.
const STRING_BUILD_CAP: usize = 4096;

fn repeat_capped(text: &str, count: i64) -> Result<Value, ExecError> {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let bytes = text.len().saturating_mul(count);
    if bytes > STRING_BUILD_CAP {
        return Err(ExecError::NumericOverflow);
    }
    Ok(Value::Utf8(text.repeat(count)))
}

fn mysql_pad(text: &str, target: i64, pad: &str, left: bool) -> Result<Value, ExecError> {
    if target < 0 {
        return Ok(Value::Null);
    }
    let target = usize::try_from(target).unwrap_or(usize::MAX);
    if target > STRING_BUILD_CAP {
        return Err(ExecError::NumericOverflow);
    }
    let length = text.chars().count();
    if target <= length {
        return Ok(Value::Utf8(text.chars().take(target).collect()));
    }
    if pad.is_empty() {
        // MySQL returns NULL when padding is required but empty.
        return Ok(Value::Null);
    }
    let filler = pad
        .chars()
        .cycle()
        .take(target - length)
        .collect::<String>();
    Ok(Value::Utf8(if left {
        format!("{filler}{text}")
    } else {
        format!("{text}{filler}")
    }))
}

fn hex_upper(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }
    output
}

/// `MySQL` `UNHEX`: decode hex pairs, `NULL` on any non-hex character.
///
/// This decodes bytes rather than string slices. Slicing the padded `String`
/// at fixed two-byte offsets panicked whenever the argument held a multibyte
/// character whose encoding straddled an offset — `UNHEX('éa')` pads to
/// `"0éa"` and then cuts `é` in half, which is a panic reachable from any
/// client query rather than the documented `NULL`.
fn unhex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len().div_ceil(2));
    // An odd-length argument is left-padded with a zero nibble, so seeding
    // the high nibble with zero makes the first digit complete a byte.
    let mut high = (bytes.len() % 2 != 0).then_some(0_u8);
    for byte in bytes {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        match high {
            None => high = Some(nibble),
            Some(leading) => {
                output.push((leading << 4) | nibble);
                high = None;
            }
        }
    }
    Some(output)
}

/// `en_US` thousands grouping with fixed fraction digits, `MySQL` `FORMAT`.
fn format_grouped(value: f64, digits: usize) -> String {
    let formatted = format!("{value:.digits$}");
    let (sign, rest) = formatted
        .strip_prefix('-')
        .map_or(("", formatted.as_str()), |rest| ("-", rest));
    let (integer, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3);
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if fraction.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{fraction}")
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * index);
        }
        for position in 0..4 {
            if position <= chunk.len() {
                let index = usize::try_from((word >> (18 - 6 * position)) & 0x3F)
                    .expect("six bits fit usize");
                output.push(char::from(BASE64_ALPHABET[index]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    // MySQL skips whitespace between encoded groups, and its notion of
    // whitespace includes the vertical tab. Rust's `is_ascii_whitespace`
    // follows the WhatWG set, which deliberately excludes it — so
    // FROM_BASE64 rejected a vertical tab that MySQL accepts. Measured, not
    // assumed: MySQL answers 61 for CHAR(11) between the group and the end.
    let cleaned = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != 0x0B)
        .collect::<Vec<_>>();
    if cleaned.len() % 4 != 0 {
        return None;
    }
    let value_of = |byte: u8| -> Option<u32> {
        BASE64_ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .and_then(|position| u32::try_from(position).ok())
    };
    let mut output = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || chunk[..4 - padding].contains(&b'=') {
            return None;
        }
        let mut word = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            let bits = if *byte == b'=' { 0 } else { value_of(*byte)? };
            word |= bits << (18 - 6 * index);
        }
        for position in 0..(3 - padding) {
            output.push(
                u8::try_from((word >> (16 - 8 * position)) & 0xFF).expect("eight bits fit u8"),
            );
        }
    }
    Some(output)
}

// Compiled regex cache: patterns and match flags are almost always literals,
// so each worker thread compiles a combination once. Case-insensitive by
// default to match the ci collations (docs/limitations.md).
thread_local! {
    static REGEX_CACHE: std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<regex::Regex>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Whether any operand is a binary string, which makes `MySQL` compare the
/// pair case-sensitively rather than under a collation.
fn binary_operand(values: &[Value]) -> bool {
    values.iter().any(|value| matches!(value, Value::Binary(_)))
}

/// Case-folds for the engine's case-insensitive default, unless a binary
/// operand demands an exact byte comparison.
fn fold_unless_binary(text: &str, binary: bool) -> String {
    if binary {
        text.to_owned()
    } else {
        text.to_lowercase()
    }
}

/// Rewrites POSIX bracket classes to their Unicode equivalents.
///
/// `MySQL` 8.4 runs ICU, where `[[:alpha:]]` matches any alphabetic character;
/// Rust's regex crate defines the POSIX classes over ASCII, so
/// `REGEXP_LIKE('e-acute', '[[:alpha:]]')` answered 0 where `MySQL` answers 1.
/// Only the classes whose ASCII and Unicode definitions actually differ are
/// rewritten; `[:xdigit:]` and the punctuation classes are ASCII in both.
fn unicode_posix_classes(pattern: &str) -> String {
    const CLASSES: [(&str, &str); 7] = [
        ("[:alpha:]", "\\p{Alphabetic}"),
        ("[:alnum:]", "\\p{Alphabetic}\\p{Nd}"),
        ("[:digit:]", "\\p{Nd}"),
        ("[:lower:]", "\\p{Lowercase}"),
        ("[:upper:]", "\\p{Uppercase}"),
        ("[:space:]", "\\s"),
        ("[:word:]", "\\w"),
    ];
    if !pattern.contains("[:") {
        return pattern.to_owned();
    }
    let mut rewritten = pattern.to_owned();
    for (posix, unicode) in CLASSES {
        if rewritten.contains(posix) {
            rewritten = rewritten.replace(posix, unicode);
        }
    }
    rewritten
}

fn compiled_regex(pattern: &str) -> Result<std::rc::Rc<regex::Regex>, ExecError> {
    compiled_regex_with_match_type(pattern, "")
}

/// ICU treats CR, CRLF, NEL, LS, and PS as line endings by default. Rust's
/// regex engine treats only LF specially, so normalize those sequences for
/// `REGEXP_LIKE` unless `MySQL`'s `u` (Unix-lines-only) match flag is present.
fn normalize_mysql_regex_line_endings(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalized.push('\n');
            }
            '\u{0085}' | '\u{2028}' | '\u{2029}' => normalized.push('\n'),
            other => normalized.push(other),
        }
    }
    normalized
}

fn compiled_regex_with_match_type(
    pattern: &str,
    match_type: &str,
) -> Result<std::rc::Rc<regex::Regex>, ExecError> {
    let mut case_insensitive = true;
    let mut multi_line = false;
    let mut dot_matches_new_line = false;
    for option in match_type.chars() {
        match option {
            // When c and i conflict, MySQL lets the rightmost flag win.
            'c' => case_insensitive = false,
            'i' => case_insensitive = true,
            'm' => multi_line = true,
            'n' => dot_matches_new_line = true,
            // Subject normalization, not compilation, implements this flag.
            'u' => {}
            _ => return Err(ExecError::InvalidExpressionType),
        }
    }
    let cache_key = format!(
        "{}{}{}\0{pattern}",
        u8::from(case_insensitive),
        u8::from(multi_line),
        u8::from(dot_matches_new_line)
    );
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(compiled) = cache.get(&cache_key) {
            return Ok(std::rc::Rc::clone(compiled));
        }
        let translated = unicode_posix_classes(pattern);
        let compiled = std::rc::Rc::new(
            regex::RegexBuilder::new(&translated)
                .case_insensitive(case_insensitive)
                .multi_line(multi_line)
                .dot_matches_new_line(dot_matches_new_line)
                .size_limit(1 << 20)
                .build()
                .map_err(|_| ExecError::InvalidExpressionType)?,
        );
        if cache.len() >= 256
            && let Some(evicted) = cache.keys().next().cloned()
        {
            cache.remove(&evicted);
        }
        cache.insert(cache_key, std::rc::Rc::clone(&compiled));
        Ok(compiled)
    })
}

/// Exact CEIL/FLOOR of canonical decimal text: the integer part, adjusted
/// by one when a fractional remainder exists in the rounding direction.
fn decimal_integer_bound(text: &str, ceiling: bool) -> Result<Value, ExecError> {
    let (whole, fraction) = text.rsplit_once('.').unwrap_or((text, ""));
    let has_fraction = fraction.bytes().any(|byte| byte != b'0');
    let mut integer: i64 = whole.parse().map_err(|_| ExecError::NumericOverflow)?;
    if has_fraction {
        let negative = text.starts_with('-');
        if ceiling && !negative {
            integer = integer.checked_add(1).ok_or(ExecError::NumericOverflow)?;
        }
        if !ceiling && negative {
            integer = integer.checked_sub(1).ok_or(ExecError::NumericOverflow)?;
        }
    }
    Ok(Value::Int64(integer))
}

/// Renders a JSON value the way `MySQL` prints JSON columns: `", "`
/// between members, `": "` after object keys, and object keys ordered by
/// length then bytes (the binary-JSON normalization order).
pub(crate) fn mysql_json_text(value: &serde_json::Value) -> String {
    fn write(value: &serde_json::Value, output: &mut String) {
        match value {
            serde_json::Value::Array(items) => {
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    write(item, output);
                }
                output.push(']');
            }
            serde_json::Value::Object(members) => {
                let mut keys: Vec<&String> = members.keys().collect();
                keys.sort_by(|left, right| {
                    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                });
                output.push('{');
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    output.push_str(&serde_json::Value::String((*key).clone()).to_string());
                    output.push_str(": ");
                    write(&members[*key], output);
                }
                output.push('}');
            }
            other => output.push_str(&other.to_string()),
        }
    }
    let mut output = String::new();
    write(value, &mut output);
    output
}

/// Maps a SQL value to the JSON value `MySQL` would store for it inside
/// `JSON_OBJECT`/`JSON_ARRAY`: NULL becomes JSON null, numbers stay
/// numbers, everything else is a JSON string.
pub(crate) fn json_value_of(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        // BOOLEAN is TINYINT(1) in MySQL, which JSON renders numerically.
        Value::Boolean(inner) => serde_json::Value::from(i64::from(*inner)),
        Value::Int64(inner) => serde_json::Value::from(*inner),
        Value::UInt64(inner) => serde_json::Value::from(*inner),
        Value::Float64(inner) => serde_json::Value::from(inner.get()),
        Value::Utf8(inner) => serde_json::Value::String(inner.clone()),
        Value::Binary(inner) => {
            serde_json::Value::String(String::from_utf8_lossy(inner).into_owned())
        }
    }
}

/// Converts a physical value using its retained logical SQL type. JSON and
/// equal-looking VARCHAR share the UTF-8 carrier, so only this metadata tells
/// constructors whether to embed a document or quote ordinary text.
pub(crate) fn json_value_of_typed(
    value: &Value,
    data_type: Option<DataType>,
) -> Result<serde_json::Value, ExecError> {
    if data_type == Some(DataType::Json) && !matches!(value, Value::Null) {
        return serde_json::from_str(&scalar_string(value)?)
            .map_err(|_| ExecError::InvalidExpressionType);
    }
    Ok(json_value_of(value))
}

/// Resolves a `MySQL` JSON path of the `$.key.nested[0]` form. Wildcards
/// and range selectors are unsupported and error explicitly.
/// `MySQL` `SUBSTRING_INDEX`: everything before the `count`-th delimiter from
/// the left, or after it from the right when `count` is negative. Fewer
/// occurrences than requested returns the whole subject rather than NULL,
/// which is what makes it usable for URL and UTM splitting.
fn substring_index(text: &str, delimiter: &str, count: i64) -> String {
    if delimiter.is_empty() || count == 0 {
        return String::new();
    }
    if count > 0 {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX);
        let mut end = 0;
        for taken in 0..wanted {
            match text[end..].find(delimiter) {
                Some(offset) => {
                    if taken + 1 == wanted {
                        return text[..end + offset].to_owned();
                    }
                    end += offset + delimiter.len();
                }
                None => return text.to_owned(),
            }
        }
        return text.to_owned();
    }
    let wanted = usize::try_from(count.unsigned_abs()).unwrap_or(usize::MAX);
    let mut start = text.len();
    for taken in 0..wanted {
        match text[..start].rfind(delimiter) {
            Some(offset) => {
                if taken + 1 == wanted {
                    return text[offset + delimiter.len()..].to_owned();
                }
                start = offset;
            }
            None => return text.to_owned(),
        }
    }
    text.to_owned()
}

/// `MySQL` `CONV`: re-base an integer between bases 2..=36. A negative target
/// base asks for a signed reading; otherwise the value is unsigned 64-bit.
/// Parsing stops at the first digit invalid for the source base, matching
/// `MySQL` rather than rejecting the whole string.
fn conv_base(subject: &str, from: i64, to: i64) -> Option<String> {
    // Range-check before abs(): i64::MIN has no positive counterpart, so
    // abs() on it overflows — a panic in debug, a wrap in release.
    if !(2..=36).contains(&from.unsigned_abs()) || !(2..=36).contains(&to.unsigned_abs()) {
        return None;
    }
    let from_base = u32::try_from(from.unsigned_abs()).ok()?;
    let signed_output = to < 0;
    let to_base = u32::try_from(to.unsigned_abs()).ok()?;
    let trimmed = subject.trim();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let mut magnitude = 0_u64;
    for character in digits.chars() {
        let Some(digit) = character.to_digit(from_base) else {
            break;
        };
        // MySQL saturates an overlong source at the unsigned ceiling rather
        // than wrapping to a small number.
        magnitude = magnitude
            .checked_mul(u64::from(from_base))
            .and_then(|scaled| scaled.checked_add(u64::from(digit)))
            .unwrap_or(u64::MAX);
    }
    // A leading minus wraps in the 64-bit space, exactly as MySQL's
    // unsigned reading does; these are reinterpretations, not conversions.
    let value = if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    };
    let as_signed = i64::from_ne_bytes(value.to_ne_bytes());
    let (sign, mut remaining) = if signed_output && as_signed < 0 {
        ("-", as_signed.unsigned_abs())
    } else {
        ("", value)
    };
    if remaining == 0 {
        return Some("0".to_owned());
    }
    let mut rendered = Vec::new();
    while remaining > 0 {
        let digit = u32::try_from(remaining % u64::from(to_base)).ok()?;
        rendered.push(char::from_digit(digit, to_base)?.to_ascii_uppercase());
        remaining /= u64::from(to_base);
    }
    rendered.reverse();
    Some(format!(
        "{sign}{}",
        rendered.into_iter().collect::<String>()
    ))
}

/// `MySQL` `MAKETIME`. Built by formatting rather than through a clock type:
/// `MySQL`'s TIME spans -838:59:59..=838:59:59, which no civil-time type
/// represents, and out-of-range hours clamp to that boundary.
fn make_time(hour: i64, minute: i64, second: i64) -> Option<String> {
    if !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    let negative = hour < 0;
    let magnitude = hour.unsigned_abs();
    let (hours, minutes, seconds) = if magnitude > 838 {
        (838, 59, 59)
    } else {
        (magnitude, minute.unsigned_abs(), second.unsigned_abs())
    };
    let sign = if negative { "-" } else { "" };
    Some(format!("{sign}{hours:02}:{minutes:02}:{seconds:02}"))
}

/// Parses a JSON-valued argument, raising rather than guessing when the text
/// is not JSON — every function except `JSON_VALID` requires a real document.
fn parse_json_argument(value: &Value) -> Result<serde_json::Value, ExecError> {
    serde_json::from_str::<serde_json::Value>(&scalar_string(value)?)
        .map_err(|_| ExecError::InvalidExpressionType)
}

/// `MySQL` `JSON_TYPE` names. The engine has no typed JSON carrier, so an
/// integral number reports INTEGER and everything else numeric reports
/// DOUBLE; `MySQL`'s DECIMAL, DATE and BLOB categories need typed JSON
/// storage to distinguish and are recorded as a gap.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "NULL",
        serde_json::Value::Bool(_) => "BOOLEAN",
        serde_json::Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "INTEGER"
            } else {
                "DOUBLE"
            }
        }
        serde_json::Value::String(_) => "STRING",
        serde_json::Value::Array(_) => "ARRAY",
        serde_json::Value::Object(_) => "OBJECT",
    }
}

/// `MySQL` `JSON_CONTAINS` containment, which is asymmetric and recursive:
/// an array contains a non-array candidate when any element contains it, an
/// object contains an object when every candidate key is present and its
/// value contained, and scalars must be equal.
fn json_contains(target: &serde_json::Value, candidate: &serde_json::Value) -> bool {
    match (target, candidate) {
        (serde_json::Value::Array(items), serde_json::Value::Array(wanted)) => wanted
            .iter()
            .all(|entry| items.iter().any(|item| json_contains(item, entry))),
        (serde_json::Value::Array(items), _) => {
            items.iter().any(|item| json_contains(item, candidate))
        }
        (serde_json::Value::Object(members), serde_json::Value::Object(wanted)) => {
            wanted.iter().all(|(key, entry)| {
                members
                    .get(key)
                    .is_some_and(|member| json_contains(member, entry))
            })
        }
        (left, right) => left == right,
    }
}

/// Collects the paths of every string in `document` matching `pattern`.
///
/// `MySQL` matches with `LIKE` semantics — `%` for any run, `_` for one
/// character — against string values only; numbers and booleans never match.
/// Search stops at the first hit unless `all` is set.
fn json_search(
    document: &serde_json::Value,
    pattern: &str,
    escape: Option<char>,
    all: bool,
    here: &str,
    found: &mut Vec<String>,
) {
    if !all && !found.is_empty() {
        return;
    }
    match document {
        serde_json::Value::String(text) => {
            if like_matches(&text.to_lowercase(), &pattern.to_lowercase(), escape) {
                found.push(here.to_owned());
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                json_search(
                    item,
                    pattern,
                    escape,
                    all,
                    &format!("{here}[{index}]"),
                    found,
                );
                if !all && !found.is_empty() {
                    return;
                }
            }
        }
        serde_json::Value::Object(members) => {
            for (key, value) in members {
                // A key needing quotes in a path gets them, so the answer can
                // be fed straight back into JSON_EXTRACT.
                let step = if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    format!("{here}.{key}")
                } else {
                    format!("{here}.\"{key}\"")
                };
                json_search(value, pattern, escape, all, &step, found);
                if !all && !found.is_empty() {
                    return;
                }
            }
        }
        _ => {}
    }
}

/// One step of a `MySQL` JSON path: an object member or an array position.
#[derive(Clone, Debug, PartialEq, Eq)]
enum JsonStep {
    Member(String),
    Index(usize),
}

/// Parses `$.a[0].b` into its steps. Every JSON function that walks a path
/// goes through this, so a path cannot mean one thing in one function and
/// something else in another.
fn json_path_steps(path: &str) -> Result<Vec<JsonStep>, ExecError> {
    let rest = path
        .strip_prefix('$')
        .ok_or(ExecError::InvalidExpressionType)?;
    let mut steps = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(step) = chars.next() {
        match step {
            '.' => {
                let mut key = String::new();
                if chars.peek() == Some(&'"') {
                    chars.next();
                    for inner in chars.by_ref() {
                        if inner == '"' {
                            break;
                        }
                        key.push(inner);
                    }
                } else {
                    while let Some(&next) = chars.peek() {
                        if next == '.' || next == '[' {
                            break;
                        }
                        key.push(next);
                        chars.next();
                    }
                }
                // A wildcard selects many targets, which a single-target
                // mutation cannot express; rejecting beats picking one.
                if key.is_empty() || key == "*" {
                    return Err(ExecError::InvalidExpressionType);
                }
                steps.push(JsonStep::Member(key));
            }
            '[' => {
                let mut digits = String::new();
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                    digits.push(inner);
                }
                steps.push(JsonStep::Index(
                    digits
                        .trim()
                        .parse()
                        .map_err(|_| ExecError::InvalidExpressionType)?,
                ));
            }
            _ => return Err(ExecError::InvalidExpressionType),
        }
    }
    Ok(steps)
}

fn json_path_lookup<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Result<Option<&'a serde_json::Value>, ExecError> {
    let mut current = document;
    for step in json_path_steps(path)? {
        let next = match step {
            JsonStep::Member(key) => current.get(&key),
            JsonStep::Index(index) => current.get(index),
        };
        match next {
            Some(value) => current = value,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
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

/// One-based character position of `needle` in `haystack`, or 0.
///
/// Both sides arrive already case-folded or deliberately not: a binary
/// operand must compare byte-exact, and folding the haystack here defeated
/// that decision no matter what the caller passed.
fn locate(needle: &str, haystack: &str, start: i64) -> u64 {
    if start <= 0 {
        return 0;
    }
    let start = usize::try_from(start - 1).unwrap_or(usize::MAX);
    let haystack_lower = haystack.to_owned();
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
        // A decimal travels on the canonical-text carrier, so the storage
        // collapse below would send it to the Utf8 arm and reject it.
        // Negating the text keeps the value exact, which is the whole point
        // of carrying decimals as text.
        UnaryOp::Minus if matches!(data_type, Some(DataType::Decimal { .. })) => {
            let text = scalar_string(value)?;
            let negated = match text.strip_prefix('-') {
                Some(positive) => positive.to_owned(),
                None => format!("-{text}"),
            };
            Ok(Value::Utf8(negated))
        }
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
    if crate::execution::accent_insensitive_collation() {
        return crate::execution::normalized_collation_text(left)
            .cmp(&crate::execution::normalized_collation_text(right));
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
    if let Some(DataType::Decimal { scale: target, .. }) = data_type {
        match op {
            BinaryOp::Divide => return divide_decimal(left, right, target),
            BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply => {
                return decimal_add_sub_mul(op, left, right, target);
            }
            _ => {}
        }
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

    use pintail_sql::{BinaryOp, DatePart, ScalarFunction};
    use pintail_types::{DataType, Value};

    use super::{CompiledExpr, compare_mysql, date_part, mysql_date_format, parse_mysql_number};

    /// The directives that used to be forwarded to chrono, where the same
    /// letters mean something else. Every expectation here is `MySQL` 8.4
    /// behaviour for a Thursday; before the rewrite `%W` returned `09`, `%D`
    /// returned `02/29/24` and `%v` returned `29-Feb-2024`, all without an
    /// error.
    #[test]
    fn date_format_directives_mean_what_mysql_says() {
        let value = chrono::NaiveDate::from_ymd_opt(2024, 2, 29)
            .expect("leap day")
            .and_hms_opt(12, 34, 56)
            .expect("time");
        for (format, expected) in [
            ("%W", "Thursday"),
            ("%a", "Thu"),
            ("%D", "29th"),
            ("%w", "4"),
            ("%M", "February"),
            ("%b", "Feb"),
            ("%j", "060"),
            ("%U", "08"),
            ("%u", "09"),
            ("%V", "08"),
            ("%v", "09"),
            ("%X", "2024"),
            ("%x", "2024"),
            ("%Y", "2024"),
            ("%y", "24"),
            ("%T", "12:34:56"),
            ("%r", "12:34:56 PM"),
            ("%p", "PM"),
            ("%c", "2"),
            ("%e", "29"),
            ("%d", "29"),
            ("%m", "02"),
            ("%H", "12"),
            ("%k", "12"),
            ("%i", "34"),
            ("%s", "56"),
            ("%Y-%m-%d %H:%i:%s", "2024-02-29 12:34:56"),
        ] {
            assert_eq!(
                mysql_date_format(value, format),
                expected,
                "DATE_FORMAT(…, '{format}')"
            );
        }
    }

    /// Midnight and noon are where a 12-hour clock goes wrong, and the
    /// ordinal suffix has three exceptions that a last-digit rule misses.
    #[test]
    fn date_format_handles_the_clock_and_suffix_edges() {
        let midnight = chrono::NaiveDate::from_ymd_opt(2024, 1, 11)
            .expect("date")
            .and_hms_opt(0, 5, 0)
            .expect("time");
        assert_eq!(
            mysql_date_format(midnight, "%h %l %p %k %H"),
            "12 12 AM 0 00"
        );
        // 11th/12th/13th are "th" despite ending in 1, 2, 3.
        for (day, expected) in [
            (1, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (31, "31st"),
        ] {
            let value = chrono::NaiveDate::from_ymd_opt(2024, 1, day)
                .expect("date")
                .and_hms_opt(0, 0, 0)
                .expect("time");
            assert_eq!(mysql_date_format(value, "%D"), expected);
        }
    }

    /// Three divergences measured against `MySQL` 8.4 and now repaired.
    #[test]
    #[allow(clippy::too_many_lines)] // a flat list of measured cases reads best whole
    fn repaired_divergences_match_mysql() {
        use pintail_types::{DataType, Value};
        let call = |function, args: Vec<Value>| {
            super::evaluate_eager_scalar(function, &args, Some(DataType::Utf8)).expect("evaluate")
        };
        // TRIM removes the space character only, not the tab.
        println!(
            "probe trim space = {:?}",
            call(ScalarFunction::Trim, vec![Value::Utf8("  a  ".into())])
        );
        assert_eq!(
            call(ScalarFunction::Trim, vec![Value::Utf8("\t".into())]),
            Value::Utf8("\t".into())
        );
        assert_eq!(
            call(ScalarFunction::Trim, vec![Value::Utf8("  a  ".into())]),
            Value::Utf8("a".into())
        );
        assert_eq!(
            call(ScalarFunction::Trim, vec![Value::Utf8("\ta ".into())]),
            Value::Utf8("\ta".into())
        );
        // TO_BASE64 wraps every 76 characters: 58 bytes -> 81 characters.
        let Value::Utf8(encoded) =
            call(ScalarFunction::ToBase64, vec![Value::Utf8("a".repeat(58))])
        else {
            panic!("expected text");
        };
        assert_eq!(encoded.chars().count(), 81);
        assert!(encoded.contains('\n'));
        // CONV must range-check before abs(): i64::MIN has no positive
        // counterpart, so abs() on it overflows.
        assert_eq!(
            super::conv_base("1", i64::MIN, 10),
            None,
            "CONV with an i64::MIN base"
        );
        assert_eq!(super::conv_base("1", 10, i64::MIN), None);
        assert_eq!(super::conv_base("ff", 16, 10), Some("255".to_owned()));
        // An overlong source saturates rather than wrapping to zero.
        assert_eq!(
            super::conv_base("10000000000000000", 16, 10),
            Some("18446744073709551615".to_owned())
        );
        // MAKETIME keeps a fractional second.
        assert_eq!(
            call(
                ScalarFunction::MakeTime,
                vec![
                    Value::Int64(12),
                    Value::Int64(15),
                    Value::Utf8("30.5".into())
                ]
            ),
            Value::Utf8("12:15:30.5".into())
        );
        // A binary operand is compared byte-exact: LOWER/UPPER leave it
        // alone and INSTR/LOCATE stop folding case.
        assert_eq!(
            call(ScalarFunction::Lower, vec![Value::Binary(b"ABC".to_vec())]),
            Value::Binary(b"ABC".to_vec())
        );
        assert_eq!(
            call(ScalarFunction::Upper, vec![Value::Binary(b"abc".to_vec())]),
            Value::Binary(b"abc".to_vec())
        );
        assert_eq!(
            call(
                ScalarFunction::Instr,
                vec![Value::Binary(b"A".to_vec()), Value::Utf8("a".into())]
            ),
            Value::Utf8("0".into())
        );
        assert_eq!(
            call(
                ScalarFunction::Locate,
                vec![Value::Utf8("a".into()), Value::Binary(b"A".to_vec())]
            ),
            Value::Utf8("0".into())
        );
        // Text operands keep the case-insensitive default.
        assert_eq!(
            call(
                ScalarFunction::Instr,
                vec![Value::Utf8("A".into()), Value::Utf8("a".into())]
            ),
            Value::Utf8("1".into())
        );
        // POSIX classes follow ICU's Unicode definitions, not ASCII.
        assert_eq!(
            call(
                ScalarFunction::RegexpLike { negated: false },
                vec![
                    Value::Utf8("\u{e9}".into()),
                    Value::Utf8("[[:alpha:]]".into())
                ]
            ),
            Value::Utf8("1".into())
        );
        assert_eq!(
            call(
                ScalarFunction::RegexpLike { negated: false },
                vec![Value::Utf8("a".into()), Value::Utf8("[[:alpha:]]".into())]
            ),
            Value::Utf8("1".into())
        );
        assert_eq!(
            call(
                ScalarFunction::RegexpLike { negated: false },
                vec![Value::Utf8("1".into()), Value::Utf8("[[:alpha:]]".into())]
            ),
            Value::Utf8("0".into())
        );
        // SEC_TO_TIME keeps the argument's fraction.
        assert_eq!(
            call(ScalarFunction::SecToTime, vec![Value::Utf8("1.5".into())]),
            Value::Utf8("00:00:01.5".into())
        );
        assert_eq!(
            call(ScalarFunction::SecToTime, vec![Value::Int64(1)]),
            Value::Utf8("00:00:01".into())
        );
    }

    #[test]
    fn md5_matches_mysql_known_vectors() {
        use pintail_types::{DataType, Value};

        let digest = |value| {
            super::evaluate_eager_scalar(ScalarFunction::Md5, &[value], Some(DataType::Utf8))
                .expect("MD5 evaluates")
        };
        assert_eq!(
            digest(Value::Utf8(String::new())),
            Value::Utf8("d41d8cd98f00b204e9800998ecf8427e".to_owned())
        );
        assert_eq!(
            digest(Value::Utf8("abc".to_owned())),
            Value::Utf8("900150983cd24fb0d6963f7d28e17f72".to_owned())
        );
        assert_eq!(digest(Value::Null), Value::Null);
    }

    #[test]
    fn json_extract_and_regexp_like_honor_optional_arguments() {
        use pintail_types::{DataType, Value};

        let json = super::evaluate_eager_scalar(
            ScalarFunction::JsonExtract { unquote: false },
            &[
                Value::Utf8(r#"{"a":1,"b":"x"}"#.to_owned()),
                Value::Utf8("$.a".to_owned()),
                Value::Utf8("$.b".to_owned()),
            ],
            Some(DataType::Utf8),
        )
        .expect("multi-path JSON_EXTRACT evaluates");
        assert_eq!(json, Value::Utf8(r#"[1, "x"]"#.to_owned()));

        let regexp = |text: &str, pattern: &str, match_type: &str| {
            super::evaluate_eager_scalar(
                ScalarFunction::RegexpLike { negated: false },
                &[
                    Value::Utf8(text.to_owned()),
                    Value::Utf8(pattern.to_owned()),
                    Value::Utf8(match_type.to_owned()),
                ],
                Some(DataType::Boolean),
            )
            .expect("REGEXP_LIKE evaluates")
        };
        assert_eq!(regexp("Abc", "abc", "c"), Value::Boolean(false));
        assert_eq!(regexp("Abc", "abc", "ci"), Value::Boolean(true));
        assert_eq!(regexp("a\nb", "^b$", "m"), Value::Boolean(true));
        assert_eq!(regexp("a\nb", "a.b", "n"), Value::Boolean(true));
        assert_eq!(regexp("a\rb", "^b$", "m"), Value::Boolean(true));
        assert_eq!(regexp("a\rb", "^b$", "mu"), Value::Boolean(false));
        assert!(
            super::evaluate_eager_scalar(
                ScalarFunction::RegexpLike { negated: false },
                &[
                    Value::Utf8("abc".to_owned()),
                    Value::Utf8("abc".to_owned()),
                    Value::Utf8("x".to_owned()),
                ],
                Some(DataType::Boolean),
            )
            .is_err()
        );
    }

    #[test]
    fn regex_cache_drops_evicted_programs() {
        super::REGEX_CACHE.with(|cache| cache.borrow_mut().clear());
        let mut programs = Vec::new();
        for index in 0..300 {
            let compiled =
                super::compiled_regex(&format!("cache-pattern-{index}")).expect("pattern compiles");
            programs.push(std::rc::Rc::downgrade(&compiled));
        }

        let retained = programs
            .iter()
            .filter(|program| program.upgrade().is_some())
            .count();
        assert_eq!(retained, 256, "only bounded cache entries remain alive");
    }

    /// Hostile arguments must return or error, never abort the process.
    ///
    /// Three defects this session were invisible to a value-comparison
    /// suite because they were bad ARGUMENTS rather than wrong answers:
    /// `UNHEX` sliced a multibyte string mid-character, `CONV` called `abs()` on
    /// `i64::MIN`, and `NTILE` walked 2^64 buckets. This sweeps the scalar
    /// surface with the argument shapes that produced them.
    ///
    /// Each function is exercised only at an arity its binder accepts —
    /// calling one with too few arguments proves nothing, because the
    /// binder rejects that before evaluation ever runs.
    #[test]
    fn hostile_arguments_never_abort_the_process() {
        use pintail_types::{DataType, Value};
        let hostile = [
            Value::Utf8(String::new()),
            Value::Utf8("\u{e9}a".into()),
            Value::Utf8("\u{1f600}\u{1f600}".into()),
            Value::Utf8("-".into()),
            Value::Utf8(".".into()),
            Value::Utf8("1e999".into()),
            Value::Int64(i64::MIN),
            Value::Int64(i64::MAX),
            Value::Int64(-1),
            Value::UInt64(u64::MAX),
            Value::float64(f64::NAN),
            Value::float64(f64::INFINITY),
            Value::Binary(vec![0xFF, 0xFE]),
        ];
        // (function, arity the binder accepts)
        let surface = [
            (ScalarFunction::SubstringIndex, 3),
            (ScalarFunction::RegexpLike { negated: false }, 3),
            (ScalarFunction::JsonExtract { unquote: false }, 3),
            (ScalarFunction::Conv, 3),
            (ScalarFunction::MakeTime, 3),
            (ScalarFunction::Lpad, 3),
            (ScalarFunction::Rpad, 3),
            (ScalarFunction::Substring, 3),
            (ScalarFunction::Left, 2),
            (ScalarFunction::Right, 2),
            (ScalarFunction::Repeat, 2),
            (ScalarFunction::Locate, 2),
            (ScalarFunction::Instr, 2),
            (ScalarFunction::LogBase, 2),
            (ScalarFunction::Power, 2),
            (ScalarFunction::Round { decimal: false }, 2),
            (ScalarFunction::Truncate { decimal: false }, 2),
            (ScalarFunction::DateFormat, 2),
            (ScalarFunction::StrToDate, 2),
            (ScalarFunction::JsonLength, 2),
            (ScalarFunction::JsonKeys, 2),
            (ScalarFunction::JsonContains, 2),
            (ScalarFunction::Unhex, 1),
            (ScalarFunction::Hex, 1),
            (ScalarFunction::Md5, 1),
            (ScalarFunction::FromBase64, 1),
            (ScalarFunction::ToBase64, 1),
            (ScalarFunction::SecToTime, 1),
            (ScalarFunction::JsonValid, 1),
            (ScalarFunction::JsonType, 1),
            (ScalarFunction::Ceil { decimal: false }, 1),
            (ScalarFunction::Abs { decimal: false }, 1),
            (ScalarFunction::Space, 1),
            (ScalarFunction::Reverse, 1),
        ];
        for (function, arity) in surface {
            for first in &hostile {
                for second in &hostile {
                    let args: Vec<Value> = (0..arity)
                        .map(|position| {
                            if position == 0 {
                                first.clone()
                            } else {
                                second.clone()
                            }
                        })
                        .collect();
                    let outcome = std::panic::catch_unwind(|| {
                        super::evaluate_eager_scalar(function, &args, Some(DataType::Utf8))
                    });
                    assert!(outcome.is_ok(), "{function:?} aborted on {args:?}");
                }
            }
        }
    }

    /// A multibyte argument used to panic here: the odd-length pad made the
    /// fixed two-byte slice land inside a character. Reachable from any
    /// client query, so it is a crash rather than a wrong answer.
    #[test]
    fn unhex_returns_null_for_non_hex_instead_of_panicking() {
        assert_eq!(super::unhex("ff"), Some(vec![0xFF]));
        assert_eq!(super::unhex("FF"), Some(vec![0xFF]));
        // Odd length left-pads with a zero nibble, matching MySQL.
        assert_eq!(super::unhex("aab"), Some(vec![0x0A, 0xAB]));
        assert_eq!(super::unhex(""), Some(Vec::new()));
        // The panic cases.
        assert_eq!(super::unhex("\u{e9}a"), None);
        assert_eq!(super::unhex("\u{e9}"), None);
        assert_eq!(super::unhex("a\u{e9}"), None);
        assert_eq!(super::unhex("\u{4e16}\u{754c}"), None);
        // Ordinary non-hex text.
        assert_eq!(super::unhex("zz"), None);
        assert_eq!(super::unhex("g"), None);
    }

    /// `MySQL` copies an unrecognized directive's bare character to the output
    /// rather than raising — so this one deliberately does not error.
    #[test]
    fn date_format_copies_unknown_directives_like_mysql() {
        let value = chrono::NaiveDate::from_ymd_opt(2024, 2, 29)
            .expect("date")
            .and_hms_opt(1, 2, 3)
            .expect("time");
        assert_eq!(mysql_date_format(value, "%q"), "q");
        assert_eq!(mysql_date_format(value, "100%%"), "100%");
        assert_eq!(mysql_date_format(value, "a%"), "a%");
        assert_eq!(mysql_date_format(value, "no directives"), "no directives");
    }

    /// A date in the first days of January can belong to the previous year's
    /// last week, which is the whole reason the week helper returns a year.
    #[test]
    fn date_format_week_year_rolls_back_across_january() {
        // 2021-01-01 was a Friday, so under the Monday-first four-day rule it
        // falls in the final week of 2020.
        let value = chrono::NaiveDate::from_ymd_opt(2021, 1, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        assert_eq!(mysql_date_format(value, "%x-%v"), "2020-53");
        assert_eq!(mysql_date_format(value, "%u"), "00");
    }

    #[test]
    fn week_modes_match_mysql_mode_inventory() {
        let value = chrono::NaiveDate::from_ymd_opt(2008, 2, 20)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        let weeks = (0..=7)
            .map(|mode| date_part(value, DatePart::WeekMode(mode)))
            .collect::<Vec<_>>();
        assert_eq!(weeks, [7, 8, 7, 8, 8, 7, 8, 7]);
    }

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
            argument_types: vec![
                Some(DataType::Date32),
                Some(DataType::Utf8),
                Some(DataType::Utf8),
            ],
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

    #[test]
    fn converts_time_zones_by_name_and_offset() {
        let convert = super::convert_tz;
        assert_eq!(
            convert("2026-03-08 06:30:00", "+00:00", "+05:30").as_deref(),
            Some("2026-03-08 12:00:00")
        );
        assert_eq!(
            convert("2026-01-15 12:00:00", "UTC", "Asia/Kolkata").as_deref(),
            Some("2026-01-15 17:30:00")
        );
        // Case-insensitive zone names, like MySQL.
        assert_eq!(
            convert("2026-01-15 12:00:00", "utc", "asia/kolkata").as_deref(),
            Some("2026-01-15 17:30:00")
        );
        // DST: July New York is UTC-4, January is UTC-5.
        assert_eq!(
            convert("2026-07-04 18:00:00", "America/New_York", "UTC").as_deref(),
            Some("2026-07-04 22:00:00")
        );
        assert_eq!(
            convert("2026-01-04 18:00:00", "America/New_York", "UTC").as_deref(),
            Some("2026-01-04 23:00:00")
        );
        // Fractional seconds keep the input's digit count.
        assert_eq!(
            convert("2026-06-15 10:00:00.250", "+00:00", "+02:00").as_deref(),
            Some("2026-06-15 12:00:00.250")
        );
        assert_eq!(convert("2026-06-15 10:00:00", "Bad/Zone", "UTC"), None);
        assert_eq!(convert("not a datetime", "+00:00", "+01:00"), None);
        assert_eq!(convert("2026-06-15 10:00:00", "+15:00", "+00:00"), None);
    }
}
