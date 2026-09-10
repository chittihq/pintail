//! Result declarations are derived before optimization changes physical carriers.
use pintail_catalog::CatalogSnapshot;
use pintail_protocol::{Column, ColumnFlags, ColumnType};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundExpr, BoundExprKind, BoundJoinKind,
    BoundQuery, DatePart, ScalarFunction, SourceFacts, WindowFunction,
};
use pintail_types::{DataType, Value};

use crate::engine::QueryField;

pub(crate) fn columns(
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Vec<Column> {
    query
        .projection
        .iter()
        .enumerate()
        .map(|(index, projection)| {
            let mut column = expression(&projection.expr, query, catalog, facts);
            column.column.clone_from(&projection.name);
            for branch in query
                .union_all
                .iter()
                .chain(query.set_ops.iter().map(|(_, branch)| branch))
            {
                if let Some(projection) = branch.projection.get(index) {
                    let other = expression(&projection.expr, branch, catalog, facts);
                    column.column_length = column.column_length.max(other.column_length);
                }
            }
            if query.recursive.is_some() {
                column.column_length = column.column_length.saturating_add(1);
                set_flags(&mut column, 0);
            }
            if query.distinct && !column.colflags.contains(ColumnFlags::NOT_NULL_FLAG) {
                column.colflags |= ColumnFlags::from_bits(32768);
            }
            if !query.group_by.is_empty()
                && !ordered_group(query)
                && grouped_scalar(&projection.expr)
            {
                if column.character_set != 63 {
                    column.decimals = 0;
                }
                if column.coltype == ColumnType::MysqlTypeLongBlob {
                    column.coltype = ColumnType::MysqlTypeBlob;
                    column.colflags |= ColumnFlags::from_bits(16);
                }
            }
            if (query.distinct || !query.union_all.is_empty() || !query.set_ops.is_empty())
                && column.character_set != 63
            {
                column.decimals = 0;
            }
            if !query.windows.is_empty() {
                let flags = column.colflags.bits() & !32768;
                set_flags(&mut column, flags);
            }
            column
        })
        .collect()
}

fn base(expr: &BoundExpr) -> Column {
    let field = QueryField {
        wire_column: None,
        name: String::new(),
        data_type: expr.data_type,
        nullable: expr.nullable,
        collation: None,
        group_concat: false,
        geometry: false,
        timestamp: false,
        wire_hint: None,
    };
    let mut column = crate::server::mysql_column(&field, 1024, "utf8mb4", 224);
    if column.character_set == 63 {
        column.colflags |= ColumnFlags::BINARY_FLAG;
    }
    if expr.data_type == Some(DataType::Json) {
        column.character_set = 224;
        column.column_length = u32::MAX - 3;
        column.decimals = 31;
        set_flags(&mut column, 128);
    }
    if expr.data_type == Some(DataType::Utf8) {
        column.decimals = 31;
    }
    column
}

fn set_flags(column: &mut Column, flags: u16) {
    column.colflags = ColumnFlags::from_bits(flags);
}
fn unsigned(column: &Column) -> bool {
    column.colflags.contains(ColumnFlags::UNSIGNED_FLAG)
}
fn integer(column: &Column) -> bool {
    matches!(
        column.coltype,
        ColumnType::MysqlTypeTiny
            | ColumnType::MysqlTypeShort
            | ColumnType::MysqlTypeLong
            | ColumnType::MysqlTypeLonglong
    )
}

fn aggregate(
    aggregate: &BoundAggregate,
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Column {
    let expr = BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        data_type: aggregate.data_type,
        nullable: aggregate.nullable,
    };
    let mut column = base(&expr);
    let input = aggregate
        .expr
        .as_ref()
        .map(|expr| expression(expr, query, catalog, facts));
    match aggregate.function {
        AggregateFunction::Count => {
            column.coltype = ColumnType::MysqlTypeLonglong;
            column.column_length = 21;
            set_flags(&mut column, 129);
        }
        AggregateFunction::Sum | AggregateFunction::Average => {
            if let Some(input) = input
                && (integer(&input) || input.coltype == ColumnType::MysqlTypeNewdecimal)
            {
                let digits = precision(&input);
                column.coltype = ColumnType::MysqlTypeNewdecimal;
                column.decimals = input
                    .decimals
                    .saturating_add(if aggregate.function == AggregateFunction::Average {
                        4
                    } else {
                        0
                    })
                    .min(30);
                let precision = (digits
                    + if aggregate.function == AggregateFunction::Average {
                        4
                    } else {
                        22
                    })
                .min(65);
                column.column_length = precision + 1 + u32::from(column.decimals > 0);
                set_flags(&mut column, 128);
            }
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum => {
            if let Some(input) = input {
                column = input;
                let flags = if !query.group_by.is_empty() && !ordered_group(query) {
                    column.colflags.bits() & (32 | 4096)
                } else {
                    (column.colflags.bits() & 32) | if column.character_set == 63 { 128 } else { 0 }
                };
                set_flags(&mut column, flags);
                if column.character_set != 63 {
                    column.decimals = 31;
                }
            }
        }
        AggregateFunction::AnyValue => {
            if let Some(input) = input {
                column = input;
                if aggregate.declared {
                    if column.colflags.bits() & 256 != 0 {
                        column.coltype = ColumnType::MysqlTypeEnum;
                    }
                    let flags = column.colflags.bits() & 1;
                    set_flags(&mut column, flags);
                    column.decimals = 31;
                }
            }
        }
        AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. } => {
            column.column_length = 23;
        }
        AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor => {
            column.column_length = 21;
        }
        AggregateFunction::GroupConcat => {
            column.coltype = ColumnType::MysqlTypeLongBlob;
            column.column_length = 1024 * if mysql80(facts) { 16 } else { 64 };
            set_flags(&mut column, 0);
        }
        _ => {}
    }
    column
}

#[allow(clippy::too_many_lines)]
fn expression(
    expr: &BoundExpr,
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Column {
    // A session-zone reading presents as the column it reads.
    if let Some(source) = expr.session_timestamp_source() {
        return expression(source, query, catalog, facts);
    }
    let mut column = base(expr);
    match &expr.kind {
        BoundExprKind::Column(reference) => {
            // Derived references resolve to their original expression, preserving declarations.
            for relation in query.from.iter().flat_map(|from| {
                std::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table))
            }) {
                if let Some(input) = &relation.input
                    && relation
                        .relation_name
                        .eq_ignore_ascii_case(&reference.relation_name)
                    && let Some(index) = relation
                        .columns
                        .iter()
                        .position(|value| value.column_id == reference.column_id)
                    && let Some(projection) = input.projection.get(index)
                {
                    let mut column = expression(&projection.expr, input, catalog, facts);
                    // A materialized derived table stores an integer of up to
                    // eleven digits as INT, whatever computed it (measured
                    // against MySQL 8.4); a merged one keeps the type.
                    if materialized_derived(input) && integer(&column) && column.column_length <= 11
                    {
                        column.coltype = ColumnType::MysqlTypeLong;
                    }
                    if input.recursive.is_some() {
                        column.column_length = column.column_length.saturating_add(1);
                        set_flags(&mut column, 0);
                    }
                    if query.from.iter().flat_map(|from| &from.joins).any(|join| {
                        join.kind == BoundJoinKind::Scalar
                            && join.table.relation_name == reference.relation_name
                    }) {
                        column.colflags.set(ColumnFlags::NOT_NULL_FLAG, false);
                    }
                    return column;
                }
            }
            if let Some(database) = catalog.database_by_id(reference.database_id)
                && let Some(table) = database.table_by_id(reference.table_id)
                && let Some(fact) = facts.columns.iter().find(|fact| {
                    fact.database.eq_ignore_ascii_case(database.name())
                        && fact.table.eq_ignore_ascii_case(table.name())
                        && fact.column.eq_ignore_ascii_case(&reference.name)
                })
            {
                database.name().clone_into(&mut column.schema);
                column.table.clone_from(&reference.relation_name);
                table.name().clone_into(&mut column.org_table);
                column.org_column.clone_from(&reference.name);
                source_declaration(&mut column, fact);
                let key = query
                    .from
                    .iter()
                    .flat_map(|from| {
                        std::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table))
                    })
                    .any(|relation| {
                        relation
                            .relation_name
                            .eq_ignore_ascii_case(&reference.relation_name)
                            && relation.key_column_ids.contains(&reference.column_id)
                    });
                if key {
                    column.colflags |= ColumnFlags::from_bits(2 | 16384);
                }
                if fact.unique_single && !key {
                    column.colflags |= ColumnFlags::from_bits(4 | 16384);
                }
                if query.from.iter().flat_map(|from| &from.joins).any(|join| {
                    join.table
                        .relation_name
                        .eq_ignore_ascii_case(&reference.relation_name)
                        && matches!(join.kind, BoundJoinKind::Left | BoundJoinKind::Scalar)
                }) {
                    column.colflags.set(ColumnFlags::NOT_NULL_FLAG, false);
                }
                let materialized = (!query.group_by.is_empty() && !ordered_group(query))
                    || !query.windows.is_empty()
                    || !query.union_all.is_empty()
                    || !query.set_ops.is_empty()
                    || query.from.iter().any(|from| {
                        from.base.input.is_some()
                            || from.joins.iter().any(|join| {
                                join.table.input.is_some()
                                    && matches!(
                                        join.kind,
                                        BoundJoinKind::Inner | BoundJoinKind::Left
                                    )
                            })
                    });
                if materialized {
                    let flags = column.colflags.bits() & !(2 | 4 | 8 | 512 | 16384);
                    set_flags(&mut column, flags);
                }
            }
            if reference.geometry {
                column.coltype = ColumnType::MysqlTypeGeometry;
                column.character_set = 63;
                column.column_length = u32::MAX;
                column.colflags |= ColumnFlags::from_bits(16 | 128);
            }
        }
        BoundExprKind::GroupKey(index) => {
            if let Some(expr) = query.group_by.get(*index) {
                column = expression(expr, query, catalog, facts);
                if !ordered_group(query) {
                    group_key(&mut column, resolved_expr(expr, query));
                }
            }
        }
        BoundExprKind::Aggregate(index) => {
            if let Some(index) = index.checked_sub(query.group_by.len()) {
                if let Some(value) = query.aggregates.get(index) {
                    column = aggregate(value, query, catalog, facts);
                    if !query.group_by.is_empty()
                        && !ordered_group(query)
                        && matches!(
                            value.function,
                            AggregateFunction::Count
                                | AggregateFunction::Sum
                                | AggregateFunction::Average
                        )
                    {
                        column.colflags.set(ColumnFlags::BINARY_FLAG, false);
                    }
                }
            } else if let Some(expr) = query.group_by.get(*index) {
                column = expression(expr, query, catalog, facts);
                if !ordered_group(query) {
                    group_key(&mut column, resolved_expr(expr, query));
                }
            }
        }
        BoundExprKind::Window(index) => {
            if let Some(window) = query.windows.get(*index) {
                match &window.function {
                    WindowFunction::Aggregate(value) => {
                        column = aggregate(value, query, catalog, facts);
                        column.colflags.set(ColumnFlags::BINARY_FLAG, false);
                    }
                    WindowFunction::Offset { expr, .. } | WindowFunction::Extreme { expr, .. } => {
                        column = expression(expr, query, catalog, facts);
                        let flags = column.colflags.bits() & (32 | 128);
                        set_flags(&mut column, flags);
                        if let WindowFunction::Offset {
                            default: Some(default),
                            ..
                        } = &window.function
                        {
                            column
                                .colflags
                                .set(ColumnFlags::NOT_NULL_FLAG, !default.nullable);
                        }
                        if column.coltype == ColumnType::MysqlTypeString {
                            column.coltype = ColumnType::MysqlTypeVarString;
                        }
                    }
                    _ => {
                        column.column_length = 21;
                        set_flags(&mut column, 33);
                    }
                }
            }
        }
        BoundExprKind::Literal(value) => match value {
            Value::Utf8(value) if expr.data_type == Some(DataType::Utf8) => {
                column.column_length = u32::try_from(value.chars().count())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(4);
                column.decimals = 31;
            }
            Value::Int64(value) => {
                column.column_length = u32::try_from(value.to_string().len()).unwrap_or(20);
            }
            Value::UInt64(value) => {
                column.column_length = u32::try_from(value.to_string().len()).unwrap_or(20);
            }
            _ => {}
        },
        BoundExprKind::Binary { op, left, right } => {
            let left_column = expression(left, query, catalog, facts);
            let right_column = expression(right, query, catalog, facts);
            let left_precision = expression_precision(left, &left_column);
            let right_precision = expression_precision(right, &right_column);
            let left = left_column;
            let right = right_column;
            if integer(&column)
                && matches!(op, BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply)
            {
                column.colflags.set(
                    ColumnFlags::UNSIGNED_FLAG,
                    unsigned(&left) && unsigned(&right),
                );
                let digits = if *op == BinaryOp::Multiply {
                    left_precision.saturating_add(right_precision)
                } else {
                    left_precision.max(right_precision).saturating_add(1)
                };
                column.column_length = digits.saturating_add(u32::from(!unsigned(&column)));
            }
            if column.coltype == ColumnType::MysqlTypeDouble {
                column.column_length = left.column_length.max(right.column_length).max(23);
            }
            if column.coltype == ColumnType::MysqlTypeNewdecimal {
                let lp = left_precision;
                let rp = right_precision;
                let ls = u32::from(left.decimals.min(30));
                let rs = u32::from(right.decimals.min(30));
                let (precision, scale) = match op {
                    BinaryOp::Add | BinaryOp::Subtract => {
                        ((lp - ls).max(rp - rs) + ls.max(rs) + 1, ls.max(rs))
                    }
                    BinaryOp::Multiply => (lp + rp, ls + rs),
                    BinaryOp::Divide => (lp + rs + 4, ls + 4),
                    _ => (precision(&column), u32::from(column.decimals)),
                };
                column.decimals = u8::try_from(scale.min(30)).unwrap_or(30);
                column.column_length = precision.min(65) + 1 + u32::from(column.decimals > 0);
            } else if matches!(
                op,
                BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessOrEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterOrEqual
                    | BinaryOp::And
                    | BinaryOp::Or
            ) {
                column.coltype = ColumnType::MysqlTypeLonglong;
                column.column_length = 1;
            }
            column.colflags.set(
                ColumnFlags::NOT_NULL_FLAG,
                left.colflags.contains(ColumnFlags::NOT_NULL_FLAG)
                    && right.colflags.contains(ColumnFlags::NOT_NULL_FLAG),
            );
        }
        BoundExprKind::Scalar { function, args } => {
            let inputs: Vec<_> = args
                .iter()
                .map(|arg| expression(arg, query, catalog, facts))
                .collect();
            let first = inputs.first();
            match function {
                ScalarFunction::DeclaredCast {
                    target: DataType::Utf8,
                    characters,
                } => {
                    column.column_length = characters.map_or_else(
                        || first.map_or(1024, |input| input.column_length),
                        |characters| characters.saturating_mul(4),
                    );
                }
                ScalarFunction::Cast(target)
                    if matches!(
                        target,
                        DataType::Int64 | DataType::UInt64 | DataType::Decimal { .. }
                    ) && first.is_some_and(|input| {
                        integer(input) || input.coltype == ColumnType::MysqlTypeNewdecimal
                    }) =>
                {
                    column = first.unwrap().clone();
                }
                ScalarFunction::Collate { .. } => {
                    if let Some(first) = first {
                        column = first.clone();
                    }
                }
                ScalarFunction::Upper
                | ScalarFunction::Lower
                | ScalarFunction::Trim
                | ScalarFunction::Substring
                | ScalarFunction::RegexpSubstr
                | ScalarFunction::SubstringIndex
                | ScalarFunction::Left
                | ScalarFunction::Right
                | ScalarFunction::Reverse
                | ScalarFunction::Cast(DataType::Utf8) => {
                    if let Some(first) = first {
                        column.column_length = first.column_length;
                    }
                }
                ScalarFunction::Concat => {
                    column.column_length = inputs.iter().fold(0u32, |width, input| {
                        width.saturating_add(input.column_length)
                    });
                }
                ScalarFunction::Hex => {
                    if let Some(first) = first {
                        column.column_length = first.column_length.saturating_mul(8);
                        if column.column_length > 65535 {
                            column.coltype = ColumnType::MysqlTypeLongBlob;
                        }
                    }
                }
                ScalarFunction::Md5 => column.column_length = 128,
                ScalarFunction::Sha1 => column.column_length = 160,
                ScalarFunction::Coalesce
                | ScalarFunction::NullIf
                | ScalarFunction::Greatest { .. }
                | ScalarFunction::Least { .. } => {
                    if let Some(widest) = inputs.iter().max_by_key(|column| column.column_length) {
                        column.column_length = widest.column_length;
                        if inputs.iter().all(integer) {
                            column.coltype = widest.coltype;
                            column
                                .colflags
                                .set(ColumnFlags::UNSIGNED_FLAG, inputs.iter().all(unsigned));
                        }
                    }
                }
                ScalarFunction::Round { .. } | ScalarFunction::Truncate { .. } => {
                    if let Some(first) = first {
                        column.colflags.set(
                            ColumnFlags::BINARY_FLAG,
                            first.colflags.contains(ColumnFlags::BINARY_FLAG),
                        );
                        if first.coltype == ColumnType::MysqlTypeDouble {
                            column.column_length = first.column_length;
                        }
                        if integer(first) {
                            column.coltype = ColumnType::MysqlTypeLonglong;
                            column.column_length = first.column_length;
                            column.decimals = 0;
                        }
                        if first.coltype == ColumnType::MysqlTypeNewdecimal {
                            column.coltype = first.coltype;
                            let scale = args
                                .get(1)
                                .and_then(literal_i64)
                                .unwrap_or(0)
                                .clamp(0, i64::from(first.decimals));
                            column.decimals = u8::try_from(scale).unwrap_or(0);
                            column.column_length = first.column_length - u32::from(first.decimals)
                                + u32::from(column.decimals)
                                + u32::from(
                                    column.decimals < first.decimals
                                        && matches!(function, ScalarFunction::Round { .. }),
                                )
                                - u32::from(column.decimals == 0 && first.decimals > 0);
                        }
                    }
                }
                ScalarFunction::PackedDateParts { width } => {
                    column.column_length = u32::from(*width);
                }
                ScalarFunction::DatePart(part) => {
                    column.coltype = ColumnType::MysqlTypeLonglong;
                    column.column_length = match part {
                        DatePart::Year if mysql80(facts) => 5,
                        DatePart::Year | DatePart::Hour => 4,
                        DatePart::Quarter | DatePart::DayOfWeek | DatePart::WeekDay => 2,
                        _ => 3,
                    };
                    column.colflags.set(
                        ColumnFlags::UNSIGNED_FLAG,
                        *part == DatePart::Year && !mysql80(facts),
                    );
                }
                ScalarFunction::DateInterval { .. } => {
                    if let Some(first) = first {
                        column.decimals = if matches!(
                            first.coltype,
                            ColumnType::MysqlTypeVarString | ColumnType::MysqlTypeString
                        ) {
                            6
                        } else {
                            first.decimals
                        };
                        if column.coltype == ColumnType::MysqlTypeDatetime {
                            column.column_length =
                                19 + u32::from(column.decimals > 0) + u32::from(column.decimals);
                        }
                    }
                }
                ScalarFunction::JsonUnquote | ScalarFunction::JsonExtract { unquote: true } => {
                    column.colflags |= ColumnFlags::BINARY_FLAG;
                    column.coltype = ColumnType::MysqlTypeLongBlob;
                    column.column_length = u32::MAX;
                }
                ScalarFunction::RegexpReplace => {
                    column.coltype = ColumnType::MysqlTypeLongBlob;
                    column.column_length = 64 * 1024 * 1024;
                }
                ScalarFunction::Length | ScalarFunction::CharLength => {
                    column.column_length = 10;
                    column.colflags.set(ColumnFlags::UNSIGNED_FLAG, false);
                }
                ScalarFunction::RegexpInstr
                | ScalarFunction::JsonLength
                | ScalarFunction::JsonContainsPath
                | ScalarFunction::TimestampDiff { .. } => {
                    column.column_length = 21;
                    column.colflags.set(ColumnFlags::UNSIGNED_FLAG, false);
                }
                ScalarFunction::Conv => column.column_length = 260,
                ScalarFunction::JsonType => {
                    column.column_length = 68;
                    column.colflags |= ColumnFlags::BINARY_FLAG;
                }
                ScalarFunction::JsonValue => {
                    column.column_length = 2048;
                    column.colflags |= ColumnFlags::BINARY_FLAG;
                }
                ScalarFunction::DayName | ScalarFunction::MonthName => column.column_length = 36,
                ScalarFunction::MakeTime
                | ScalarFunction::SecToTime
                | ScalarFunction::ConvertTz => {
                    column.coltype = if matches!(function, ScalarFunction::ConvertTz) {
                        ColumnType::MysqlTypeDatetime
                    } else {
                        ColumnType::MysqlTypeTime
                    };
                    column.character_set = 63;
                    let precision_input = if matches!(function, ScalarFunction::MakeTime) {
                        inputs.get(2)
                    } else {
                        first
                    };
                    column.decimals = precision_input.map_or(0, |input| input.decimals.min(6));
                    column.column_length = if matches!(function, ScalarFunction::ConvertTz) {
                        19
                    } else {
                        10
                    } + u32::from(column.decimals > 0)
                        + u32::from(column.decimals);
                    set_flags(&mut column, 128);
                }
                ScalarFunction::If => {
                    column.column_length = inputs
                        .iter()
                        .skip(1)
                        .map(|input| input.column_length)
                        .max()
                        .unwrap_or(column.column_length);
                }
                ScalarFunction::DateFormat => {
                    if let Some(BoundExpr {
                        kind: BoundExprKind::Literal(Value::Utf8(format)),
                        ..
                    }) = args.get(1)
                    {
                        column.column_length = date_format_width(format).saturating_mul(4);
                    }
                }
                ScalarFunction::RegexpLike { .. } => {
                    column.coltype = ColumnType::MysqlTypeLonglong;
                    column.column_length = 1;
                }
                _ => {}
            }
            if !matches!(
                function,
                ScalarFunction::Coalesce | ScalarFunction::NullIf | ScalarFunction::If
            ) && !inputs.is_empty()
            {
                column.colflags.set(
                    ColumnFlags::NOT_NULL_FLAG,
                    inputs
                        .iter()
                        .all(|input| input.colflags.contains(ColumnFlags::NOT_NULL_FLAG))
                        && !matches!(
                            function,
                            ScalarFunction::JsonExtract { .. }
                                | ScalarFunction::JsonUnquote
                                | ScalarFunction::Date
                                | ScalarFunction::DatePart(_)
                                | ScalarFunction::DateFormat
                                | ScalarFunction::DateInterval { .. }
                                | ScalarFunction::Conv
                                | ScalarFunction::RegexpReplace
                                | ScalarFunction::DayName
                                | ScalarFunction::MonthName
                                | ScalarFunction::LastDay
                                | ScalarFunction::SecToTime
                                | ScalarFunction::MakeTime
                                | ScalarFunction::ConvertTz
                        ),
                );
            }
        }
        _ => {}
    }
    if query
        .from
        .iter()
        .flat_map(|from| &from.joins)
        .any(|join| join.scalar_aggregate)
    {
        column.colflags.set(ColumnFlags::NOT_NULL_FLAG, false);
    }
    column
}

