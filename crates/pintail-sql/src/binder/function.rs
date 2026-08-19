//! Binding for window and scalar function calls, including CAST and
//! CONVERT targets, interval arithmetic and result-type coercion.

use pintail_types::{DataType, Value};
use sqlparser::ast::{
    DataType as SqlDataType, DateTimeField, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Value as SqlValue, WindowType,
};

use super::{
    BindError, MAX_DECIMAL_PRECISION, SubqueryResolver, aggregate_function_name,
    aggregate_result_type, bind_expr_inner, bind_modulo, bind_window_frame, comparable,
    exact_numeric_digits, is_mysql_scalar, is_truth_value, object_name_parts,
};
use crate::bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundExpr, BoundExprKind, BoundTable, BoundWindow,
    BoundWindowOrderKey, DatePart, IntervalUnit, ScalarFunction, WindowFunction,
};

/// A top-level `function(...) OVER (...)` projection item.
#[allow(clippy::too_many_lines)]
pub(super) fn bind_window_function(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
    {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let Some(WindowType::WindowSpec(spec)) = &function.over else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    // A name surviving to here means substitution did not resolve it —
    // the WINDOW clause was absent, so the reference is unresolvable.
    if spec.window_name.is_some() {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let name_parts = object_name_parts(&function.name)?;
    let [name] = name_parts.as_slice() else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let upper = name.to_ascii_uppercase();
    let ranking = match upper.as_str() {
        "ROW_NUMBER" => Some(WindowFunction::RowNumber),
        "RANK" => Some(WindowFunction::Rank),
        "DENSE_RANK" => Some(WindowFunction::DenseRank),
        _ => None,
    };
    let (window_function, data_type, nullable) = if let Some(window_function) = ranking {
        if !arguments.args.is_empty() {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        }
        (window_function, Some(DataType::UInt64), false)
    } else if matches!(upper.as_str(), "LAG" | "LEAD") {
        // MySQL takes (expr), (expr, offset) or (expr, offset, default), and
        // requires the offset to be a constant non-negative integer.
        if arguments.args.is_empty() || arguments.args.len() > 3 {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        }
        let mut bound = Vec::new();
        for argument in &arguments.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = argument else {
                return Err(BindError::UnsupportedExpression(function.to_string()));
            };
            bound.push(bind_expr_inner(
                expr, tables, aggregates, &mut None, subqueries,
            )?);
        }
        let offset = match bound.get(1) {
            None => 1,
            Some(BoundExpr {
                kind: BoundExprKind::Literal(Value::Int64(count)),
                ..
            }) if *count >= 0 => u64::try_from(*count).unwrap_or(1),
            Some(BoundExpr {
                kind: BoundExprKind::Literal(Value::UInt64(count)),
                ..
            }) => *count,
            Some(_) => return Err(BindError::UnsupportedExpression(function.to_string())),
        };
        let value_type = bound[0].data_type;
        // MySQL coerces the default to the value's type. Without this a
        // literal 0 defaulting a BIGINT UNSIGNED column yields a signed
        // value, and the output column rejects the mixed types.
        let default = match (bound.get(2).cloned(), value_type) {
            (Some(expr), Some(target)) if expr.data_type != Some(target) => {
                Some(bind_scalar(ScalarFunction::Cast(target), vec![expr])?)
            }
            (other, _) => other,
        };
        (
            WindowFunction::Offset {
                lead: upper == "LEAD",
                expr: Box::new(bound[0].clone()),
                offset,
                default: default.map(Box::new),
            },
            value_type,
            // A default only removes the edge NULL when it is itself
            // non-NULL, and the value expression may be NULL regardless.
            true,
        )
    } else if upper == "NTILE" {
        let [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] = arguments.args.as_slice() else {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        };
        let bound = bind_expr_inner(expr, tables, aggregates, &mut None, subqueries)?;
        let buckets = match &bound.kind {
            BoundExprKind::Literal(Value::Int64(count)) if *count > 0 => {
                u64::try_from(*count).unwrap_or(1)
            }
            BoundExprKind::Literal(Value::UInt64(count)) if *count > 0 => *count,
            _ => return Err(BindError::UnsupportedExpression(function.to_string())),
        };
        (
            WindowFunction::NTile(buckets),
            Some(DataType::UInt64),
            false,
        )
    } else if matches!(upper.as_str(), "FIRST_VALUE" | "LAST_VALUE") {
        let [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] = arguments.args.as_slice() else {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        };
        let bound = bind_expr_inner(expr, tables, aggregates, &mut None, subqueries)?;
        let value_type = bound.data_type;
        (
            WindowFunction::Extreme {
                last: upper == "LAST_VALUE",
                expr: Box::new(bound),
            },
            value_type,
            true,
        )
    } else {
        {
            let aggregate_function = aggregate_function_name(function)
                .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
            if arguments.args.len() != 1 {
                return Err(BindError::UnsupportedAggregate(function.to_string()));
            }
            let expr = match &arguments.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(bind_expr_inner(
                    expr, tables, aggregates, &mut None, subqueries,
                )?),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                    if aggregate_function == AggregateFunction::Count =>
                {
                    None
                }
                _ => return Err(BindError::UnsupportedAggregate(function.to_string())),
            };
            let (data_type, nullable) = aggregate_result_type(aggregate_function, expr.as_ref())?;
            (
                WindowFunction::Aggregate(BoundAggregate {
                    function: aggregate_function,
                    expr,
                    distinct: false,
                    data_type,
                    nullable,
                    separator: None,
                    order_within: Vec::new(),
                }),
                data_type,
                nullable,
            )
        }
    };
    let partition_by = spec
        .partition_by
        .iter()
        .map(|expr| bind_expr_inner(expr, tables, aggregates, &mut None, subqueries))
        .collect::<Result<Vec<_>, _>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|order| {
            if order.with_fill.is_some() {
                return Err(BindError::InvalidOrderBy(order.to_string()));
            }
            let ascending = order.options.asc.unwrap_or(true);
            Ok(BoundWindowOrderKey {
                expr: bind_expr_inner(&order.expr, tables, aggregates, &mut None, subqueries)?,
                ascending,
                nulls_first: order.options.nulls_first.unwrap_or(ascending),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if partition_by
        .iter()
        .any(|expression| expression.data_type == Some(DataType::Json))
    {
        return Err(BindError::InvalidGrouping(
            "window PARTITION BY over JSON requires JSON-aware equality".to_owned(),
        ));
    }
    if order_by
        .iter()
        .any(|key| key.expr.data_type == Some(DataType::Json))
    {
        return Err(BindError::InvalidOrderBy(
            "window ORDER BY over JSON requires JSON-aware ordering".to_owned(),
        ));
    }
    let frame = bind_window_frame(spec, function, &order_by)?;
    // FIRST_VALUE and LAST_VALUE read the frame by definition — LAST_VALUE's
    // whole reputation for surprise comes from the default frame — so a
    // frame on them is meaningful. Ranking and offset functions ignore or
    // forbid one, and rejecting beats silently dropping it.
    if frame.is_some()
        && !matches!(
            window_function,
            WindowFunction::Aggregate(_) | WindowFunction::Extreme { .. }
        )
    {
        return Err(BindError::UnsupportedQueryClause(format!(
            "window frame on {function}"
        )));
    }
    let window = BoundWindow {
        function: window_function,
        partition_by,
        order_by,
        frame,
        data_type,
        nullable,
    };
    let window_list = windows
        .as_deref_mut()
        .ok_or_else(|| BindError::UnsupportedExpression(function.to_string()))?;
    let index = window_list
        .iter()
        .position(|existing| existing == &window)
        .unwrap_or_else(|| {
            let index = window_list.len();
            window_list.push(window.clone());
            index
        });
    Ok(BoundExpr {
        kind: BoundExprKind::Window(index),
        data_type,
        nullable,
    })
}

#[allow(clippy::too_many_lines)] // a flat name-dispatch table reads best unsplit
pub(super) fn bind_scalar_function(
    function: &Function,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
    {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    let name = object_name_parts(&function.name)?;
    let [name] = name.as_slice() else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let FunctionArguments::List(arguments) = &function.args else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let function_name = name.to_ascii_uppercase();
    // JSON_VALUE owns a RETURNING clause; it lowers to a CAST around the
    // extraction below, so the function itself stays a plain extractor.
    let mut json_value_returning = None;
    for clause in &arguments.clauses {
        match clause {
            sqlparser::ast::FunctionArgumentClause::JsonReturningClause(returning)
                if function_name == "JSON_VALUE" =>
            {
                json_value_returning =
                    Some(cast_data_type(&returning.data_type).ok_or_else(|| {
                        BindError::InvalidScalarFunction(format!(
                            "JSON_VALUE RETURNING {}",
                            returning.data_type
                        ))
                    })?);
            }
            _ => return Err(BindError::UnsupportedExpression(function.to_string())),
        }
    }
    if arguments.duplicate_treatment.is_some() {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    }
    if matches!(function_name.as_str(), "DATE_ADD" | "DATE_SUB") {
        return bind_date_interval(
            function,
            arguments,
            function_name == "DATE_SUB",
            tables,
            aggregates,
            windows,
            subqueries,
        );
    }
    if function_name == "TIMESTAMPDIFF" {
        return bind_timestamp_diff(function, arguments, tables, aggregates, windows, subqueries);
    }
    if function_name == "TIMESTAMPADD" {
        return bind_timestamp_add(function, arguments, tables, aggregates, windows, subqueries);
    }
    let mut args = Vec::with_capacity(arguments.args.len());
    for argument in &arguments.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) = argument else {
            return Err(BindError::UnsupportedExpression(function.to_string()));
        };
        args.push(bind_expr_inner(
            expression, tables, aggregates, windows, subqueries,
        )?);
    }
    let scalar = match function_name.as_str() {
        "CONCAT" if !args.is_empty() => ScalarFunction::Concat,
        "SUBSTRING" | "SUBSTR" if matches!(args.len(), 2 | 3) => ScalarFunction::Substring,
        "LOWER" | "LCASE" if args.len() == 1 => ScalarFunction::Lower,
        "UPPER" | "UCASE" if args.len() == 1 => ScalarFunction::Upper,
        "TRIM" if args.len() == 1 => ScalarFunction::Trim,
        "LENGTH" if args.len() == 1 => ScalarFunction::Length,
        "CHAR_LENGTH" | "CHARACTER_LENGTH" if args.len() == 1 => ScalarFunction::CharLength,
        "REPLACE" if args.len() == 3 => ScalarFunction::Replace,
        "LEFT" if args.len() == 2 => ScalarFunction::Left,
        "RIGHT" if args.len() == 2 => ScalarFunction::Right,
        "LOCATE" if matches!(args.len(), 2 | 3) => ScalarFunction::Locate,
        "IF" if args.len() == 3 => ScalarFunction::If,
        "IFNULL" if args.len() == 2 => ScalarFunction::Coalesce,
        "COALESCE" if !args.is_empty() => ScalarFunction::Coalesce,
        "NULLIF" if args.len() == 2 => ScalarFunction::NullIf,
        "ROUND" if matches!(args.len(), 1 | 2) => ScalarFunction::Round {
            // Exact only when the digit count is knowable at bind time.
            decimal: matches!(args[0].data_type, Some(DataType::Decimal { .. }))
                && args.get(1).is_none_or(|digits| {
                    matches!(digits.kind, BoundExprKind::Literal(Value::Int64(_)))
                }),
        },
        "CEIL" | "CEILING" if args.len() == 1 => ScalarFunction::Ceil { decimal: false },
        "FLOOR" if args.len() == 1 => ScalarFunction::Floor { decimal: false },
        "ABS" if args.len() == 1 => ScalarFunction::Abs {
            decimal: matches!(args[0].data_type, Some(DataType::Decimal { .. })),
        },
        "SIGN" if args.len() == 1 => ScalarFunction::Sign,
        "POW" | "POWER" if args.len() == 2 => ScalarFunction::Power,
        "SQRT" if args.len() == 1 => ScalarFunction::Sqrt,
        "EXP" if args.len() == 1 => ScalarFunction::Exp,
        "LN" | "LOG" if args.len() == 1 => ScalarFunction::Ln,
        "LOG" if args.len() == 2 => ScalarFunction::LogBase,
        "LOG2" if args.len() == 1 => ScalarFunction::Log2,
        "LOG10" if args.len() == 1 => ScalarFunction::Log10,
        "TRUNCATE" if args.len() == 2 => ScalarFunction::Truncate { decimal: false },
        "CONCAT_WS" if args.len() >= 2 => ScalarFunction::ConcatWs,
        "REVERSE" if args.len() == 1 => ScalarFunction::Reverse,
        "REPEAT" if args.len() == 2 => ScalarFunction::Repeat,
        "SPACE" if args.len() == 1 => ScalarFunction::Space,
        "LPAD" if args.len() == 3 => ScalarFunction::Lpad,
        "RPAD" if args.len() == 3 => ScalarFunction::Rpad,
        "INSTR" if args.len() == 2 => ScalarFunction::Instr,
        "FIND_IN_SET" if args.len() == 2 => ScalarFunction::FindInSet,
        "ASCII" if args.len() == 1 => ScalarFunction::Ascii,
        "ORD" if args.len() == 1 => ScalarFunction::Ord,
        "HEX" if args.len() == 1 => ScalarFunction::Hex,
        "MD5" if args.len() == 1 => ScalarFunction::Md5,
        "UNHEX" if args.len() == 1 => ScalarFunction::Unhex,
        "ELT" if args.len() >= 2 => ScalarFunction::Elt,
        "FIELD" if args.len() >= 2 => ScalarFunction::Field,
        "FORMAT" if args.len() == 2 => ScalarFunction::Format,
        "TO_BASE64" if args.len() == 1 => ScalarFunction::ToBase64,
        "FROM_BASE64" if args.len() == 1 => ScalarFunction::FromBase64,
        "QUARTER" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Quarter),
        "DAYOFWEEK" if args.len() == 1 => ScalarFunction::DatePart(DatePart::DayOfWeek),
        "WEEKDAY" if args.len() == 1 => ScalarFunction::DatePart(DatePart::WeekDay),
        "DAYOFYEAR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::DayOfYear),
        "WEEK" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Week),
        "WEEK" if args.len() == 2 => {
            let mode = match &args[1].kind {
                BoundExprKind::Literal(Value::Int64(mode @ 0..=7)) => {
                    u8::try_from(*mode).expect("WEEK mode is bounded")
                }
                BoundExprKind::Literal(Value::UInt64(mode @ 0..=7)) => {
                    u8::try_from(*mode).expect("WEEK mode is bounded")
                }
                _ => return Err(BindError::UnsupportedExpression(function.to_string())),
            };
            args.truncate(1);
            ScalarFunction::DatePart(DatePart::WeekMode(mode))
        }
        "WEEKOFYEAR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::IsoWeek),
        "DAYNAME" if args.len() == 1 => ScalarFunction::DayName,
        "MONTHNAME" if args.len() == 1 => ScalarFunction::MonthName,
        "LAST_DAY" if args.len() == 1 => ScalarFunction::LastDay,
        "TO_DAYS" if args.len() == 1 => ScalarFunction::ToDays,
        "FROM_DAYS" if args.len() == 1 => ScalarFunction::FromDays,
        "YEARWEEK" if args.len() == 1 => ScalarFunction::YearWeek,
        "TIME_TO_SEC" if args.len() == 1 => ScalarFunction::TimeToSec,
        "SEC_TO_TIME" if args.len() == 1 => ScalarFunction::SecToTime,
        "MAKEDATE" if args.len() == 2 => ScalarFunction::MakeDate,
        "CURTIME" | "CURRENT_TIME" if args.is_empty() => ScalarFunction::Curtime,
        "STR_TO_DATE" if args.len() == 2 => ScalarFunction::StrToDate,
        "CONVERT_TZ" if args.len() == 3 => ScalarFunction::ConvertTz,
        "CHAR" if !args.is_empty() => ScalarFunction::Char,
        "RAND" if args.is_empty() => ScalarFunction::Rand,
        "REGEXP_LIKE" if matches!(args.len(), 2 | 3) => {
            ScalarFunction::RegexpLike { negated: false }
        }
        "REGEXP_SUBSTR" if args.len() == 2 => ScalarFunction::RegexpSubstr,
        "REGEXP_INSTR" if args.len() == 2 => ScalarFunction::RegexpInstr,
        "REGEXP_REPLACE" if args.len() == 3 => ScalarFunction::RegexpReplace,
        "JSON_EXTRACT" if args.len() >= 2 => ScalarFunction::JsonExtract { unquote: false },
        "JSON_UNQUOTE" if args.len() == 1 => ScalarFunction::JsonUnquote,
        "SUBSTRING_INDEX" if args.len() == 3 => ScalarFunction::SubstringIndex,
        "CONV" if args.len() == 3 => ScalarFunction::Conv,
        "MAKETIME" if args.len() == 3 => ScalarFunction::MakeTime,
        "JSON_VALID" if args.len() == 1 => ScalarFunction::JsonValid,
        "JSON_SEARCH" if matches!(args.len(), 3 | 4) => ScalarFunction::JsonSearch,
        "JSON_VALUE" if args.len() == 2 => ScalarFunction::JsonValue,
        "JSON_TYPE" if args.len() == 1 => ScalarFunction::JsonType,
        "JSON_LENGTH" if matches!(args.len(), 1 | 2) => ScalarFunction::JsonLength,
        "JSON_KEYS" if matches!(args.len(), 1 | 2) => ScalarFunction::JsonKeys,
        "JSON_CONTAINS" if matches!(args.len(), 2 | 3) => ScalarFunction::JsonContains,
        "JSON_CONTAINS_PATH" if args.len() >= 3 => ScalarFunction::JsonContainsPath,
        "JSON_OBJECT" if args.len() % 2 == 0 => ScalarFunction::JsonObject,
        "JSON_ARRAY" => ScalarFunction::JsonArray,
        "GREATEST" if args.len() >= 2 => ScalarFunction::Greatest {
            decimal: matches!(common_result_type(&args)?, Some(DataType::Decimal { .. })),
        },
        "LEAST" if args.len() >= 2 => ScalarFunction::Least {
            decimal: matches!(common_result_type(&args)?, Some(DataType::Decimal { .. })),
        },
        // MOD(a, b) is the % operator spelled as a function.
        "MOD" if args.len() == 2 => return Ok(bind_modulo(args)),
        "NOW" if args.is_empty() => ScalarFunction::Now,
        "CURDATE" if args.is_empty() => ScalarFunction::CurrentDate,
        "DATE" if args.len() == 1 => ScalarFunction::Date,
        "YEAR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Year),
        "MONTH" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Month),
        "DAY" | "DAYOFMONTH" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Day),
        "HOUR" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Hour),
        "MINUTE" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Minute),
        "SECOND" if args.len() == 1 => ScalarFunction::DatePart(DatePart::Second),
        "DATE_FORMAT" if args.len() == 2 => ScalarFunction::DateFormat,
        "DATEDIFF" if args.len() == 2 => ScalarFunction::DateDiff,
        "UNIX_TIMESTAMP" if args.len() <= 1 => ScalarFunction::UnixTimestamp,
        "FROM_UNIXTIME" if args.len() == 1 => ScalarFunction::FromUnixTime,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
    let bound = bind_scalar(scalar, args)?;
    // RETURNING is a cast over the extracted member, which keeps JSON_VALUE
    // itself a single job and reuses the CAST target table wholesale.
    match json_value_returning {
        Some(target) => bind_scalar(ScalarFunction::Cast(target), vec![bound]),
        None => Ok(bound),
    }
}

/// `TIMESTAMPADD(unit, amount, datetime)` is `DATE_ADD` with reordered
/// arguments.
fn bind_timestamp_add(
    function: &Function,
    arguments: &sqlparser::ast::FunctionArgumentList,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(unit)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(amount)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(datetime)),
    ] = arguments.args.as_slice()
    else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let Expr::Identifier(unit) = unit else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let unit = match unit.value.to_ascii_uppercase().as_str() {
        "YEAR" => IntervalUnit::Year,
        "MONTH" => IntervalUnit::Month,
        "DAY" => IntervalUnit::Day,
        "HOUR" => IntervalUnit::Hour,
        "MINUTE" => IntervalUnit::Minute,
        "SECOND" => IntervalUnit::Second,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
    let datetime = bind_expr_inner(datetime, tables, aggregates, windows, subqueries)?;
    let amount = bind_expr_inner(amount, tables, aggregates, windows, subqueries)?;
    bind_scalar(
        ScalarFunction::DateInterval {
            unit,
            subtract: false,
        },
        vec![datetime, amount],
    )
}

fn bind_date_interval(
    function: &Function,
    arguments: &sqlparser::ast::FunctionArgumentList,
    subtract: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(date)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Interval(interval))),
    ] = arguments.args.as_slice()
    else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    bind_interval_arithmetic(
        date, interval, subtract, tables, aggregates, windows, subqueries,
    )
    .map_err(|_| BindError::UnsupportedExpression(function.to_string()))
}