fn mysql80(facts: &SourceFacts) -> bool {
    facts
        .server_version
        .as_deref()
        .is_some_and(|version| version.starts_with("8.0."))
}

fn date_format_width(format: &str) -> u32 {
    let mut chars = format.chars();
    let mut width = 0u32;
    while let Some(ch) = chars.next() {
        width = width.saturating_add(if ch == '%' {
            match chars.next() {
                Some('M' | 'W') => 64,
                Some('D' | 'Y' | 'X' | 'x') => 4,
                Some('a' | 'b') => 32,
                Some('j') => 3,
                Some('r') => 11,
                Some('T') => 8,
                Some('f') => 6,
                Some('w' | '%') => 1,
                Some(_) => 2,
                None => 0,
            }
        } else {
            1
        });
    }
    width
}

fn grouped_scalar(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::GroupKey(_) | BoundExprKind::Literal(_) => true,
        BoundExprKind::Scalar { args, .. } => args.iter().all(grouped_scalar),
        BoundExprKind::Binary { left, right, .. } => grouped_scalar(left) && grouped_scalar(right),
        _ => false,
    }
}

fn ordered_group(query: &BoundQuery) -> bool {
    let Some(from) = query.from.first() else {
        return false;
    };
    query.aggregates.iter().any(|aggregate| aggregate.distinct)
        || (!query.group_by.is_empty()
            && from.joins.is_empty()
            && !from.base.key_column_ids.is_empty()
            && query
                .group_by
                .iter()
                .zip(&from.base.key_column_ids)
                .all(|(expr, key)| {
                    matches!(&expr.kind, BoundExprKind::Column(column)
                    if column.column_id == *key && column.relation_name == from.base.relation_name)
                }))
}

/// The expression a column reference stands for when it reads a derived
/// table's projection, followed down through nested derived tables; a
/// base-table column stands for itself. Grouping materializes a computed
/// integer as INT in `MySQL` whether it is grouped where it is computed or
/// read back from a derived table first, while a stored column keeps its
/// declared type either way.
fn resolved_expr<'a>(expr: &'a BoundExpr, query: &'a BoundQuery) -> &'a BoundExpr {
    let BoundExprKind::Column(reference) = &expr.kind else {
        return expr;
    };
    for relation in query.from.iter().flat_map(|from| {
        std::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table))
    }) {
        if let Some(input) = &relation.input
            && relation
                .relation_name
                .eq_ignore_ascii_case(&reference.relation_name)
            && let Some(index) = relation
                .columns
                .iter()
                .position(|value| value.column_id == reference.column_id)
            && let Some(projection) = input.projection.get(index)
        {
            return match projection.expr.kind {
                BoundExprKind::GroupKey(key) => input
                    .group_by
                    .get(key)
                    .map_or(&projection.expr, |grouped| resolved_expr(grouped, input)),
                _ => resolved_expr(&projection.expr, input),
            };
        }
    }
    expr
}

/// Whether `MySQL` materializes a derived table rather than merging it into
/// the outer query: grouping, aggregates, DISTINCT, windows, LIMIT and set
/// operations all prevent the merge.
fn materialized_derived(input: &BoundQuery) -> bool {
    !input.group_by.is_empty()
        || !input.aggregates.is_empty()
        || input.distinct
        || !input.windows.is_empty()
        || input.limit.is_some()
        || !input.union_all.is_empty()
        || !input.set_ops.is_empty()
}