/// Shared by `DATE_ADD`/`DATE_SUB` and `MySQL`'s inline operator form
/// (`expr +- INTERVAL n unit`): both are the same `DateInterval` scalar.
#[allow(clippy::too_many_arguments)]
pub(super) fn bind_interval_arithmetic(
    date: &Expr,
    interval: &sqlparser::ast::Interval,
    subtract: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if interval.leading_precision.is_some()
        || interval.last_field.is_some()
        || interval.fractional_seconds_precision.is_some()
    {
        return Err(BindError::UnsupportedExpression(interval.to_string()));
    }
    let Some(unit) = interval_unit_of(interval.leading_field.as_ref()) else {
        return Err(BindError::UnsupportedExpression(interval.to_string()));
    };
    bind_scalar(
        ScalarFunction::DateInterval { unit, subtract },
        vec![
            bind_expr_inner(date, tables, aggregates, windows, subqueries)?,
            bind_expr_inner(&interval.value, tables, aggregates, windows, subqueries)?,
        ],
    )
}

fn interval_unit_of(field: Option<&DateTimeField>) -> Option<IntervalUnit> {
    match field {
        Some(DateTimeField::Year) => Some(IntervalUnit::Year),
        Some(DateTimeField::Month) => Some(IntervalUnit::Month),
        Some(DateTimeField::Day) => Some(IntervalUnit::Day),
        Some(DateTimeField::Hour) => Some(IntervalUnit::Hour),
        Some(DateTimeField::Minute) => Some(IntervalUnit::Minute),
        Some(DateTimeField::Second) => Some(IntervalUnit::Second),
        _ => None,
    }
}