fn group_key(column: &mut Column, expr: &BoundExpr) {
    if !matches!(expr.kind, BoundExprKind::Column(_)) {
        if column.character_set != 63 {
            column.decimals = 0;
        }
        if column.coltype == ColumnType::MysqlTypeLongBlob {
            column.coltype = ColumnType::MysqlTypeBlob;
            column.colflags |= ColumnFlags::from_bits(16);
        }
        // The grouping temporary table stores an integer of up to eleven
        // digits as INT; a wider one - `id + 1`, NTILE - stays BIGINT.
        if integer(column) && column.column_length <= 11 {
            column.coltype = ColumnType::MysqlTypeLong;
            column.colflags.set(ColumnFlags::BINARY_FLAG, false);
        }
    }
    if !column.colflags.contains(ColumnFlags::NOT_NULL_FLAG) {
        column.colflags |= ColumnFlags::from_bits(32768);
    }
}

fn expression_precision(expr: &BoundExpr, column: &Column) -> u32 {
    if let BoundExprKind::Literal(Value::Int64(value)) = expr.kind {
        return u32::try_from(value.unsigned_abs().to_string().len()).unwrap_or(20);
    }
    precision(column)
}

fn precision(column: &Column) -> u32 {
    if integer(column) && column.column_length == 1 {
        return 1;
    }
    column
        .column_length
        .saturating_sub(u32::from(!unsigned(column)))
        .saturating_sub(u32::from(column.decimals > 0))
}