/// `TIMESTAMPDIFF(unit, from, to)` — the unit is a bare keyword argument.
fn bind_timestamp_diff(
    function: &Function,
    arguments: &sqlparser::ast::FunctionArgumentList,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(unit)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(from)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(to)),
    ] = arguments.args.as_slice()
    else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let Expr::Identifier(unit) = unit else {
        return Err(BindError::UnsupportedExpression(function.to_string()));
    };
    let unit = match unit.value.to_ascii_uppercase().as_str() {
        "YEAR" => IntervalUnit::Year,
        "MONTH" => IntervalUnit::Month,
        "DAY" => IntervalUnit::Day,
        "HOUR" => IntervalUnit::Hour,
        "MINUTE" => IntervalUnit::Minute,
        "SECOND" => IntervalUnit::Second,
        _ => return Err(BindError::UnsupportedExpression(function.to_string())),
    };
    bind_scalar(
        ScalarFunction::TimestampDiff { unit },
        vec![
            bind_expr_inner(from, tables, aggregates, windows, subqueries)?,
            bind_expr_inner(to, tables, aggregates, windows, subqueries)?,
        ],
    )
}

pub(super) fn bind_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if list.is_empty() {
        return Err(BindError::UnsupportedExpression(expr.to_string()));
    }
    // A row-constructor subject - `(a, b) IN ((1, 2), (3, 4))` - is exactly
    // the OR over items of the AND of pairwise equalities, including under
    // three-valued NULL logic, so it desugars here rather than teaching the
    // executor about tuples. The natural predicate for composite-key tables
    // (found by a customer addressing one).
    if let Expr::Tuple(subject) = expr {
        let mut alternatives: Option<BoundExpr> = None;
        for item in list {
            let Expr::Tuple(values) = item else {
                return Err(BindError::UnsupportedExpression(format!(
                    "IN list item {item} must be a row constructor of {} values",
                    subject.len()
                )));
            };
            if values.len() != subject.len() {
                return Err(BindError::UnsupportedExpression(format!(
                    "IN list item {item} must be a row constructor of {} values",
                    subject.len()
                )));
            }
            let mut conjunction: Option<BoundExpr> = None;
            for (column, value) in subject.iter().zip(values) {
                let left = bind_expr_inner(column, tables, aggregates, windows, subqueries)?;
                let right = bind_expr_inner(value, tables, aggregates, windows, subqueries)?;
                if !comparable(left.data_type, right.data_type) {
                    return Err(BindError::InvalidScalarFunction("IN".to_owned()));
                }
                let equality = equality_expr(left, right)?;
                conjunction = Some(match conjunction {
                    None => equality,
                    Some(existing) => BoundExpr {
                        nullable: existing.nullable || equality.nullable,
                        data_type: Some(DataType::Boolean),
                        kind: BoundExprKind::Binary {
                            op: BinaryOp::And,
                            left: Box::new(existing),
                            right: Box::new(equality),
                        },
                    },
                });
            }
            let conjunction = conjunction.expect("row constructors are non-empty");
            alternatives = Some(match alternatives {
                None => conjunction,
                Some(existing) => BoundExpr {
                    nullable: existing.nullable || conjunction.nullable,
                    data_type: Some(DataType::Boolean),
                    kind: BoundExprKind::Binary {
                        op: BinaryOp::Or,
                        left: Box::new(existing),
                        right: Box::new(conjunction),
                    },
                },
            });
        }
        let bound = alternatives.expect("IN lists are non-empty");
        if negated {
            return Ok(BoundExpr {
                nullable: bound.nullable,
                data_type: Some(DataType::Boolean),
                kind: BoundExprKind::Unary {
                    op: crate::bound::UnaryOp::Not,
                    expr: Box::new(bound),
                },
            });
        }
        return Ok(bound);
    }
    let mut args = Vec::with_capacity(list.len() + 1);
    args.push(bind_expr_inner(
        expr, tables, aggregates, windows, subqueries,
    )?);
    for value in list {
        args.push(bind_expr_inner(
            value, tables, aggregates, windows, subqueries,
        )?);
    }
    if args[1..]
        .iter()
        .any(|value| !comparable(args[0].data_type, value.data_type))
    {
        return Err(BindError::InvalidScalarFunction("IN".to_owned()));
    }
    bind_scalar(ScalarFunction::InList { negated }, args)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn bind_between(
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let args = vec![
        bind_expr_inner(expr, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(low, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(high, tables, aggregates, windows, subqueries)?,
    ];
    if !comparable(args[0].data_type, args[1].data_type)
        || !comparable(args[0].data_type, args[2].data_type)
    {
        return Err(BindError::InvalidScalarFunction("BETWEEN".to_owned()));
    }
    bind_scalar(ScalarFunction::Between { negated }, args)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn bind_like(
    expr: &Expr,
    pattern: &Expr,
    negated: bool,
    escape: Option<&sqlparser::ast::ValueWithSpan>,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let escape = match escape {
        None => None,
        Some(escape) => Some(
            match &escape.value {
                SqlValue::SingleQuotedString(value) => {
                    let mut chars = value.chars();
                    let character = chars.next();
                    if chars.next().is_none() {
                        character
                    } else {
                        None
                    }
                }
                _ => None,
            }
            .ok_or_else(|| BindError::InvalidScalarFunction("LIKE ESCAPE".to_owned()))?,
        ),
    };
    let args = vec![
        bind_expr_inner(expr, tables, aggregates, windows, subqueries)?,
        bind_expr_inner(pattern, tables, aggregates, windows, subqueries)?,
    ];
    bind_scalar(ScalarFunction::Like { negated, escape }, args)
}

pub(super) fn bind_case(
    operand: Option<&Expr>,
    conditions: &[sqlparser::ast::CaseWhen],
    else_result: Option<&Expr>,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    if conditions.is_empty() {
        return Err(BindError::InvalidScalarFunction("CASE".to_owned()));
    }
    let operand = operand
        .map(|expression| bind_expr_inner(expression, tables, aggregates, windows, subqueries))
        .transpose()?;
    let mut result = else_result.map_or_else(
        || {
            Ok(BoundExpr {
                kind: BoundExprKind::Literal(Value::Null),
                data_type: None,
                nullable: true,
            })
        },
        |expression| bind_expr_inner(expression, tables, aggregates, windows, subqueries),
    )?;
    for clause in conditions.iter().rev() {
        let condition =
            bind_expr_inner(&clause.condition, tables, aggregates, windows, subqueries)?;
        let condition = if let Some(operand) = &operand {
            equality_expr(operand.clone(), condition)?
        } else {
            if !is_truth_value(condition.data_type) {
                return Err(BindError::ExpectedPredicate {
                    actual: condition.data_type,
                });
            }
            condition
        };
        let value = bind_expr_inner(&clause.result, tables, aggregates, windows, subqueries)?;
        result = bind_scalar(ScalarFunction::If, vec![condition, value, result])?;
    }
    Ok(result)
}

pub(super) fn bind_cast(
    expr: &Expr,
    data_type: &SqlDataType,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let target = cast_data_type(data_type)
        .ok_or_else(|| BindError::InvalidScalarFunction(format!("CAST AS {data_type}")))?;
    bind_scalar(
        ScalarFunction::Cast(target),
        vec![bind_expr_inner(
            expr, tables, aggregates, windows, subqueries,
        )?],
    )
}

pub(super) fn bind_convert(
    conversion: &Expr,
    tables: &[BoundTable],
    aggregates: &mut Option<&mut Vec<BoundAggregate>>,
    windows: &mut Option<&mut Vec<BoundWindow>>,
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<BoundExpr, BindError> {
    let Expr::Convert {
        is_try,
        expr,
        data_type,
        charset,
        target_before_value,
        styles,
    } = conversion
    else {
        unreachable!("bind_convert is called only for CONVERT expressions");
    };
    if *is_try || *target_before_value || !styles.is_empty() {
        return Err(BindError::UnsupportedExpression(conversion.to_string()));
    }
    let target = match (data_type, charset) {
        (Some(data_type), _) => cast_data_type(data_type)
            .ok_or_else(|| BindError::InvalidScalarFunction(format!("CONVERT TO {data_type}")))?,
        (None, Some(charset)) => match charset.to_string().to_ascii_lowercase().as_str() {
            "binary" => DataType::Binary,
            // These names all denote Pintail's UTF-8 carrier. Other MySQL
            // character sets require byte-level transcoding and must not be
            // silently relabeled as UTF-8.
            "utf8" | "utf8mb3" | "utf8mb4" => DataType::Utf8,
            unsupported => {
                return Err(BindError::InvalidScalarFunction(format!(
                    "CONVERT USING unsupported character set {unsupported}"
                )));
            }
        },
        (None, None) => {
            return Err(BindError::InvalidScalarFunction(
                "CONVERT requires a target type or character set".to_owned(),
            ));
        }
    };
    bind_scalar(
        ScalarFunction::Cast(target),
        vec![bind_expr_inner(
            expr, tables, aggregates, windows, subqueries,
        )?],
    )
}

fn cast_data_type(data_type: &SqlDataType) -> Option<DataType> {
    // CAST AS DECIMAL is exact: the executor coerces onto scaled i128
    // units with MySQL's half-away-from-zero rounding.
    if let SqlDataType::Decimal(info) | SqlDataType::Numeric(info) | SqlDataType::Dec(info) =
        data_type
    {
        let (precision, scale) = match info {
            sqlparser::ast::ExactNumberInfo::None => (10, 0),
            sqlparser::ast::ExactNumberInfo::Precision(precision) => (*precision, 0),
            sqlparser::ast::ExactNumberInfo::PrecisionAndScale(precision, scale) => {
                (*precision, *scale)
            }
        };
        let precision = u8::try_from(precision).ok()?.clamp(1, 38);
        let scale = u8::try_from(scale).ok()?.min(30).min(precision);
        return Some(DataType::Decimal { precision, scale });
    }
    // Temporal and JSON targets, checked before the substring heuristic
    // below: `DATETIME` contains neither `CHAR` nor `INT`, so it used to fall
    // through to `None` and reject. Fractional-second precision rides along
    // where MySQL allows it.
    match data_type {
        SqlDataType::Date => return Some(DataType::Date32),
        SqlDataType::Datetime(fsp) | SqlDataType::Timestamp(fsp, _) => {
            let fsp = fsp
                .and_then(|digits| u8::try_from(digits).ok())
                .unwrap_or(0);
            return Some(DataType::DateTime64 { fsp: fsp.min(6) });
        }
        SqlDataType::Time(fsp, sqlparser::ast::TimezoneInfo::None) => {
            let fsp = fsp
                .and_then(|digits| u8::try_from(digits).ok())
                .unwrap_or(0);
            return Some(DataType::Time64 { fsp: fsp.min(6) });
        }
        SqlDataType::JSON => return Some(DataType::Json),
        _ => {}
    }
    let name = data_type.to_string().to_ascii_uppercase();
    if name == "YEAR" {
        Some(DataType::Year)
    } else if name.contains("BINARY") || name.contains("BLOB") {
        Some(DataType::Binary)
    } else if name.contains("CHAR") || name.contains("TEXT") {
        Some(DataType::Utf8)
    } else if name.contains("UNSIGNED") {
        Some(DataType::UInt64)
    } else if name.contains("DOUBLE")
        || name.contains("FLOAT")
        || name.contains("REAL")
        || name.contains("DECIMAL")
    {
        Some(DataType::Float64)
    } else if name.contains("INT") || name == "SIGNED" {
        Some(DataType::Int64)
    } else if name.contains("BOOL") {
        Some(DataType::Boolean)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)] // a flat type-dispatch table reads best unsplit
pub(super) fn bind_scalar(
    function: ScalarFunction,
    args: Vec<BoundExpr>,
) -> Result<BoundExpr, BindError> {
    // Exact-decimal math flags resolve here, once the operand types are
    // known, so every construction site stays oblivious.
    let arg0_decimal = matches!(
        args.first().and_then(|argument| argument.data_type),
        Some(DataType::Decimal { .. })
    );
    let function = match function {
        ScalarFunction::Ceil { .. } => ScalarFunction::Ceil {
            decimal: arg0_decimal,
        },
        ScalarFunction::Floor { .. } => ScalarFunction::Floor {
            decimal: arg0_decimal,
        },
        ScalarFunction::Truncate { .. } => ScalarFunction::Truncate {
            decimal: arg0_decimal
                && matches!(
                    args.get(1).map(|digits| &digits.kind),
                    Some(BoundExprKind::Literal(Value::Int64(_)))
                ),
        },
        other => other,
    };
    if matches!(
        function,
        ScalarFunction::Locate
            | ScalarFunction::Like { .. }
            | ScalarFunction::InList { .. }
            | ScalarFunction::Between { .. }
            | ScalarFunction::NullIf
            | ScalarFunction::Greatest { .. }
            | ScalarFunction::Least { .. }
            | ScalarFunction::Instr
            | ScalarFunction::FindInSet
            | ScalarFunction::RegexpLike { .. }
            | ScalarFunction::RegexpSubstr
            | ScalarFunction::RegexpInstr
            | ScalarFunction::RegexpReplace
    ) {
        ensure_supported_text_collation(&args.iter().collect::<Vec<_>>())?;
    }
    if function == ScalarFunction::StrToDate
        && let Some(BoundExpr {
            kind: BoundExprKind::Literal(Value::Utf8(format)),
            ..
        }) = args.get(1)
        && !str_to_date_format_supported(format)
    {
        return Err(BindError::UnsupportedExpression(format!(
            "STR_TO_DATE format {format:?}"
        )));
    }
    if matches!(
        function,
        ScalarFunction::RegexpLike { .. }
            | ScalarFunction::RegexpSubstr
            | ScalarFunction::RegexpInstr
            | ScalarFunction::RegexpReplace
    ) && args
        .iter()
        .any(|argument| argument.data_type == Some(DataType::Binary))
    {
        return Err(BindError::InvalidScalarFunction(
            "regular expressions do not accept binary-string operands".to_owned(),
        ));
    }
    let (data_type, nullable) = match function {
        // LOWER/UPPER over a binary argument return it unchanged, so the
        // result stays binary — declaring Utf8 here made the output column
        // reject the value it was handed.
        ScalarFunction::Lower | ScalarFunction::Upper
            if matches!(
                args.first().and_then(|argument| argument.data_type),
                Some(DataType::Binary)
            ) =>
        {
            (
                Some(DataType::Binary),
                args.iter().any(|argument| argument.nullable),
            )
        }
        ScalarFunction::Concat
        | ScalarFunction::Substring
        | ScalarFunction::Lower
        | ScalarFunction::Upper
        | ScalarFunction::Trim
        | ScalarFunction::Replace
        | ScalarFunction::Left
        | ScalarFunction::Right => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Length | ScalarFunction::CharLength | ScalarFunction::Locate => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Like { .. }
        | ScalarFunction::InList { .. }
        | ScalarFunction::Between { .. } => (
            Some(DataType::Boolean),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::If => (
            common_result_type(&args[1..])?,
            args[1..].iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Coalesce => (
            common_result_type(&args)?,
            args.iter().all(|argument| argument.nullable),
        ),
        ScalarFunction::NullIf => (args[0].data_type, true),
        ScalarFunction::Round { decimal: true } => {
            let Some(DataType::Decimal { precision, scale }) = args[0].data_type else {
                return Err(BindError::UnsupportedExpression("ROUND".to_owned()));
            };
            let digits = match args.get(1).map(|argument| &argument.kind) {
                Some(BoundExprKind::Literal(Value::Int64(digits))) => *digits,
                None => 0,
                _ => return Err(BindError::UnsupportedExpression("ROUND".to_owned())),
            };
            // MySQL keeps min(input scale, digit count) fraction digits and
            // one extra integer digit for the carry.
            let result_scale = u8::try_from(digits.clamp(0, i64::from(scale))).unwrap_or(scale);
            let result_precision = precision
                .saturating_sub(scale)
                .saturating_add(result_scale)
                .saturating_add(1)
                .min(38);
            (
                Some(DataType::Decimal {
                    precision: result_precision.max(result_scale.saturating_add(1)),
                    scale: result_scale,
                }),
                args.iter().any(|argument| argument.nullable),
            )
        }
        // MySQL's CEIL/FLOOR of an exact numeric is an exact integer value.
        ScalarFunction::Ceil { decimal: true } | ScalarFunction::Floor { decimal: true } => {
            (Some(DataType::Int64), args[0].nullable)
        }
        ScalarFunction::Truncate { decimal: true } => {
            let Some(DataType::Decimal { precision, scale }) = args[0].data_type else {
                return Err(BindError::UnsupportedExpression("TRUNCATE".to_owned()));
            };
            let digits = match args.get(1).map(|argument| &argument.kind) {
                Some(BoundExprKind::Literal(Value::Int64(digits))) => *digits,
                _ => return Err(BindError::UnsupportedExpression("TRUNCATE".to_owned())),
            };
            let result_scale = u8::try_from(digits.clamp(0, i64::from(scale))).unwrap_or(scale);
            (
                Some(DataType::Decimal {
                    precision: precision
                        .saturating_sub(scale)
                        .saturating_add(result_scale)
                        .max(result_scale.saturating_add(1))
                        .min(38),
                    scale: result_scale,
                }),
                args[0].nullable,
            )
        }
        ScalarFunction::Round { decimal: false }
        | ScalarFunction::Ceil { decimal: false }
        | ScalarFunction::Floor { decimal: false } => (
            Some(DataType::Float64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Abs { .. } => (
            // Exact numerics keep their type; everything else takes the f64
            // carrier like the other math scalars.
            match args[0].data_type.map(DataType::storage_type) {
                Some(DataType::Int64) => Some(DataType::Int64),
                Some(DataType::UInt64) => Some(DataType::UInt64),
                _ if matches!(args[0].data_type, Some(DataType::Decimal { .. })) => {
                    args[0].data_type
                }
                _ => Some(DataType::Float64),
            },
            args[0].nullable,
        ),
        ScalarFunction::Sign => (Some(DataType::Int64), args[0].nullable),
        ScalarFunction::Power
        | ScalarFunction::Sqrt
        | ScalarFunction::Exp
        | ScalarFunction::Ln
        | ScalarFunction::LogBase
        | ScalarFunction::Log2
        | ScalarFunction::Log10
        | ScalarFunction::Truncate { decimal: false } => (
            // MySQL returns NULL outside a function's domain (SQRT of a
            // negative, logs of non-positives), so these stay nullable.
            Some(DataType::Float64),
            true,
        ),
        ScalarFunction::Greatest { .. } | ScalarFunction::Least { .. } => (
            common_result_type(&args)?,
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::ConcatWs => (Some(DataType::Utf8), args[0].nullable),
        ScalarFunction::Reverse
        | ScalarFunction::Repeat
        | ScalarFunction::Space
        | ScalarFunction::Lpad
        | ScalarFunction::Rpad
        | ScalarFunction::Format
        | ScalarFunction::ToBase64
        | ScalarFunction::Hex
        | ScalarFunction::Md5
        | ScalarFunction::DayName
        | ScalarFunction::MonthName
        | ScalarFunction::LastDay
        | ScalarFunction::FromDays
        | ScalarFunction::SecToTime => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        // NULL out of range / on malformed or unmatched input, like MySQL.
        ScalarFunction::Elt
        | ScalarFunction::MakeDate
        | ScalarFunction::StrToDate
        | ScalarFunction::ConvertTz
        | ScalarFunction::RegexpSubstr
        | ScalarFunction::JsonUnquote
        // JSON_TYPE raises on a non-JSON document; JSON_KEYS answers NULL for
        // a target that is not an object.
        | ScalarFunction::JsonType
        | ScalarFunction::JsonExtract { unquote: true }
        // JSON_SEARCH answers NULL when nothing matches; JSON_VALUE answers
        // NULL for an absent path.
        | ScalarFunction::JsonValue
        // CONV and MAKETIME answer NULL for an unparseable number or an
        // out-of-range minute/second.
        | ScalarFunction::Conv
        | ScalarFunction::MakeTime => (Some(DataType::Utf8), true),
        // SUBSTRING_INDEX always returns a string for non-NULL input; an
        // absent delimiter yields the whole subject rather than NULL.
        ScalarFunction::SubstringIndex => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Unhex | ScalarFunction::FromBase64 => (Some(DataType::Binary), true),
        // NULL arguments are skipped, so the result itself is never NULL.
        ScalarFunction::Char => (Some(DataType::Binary), false),
        ScalarFunction::Rand => (Some(DataType::Float64), false),
        ScalarFunction::Instr
        | ScalarFunction::FindInSet
        | ScalarFunction::Ascii
        | ScalarFunction::Ord
        | ScalarFunction::Field
        | ScalarFunction::ToDays
        | ScalarFunction::YearWeek => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::RegexpLike { .. } | ScalarFunction::DecimalComparison { .. } => (
            Some(DataType::Boolean),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::JsonExtract { unquote: false }
        | ScalarFunction::JsonKeys
        | ScalarFunction::JsonSearch => (Some(DataType::Json), true),
        // NULL arguments become JSON nulls, never a NULL result.
        ScalarFunction::JsonObject | ScalarFunction::JsonArray => (Some(DataType::Json), false),
        // JSON_VALID answers 0/1 for any input, so it is the one predicate
        // here that never yields NULL for a non-NULL argument.
        ScalarFunction::JsonValid => (Some(DataType::Int64), true),
        ScalarFunction::JsonContains | ScalarFunction::JsonContainsPath => {
            (Some(DataType::Int64), true)
        }
        ScalarFunction::JsonLength => (Some(DataType::UInt64), true),
        ScalarFunction::RegexpInstr => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::RegexpReplace => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::TimeToSec => (
            Some(DataType::Int64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::TimestampDiff { .. } => (
            Some(DataType::Int64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::Cast(target) => (Some(target), args[0].nullable),
        ScalarFunction::Now | ScalarFunction::CurrentDate | ScalarFunction::Curtime => {
            (Some(DataType::Utf8), false)
        }
        ScalarFunction::Date
        | ScalarFunction::DateFormat
        | ScalarFunction::DateInterval { .. }
        | ScalarFunction::FromUnixTime => (
            Some(DataType::Utf8),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::DatePart(_) | ScalarFunction::UnixTimestamp => (
            Some(DataType::UInt64),
            args.iter().any(|argument| argument.nullable),
        ),
        ScalarFunction::DateDiff => (
            Some(DataType::Int64),
            args.iter().any(|argument| argument.nullable),
        ),
    };
    let mut args = args;
    coerce_decimal_branches(function, data_type, &mut args);
    Ok(BoundExpr {
        kind: BoundExprKind::Scalar { function, args },
        data_type,
        nullable,
    })
}

fn str_to_date_format_supported(format: &str) -> bool {
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character == '%'
            && !matches!(
                characters.next(),
                Some(
                    'c' | 'e'
                        | 'M'
                        | 'k'
                        | 'l'
                        | 'i'
                        | 's'
                        | 'f'
                        | 'Y'
                        | 'y'
                        | 'm'
                        | 'd'
                        | 'H'
                        | 'h'
                        | 'I'
                        | 'p'
                        | 'b'
                        | 'W'
                        | 'a'
                        | 'j'
                        | 'r'
                        | 'T'
                        | '%'
                )
            )
        {
            return false;
        }
    }
    true
}

/// A decimal-unified IF/COALESCE/GREATEST/LEAST must also coerce its branch
/// VALUES: integer branches would otherwise reach decimal-typed consumers
/// (SUM, the wire layer) as raw integers and lose the declared scale.
fn coerce_decimal_branches(
    function: ScalarFunction,
    data_type: Option<DataType>,
    args: &mut [BoundExpr],
) {
    let Some(unified @ DataType::Decimal { .. }) = data_type else {
        return;
    };
    if !matches!(
        function,
        ScalarFunction::If
            | ScalarFunction::Coalesce
            | ScalarFunction::Greatest { .. }
            | ScalarFunction::Least { .. }
    ) {
        return;
    }
    let first_value = usize::from(matches!(function, ScalarFunction::If));
    for argument in args.iter_mut().skip(first_value) {
        if argument.data_type != Some(unified)
            && !matches!(argument.kind, BoundExprKind::Literal(Value::Null))
        {
            let nullable = argument.nullable;
            let inner = std::mem::replace(
                argument,
                BoundExpr {
                    kind: BoundExprKind::Literal(Value::Null),
                    data_type: None,
                    nullable: true,
                },
            );
            *argument = BoundExpr {
                kind: BoundExprKind::Scalar {
                    function: ScalarFunction::Cast(unified),
                    args: vec![inner],
                },
                data_type: Some(unified),
                nullable,
            };
        }
    }
}

fn common_result_type(args: &[BoundExpr]) -> Result<Option<DataType>, BindError> {
    let types = args
        .iter()
        .filter_map(|argument| argument.data_type)
        .collect::<Vec<_>>();
    let Some(first) = types.first().copied() else {
        return Ok(None);
    };
    if types.iter().all(|data_type| *data_type == first) {
        return Ok(Some(first));
    }
    if types
        .iter()
        .all(|data_type| is_mysql_scalar(Some(*data_type)))
    {
        if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Utf8 | DataType::Binary))
        {
            Ok(Some(DataType::Utf8))
        } else if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Float32 | DataType::Float64))
        {
            Ok(Some(DataType::Float64))
        } else if types
            .iter()
            .any(|data_type| matches!(data_type, DataType::Decimal { .. }))
        {
            // MySQL keeps CASE/COALESCE branches exact when decimals mix
            // with integers: unify to a decimal wide enough for every
            // branch. Collapsing to an integer type truncated the fraction
            // (2026-08-03 production-acceptance q01).
            let mut scale: u8 = 0;
            let mut integer_digits: u8 = 0;
            for data_type in &types {
                let (branch_scale, branch_integer) =
                    exact_numeric_digits(*data_type).unwrap_or((0, 20));
                scale = scale.max(branch_scale);
                integer_digits = integer_digits.max(branch_integer);
            }
            Ok(Some(DataType::Decimal {
                precision: integer_digits
                    .saturating_add(scale)
                    .min(MAX_DECIMAL_PRECISION),
                scale,
            }))
        } else if types.contains(&DataType::Int64) && types.contains(&DataType::UInt64) {
            Ok(Some(DataType::Float64))
        } else if types.contains(&DataType::UInt64) {
            Ok(Some(DataType::UInt64))
        } else {
            Ok(Some(DataType::Int64))
        }
    } else {
        Err(BindError::InvalidScalarFunction(
            "incompatible result types".to_owned(),
        ))
    }
}

pub(super) fn ensure_supported_text_collation(expressions: &[&BoundExpr]) -> Result<(), BindError> {
    let mut collations = Vec::new();
    for expression in expressions {
        expression.collect_source_collations(&mut collations);
    }
    collations.sort_unstable();
    collations.dedup();
    if collations.is_empty() {
        return Ok(());
    }
    // One supported collation for the whole expression is fine; the executor
    // is told which one and compares accordingly. A MIXTURE is not, even when
    // both halves are supported - the two disagree about trailing spaces and
    // about supplementary characters, so the comparison has two defensible
    // answers. MySQL picks one by coercibility; guessing here would produce a
    // wrong answer where refusing produces an error.
    if collations.len() == 1
        && crate::bound::SUPPORTED_TEXT_COLLATIONS.contains(&collations[0].as_str())
    {
        return Ok(());
    }
    let detail = collations.join(", ");
    let supported = crate::bound::SUPPORTED_TEXT_COLLATIONS.join(", ");
    Err(BindError::UnsupportedExpression(if collations.len() > 1 {
        format!("comparing text across collations {detail} is unsupported")
    } else {
        format!("text collation {detail} is unsupported; supported: {supported}")
    }))
}

fn equality_expr(left: BoundExpr, right: BoundExpr) -> Result<BoundExpr, BindError> {
    if !comparable(left.data_type, right.data_type) {
        return Err(BindError::InvalidBinaryTypes {
            operation: "=".to_owned(),
            left: left.data_type,
            right: right.data_type,
        });
    }
    ensure_supported_text_collation(&[&left, &right])?;
    Ok(BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: Box::new(left),
            right: Box::new(right),
        },
    })
}