fn literal_i64(expr: &BoundExpr) -> Option<i64> {
    match &expr.kind {
        BoundExprKind::Literal(Value::Int64(value)) => Some(*value),
        BoundExprKind::Literal(Value::UInt64(value)) => i64::try_from(*value).ok(),
        _ => None,
    }
}

fn source_declaration(column: &mut Column, fact: &pintail_sql::ColumnFacts) {
    let declaration = fact.mysql_column_type.as_deref().unwrap_or_default();
    let kind = fact.mysql_data_type.as_deref().unwrap_or_default();
    let width = declaration
        .split_once('(')
        .and_then(|(_, tail)| tail.split(')').next())
        .and_then(|width| width.parse::<u32>().ok());
    let unsigned = declaration.contains("unsigned");
    let mut flags = u16::from(fact.nullable == Some(false)) | if unsigned { 32 } else { 0 };
    if fact.auto_increment {
        flags |= 512;
    }
    if fact.nullable == Some(false)
        && fact.default_value.is_none()
        && !fact.auto_increment
        && !fact.generated_stored
    {
        flags |= 4096;
    }
    if fact.extra.contains("on update") {
        flags |= 8192;
    }
    match kind {
        "tinyint" => column.column_length = width.unwrap_or(if unsigned { 3 } else { 4 }),
        "smallint" => column.column_length = width.unwrap_or(if unsigned { 5 } else { 6 }),
        "mediumint" => column.column_length = width.unwrap_or(if unsigned { 8 } else { 9 }),
        "int" | "integer" => column.column_length = width.unwrap_or(if unsigned { 10 } else { 11 }),
        "bigint" => column.column_length = width.unwrap_or(20),
        "varchar" | "char" => {
            column.column_length = width.unwrap_or(256).saturating_mul(4);
            if kind == "char" {
                column.coltype = ColumnType::MysqlTypeString;
            }
        }
        "enum" | "set" => {
            column.coltype = ColumnType::MysqlTypeString;
            let labels = pintail_types::declaration_labels(declaration, kind).unwrap_or_default();
            let length = if kind == "enum" {
                labels
                    .iter()
                    .map(|label| label.chars().count())
                    .max()
                    .unwrap_or(0)
            } else {
                labels
                    .iter()
                    .map(|label| label.chars().count())
                    .sum::<usize>()
                    + labels.len().saturating_sub(1)
            };
            column.column_length = u32::try_from(length).unwrap_or(u32::MAX).saturating_mul(4);
            flags |= if kind == "enum" { 256 } else { 2048 };
        }
        "datetime" | "date" | "time" => flags |= 128,
        "timestamp" => {
            column.coltype = ColumnType::MysqlTypeTimestamp;
            flags |= 128;
            // MySQL 8.4 marks a TIMESTAMP column with TIMESTAMP_FLAG only when
            // it initializes or updates itself - DEFAULT CURRENT_TIMESTAMP or
            // ON UPDATE - not every TIMESTAMP (measured against the server).
            let initializes = fact.default_value.as_deref().is_some_and(|default| {
                default
                    .to_ascii_uppercase()
                    .starts_with("CURRENT_TIMESTAMP")
            });
            if initializes || fact.extra.contains("on update") {
                flags |= 1024;
            }
        }
        "json" => {
            flags |= 128 | 16;
            column.character_set = 63;
            column.column_length = u32::MAX;
        }
        _ => {}
    }
    if matches!(kind, "varchar" | "char" | "enum" | "set" | "json") {
        column.decimals = 0;
    }
    set_flags(column, flags);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spatial_and_text_carried_temporal_declarations_survive_binding() {
        use pintail_catalog::{DatabaseEntry, DatabaseId, TableEntry, TableId};
        use pintail_types::{Column as SchemaColumn, TableSchema};
        let schema = TableSchema::new(
            1,
            vec![
                SchemaColumn::new(0, "location", DataType::Binary, false).with_geometry(true),
                SchemaColumn::new(1, "at", DataType::DateTime64 { fsp: 6 }, true),
            ],
        )
        .unwrap();
        let catalog = CatalogSnapshot::new([DatabaseEntry::new(
            DatabaseId::new(1),
            "sample",
            [TableEntry::new(
                TableId::new(1),
                "markers",
                schema,
                pintail_catalog::TableStatistics::default(),
            )
            .unwrap()],
        )
        .unwrap()])
        .unwrap();
        let statement = pintail_sql::parse_statement(
            "SELECT location, HEX(location), SEC_TO_TIME(3661), CONVERT_TZ(at, '+00:00', '+05:30') FROM markers"
        ).unwrap();
        let query = pintail_sql::Binder::new(&catalog, Some("sample"))
            .bind(&statement)
            .unwrap();
        let fields = columns(&query, &catalog, &SourceFacts::default());
        assert_eq!(
            (fields[0].coltype, fields[0].character_set),
            (ColumnType::MysqlTypeGeometry, 63)
        );
        assert_eq!(
            (fields[1].coltype, fields[1].column_length),
            (ColumnType::MysqlTypeLongBlob, u32::MAX)
        );
        assert_eq!(
            (
                fields[2].coltype,
                fields[2].character_set,
                fields[2].decimals
            ),
            (ColumnType::MysqlTypeTime, 63, 0)
        );
        assert_eq!(
            (
                fields[3].coltype,
                fields[3].character_set,
                fields[3].decimals
            ),
            (ColumnType::MysqlTypeDatetime, 63, 6)
        );
    }

    #[test]
    fn source_version_controls_year_and_group_concat_declarations() {
        let catalog = CatalogSnapshot::new([]).unwrap();
        for (version, year_width, unsigned_year, concat_width) in
            [("8.0.46", 5, false, 16384), ("8.4.8", 4, true, 65536)]
        {
            let facts = SourceFacts {
                server_version: Some(version.to_owned()),
                ..SourceFacts::default()
            };
            let statement =
                pintail_sql::parse_statement("SELECT YEAR('2025-01-02'), GROUP_CONCAT('x')")
                    .unwrap();
            let query = pintail_sql::Binder::new(&catalog, None)
                .bind(&statement)
                .unwrap();
            let fields = columns(&query, &catalog, &facts);
            assert_eq!(fields[0].column_length, year_width);
            assert_eq!(
                fields[0].colflags.contains(ColumnFlags::UNSIGNED_FLAG),
                unsigned_year
            );
            assert_eq!(fields[1].column_length, concat_width);
        }
    }

    #[test]
    fn declared_decimal_scale_and_temporal_precision_survive_binding() {
        let catalog = CatalogSnapshot::new([]).unwrap();
        let statement = pintail_sql::parse_statement("SELECT CAST(1 AS DECIMAL(18,4)), CAST('2024-01-02 03:04:05.123456' AS DATETIME(6)), COUNT(*)").unwrap();
        let query = pintail_sql::Binder::new(&catalog, None)
            .bind(&statement)
            .unwrap();
        let fields = columns(&query, &catalog, &SourceFacts::default());
        assert_eq!(
            (
                fields[0].coltype,
                fields[0].decimals,
                fields[0].column_length
            ),
            (ColumnType::MysqlTypeNewdecimal, 4, 20)
        );
        assert_eq!(
            (fields[1].coltype, fields[1].decimals),
            (ColumnType::MysqlTypeDatetime, 6)
        );
        assert!(fields[2].colflags.contains(ColumnFlags::NOT_NULL_FLAG));
        assert!(!fields[2].colflags.contains(ColumnFlags::UNSIGNED_FLAG));
    }
    /// An integer expression grouped on is materialized in the `MySQL` grouping
    /// temporary table as INT, and keeps that type when an outer query reads
    /// it back through a derived table or groups on it there (measured
    /// against `MySQL` 8.4: `YEAR(...)` as a grouping key reads as LONG).
    #[test]
    fn a_grouped_integer_expression_reads_as_int_through_a_derived_table() {
        use pintail_catalog::{DatabaseEntry, DatabaseId, TableEntry, TableId};
        use pintail_types::{Column as SchemaColumn, TableSchema};
        let schema = TableSchema::new(
            1,
            vec![
                SchemaColumn::new(0, "id", DataType::Int64, false),
                SchemaColumn::new(1, "at", DataType::DateTime64 { fsp: 0 }, false),
            ],
        )
        .unwrap();
        let catalog = CatalogSnapshot::new([DatabaseEntry::new(
            DatabaseId::new(1),
            "sample",
            [TableEntry::new(
                TableId::new(1),
                "sales",
                schema,
                pintail_catalog::TableStatistics::default(),
            )
            .unwrap()
            .with_key_columns([0])
            .unwrap()],
        )
        .unwrap()])
        .unwrap();
        let types = |sql: &str| {
            let statement = pintail_sql::parse_statement(sql).unwrap();
            let query = pintail_sql::Binder::new(&catalog, Some("sample"))
                .bind(&statement)
                .unwrap();
            columns(&query, &catalog, &SourceFacts::default())
                .into_iter()
                .map(|column| column.coltype)
                .collect::<Vec<_>>()
        };
        let inner = "SELECT s.id, YEAR(CONVERT_TZ(s.at, '+00:00', '+09:30')) AS year, \
                     MONTH(CONVERT_TZ(s.at, '+00:00', '+09:30')) AS month, COUNT(*) AS n \
                     FROM sales s JOIN sales o ON o.id = s.id GROUP BY s.id, year, month";
        assert_eq!(
            types(&format!("SELECT t.year, t.month FROM ({inner}) t")),
            vec![ColumnType::MysqlTypeLong, ColumnType::MysqlTypeLong],
            "grouped inside the derived table"
        );
        assert_eq!(
            types(
                "SELECT t.year, COUNT(*) FROM (SELECT YEAR(CONVERT_TZ(at, '+00:00', '+09:30')) \
                 AS year FROM sales) t GROUP BY t.year"
            )[0],
            ColumnType::MysqlTypeLong,
            "grouped on outside it"
        );
        assert_eq!(
            types(
                "SELECT t.w, COUNT(*) FROM (SELECT NTILE(4) OVER (ORDER BY id) AS w FROM sales) t \
                 GROUP BY t.w"
            )[0],
            ColumnType::MysqlTypeLonglong,
            "a BIGINT-wide integer stays BIGINT when grouped"
        );
        assert_eq!(
            types("SELECT t.id FROM (SELECT id, COUNT(*) AS n FROM sales GROUP BY id) t")[0],
            ColumnType::MysqlTypeLonglong,
            "a stored BIGINT keeps its type through a grouped derived table"
        );
        assert_eq!(
            types("SELECT t.y FROM (SELECT YEAR(at) AS y FROM sales) t")[0],
            ColumnType::MysqlTypeLonglong,
            "a merged derived table keeps the computed type"
        );
    }
}
