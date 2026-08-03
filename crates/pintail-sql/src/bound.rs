use pintail_catalog::{DatabaseId, TableId};
use pintail_types::{DataType, Value};

/// A table made unambiguous against one catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundTable {
    /// Stable source database ID.
    pub database_id: DatabaseId,
    /// Stable physical table ID.
    pub table_id: TableId,
    /// Source database spelling.
    pub database_name: String,
    /// Source table spelling.
    pub table_name: String,
    /// Query-visible relation name, including any alias.
    pub relation_name: String,
    /// Catalog schema version used while binding.
    pub schema_version: u32,
    /// Columns visible through this relation.
    pub columns: Vec<BoundColumn>,
    /// Exact catalog row count, when available.
    pub row_count: Option<u64>,
    /// Best-effort row-count estimate (exact when known); used only for
    /// planning guards, never to answer queries.
    pub estimated_rows: Option<u64>,
    /// Stable columns that produce the physical storage key.
    pub key_column_ids: Vec<u32>,
    /// Bound input for a derived table or common table expression.
    ///
    /// Catalog-backed tables leave this empty and become storage scans.
    pub input: Option<Box<BoundQuery>>,
}

/// A column made unambiguous against one catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundColumn {
    /// Stable source database ID.
    pub database_id: DatabaseId,
    /// Stable physical table ID.
    pub table_id: TableId,
    /// Stable physical column ID.
    pub column_id: u32,
    /// Query-visible relation name.
    pub relation_name: String,
    /// Source column spelling.
    pub name: String,
    /// Logical scalar type.
    pub data_type: DataType,
    /// Whether the value can be `NULL`.
    pub nullable: bool,
}

/// A fully name-resolved and type-checked scalar expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundExpr {
    /// Expression operation and operands.
    pub kind: BoundExprKind,
    /// Logical scalar type, or `None` for an untyped `NULL` literal.
    pub data_type: Option<DataType>,
    /// Whether evaluating the expression can produce `NULL`.
    pub nullable: bool,
}

/// Operations represented by a bound scalar expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundExprKind {
    /// Stable catalog column reference.
    Column(BoundColumn),
    /// Positional grouping-key reference after hash aggregation.
    GroupKey(usize),
    /// Absolute positional aggregate-result reference after hash aggregation.
    Aggregate(usize),
    /// Positional reference into [`BoundQuery::windows`].
    Window(usize),
    /// Typed scalar literal.
    Literal(Value),
    /// One-column, at-most-one-row uncorrelated query.
    ScalarSubquery(Box<BoundQuery>),
    /// Uncorrelated `EXISTS` / `NOT EXISTS` test.
    ExistsSubquery {
        /// Query whose row presence is tested.
        query: Box<BoundQuery>,
        /// Whether the test is `NOT EXISTS`.
        negated: bool,
    },
    /// Uncorrelated one-column query used for SQL membership.
    InSubquery {
        /// Outer value tested against the materialized result.
        expr: Box<BoundExpr>,
        /// Query producing candidate values.
        query: Box<BoundQuery>,
        /// Whether membership is negated.
        negated: bool,
    },
    /// Unary scalar operation.
    Unary {
        /// Operation.
        op: UnaryOp,
        /// Operand.
        expr: Box<BoundExpr>,
    },
    /// Binary scalar operation.
    Binary {
        /// Operation.
        op: BinaryOp,
        /// Left operand.
        left: Box<BoundExpr>,
        /// Right operand.
        right: Box<BoundExpr>,
    },
    /// `IS NULL` or `IS NOT NULL`.
    IsNull {
        /// Operand.
        expr: Box<BoundExpr>,
        /// Whether this is `IS NOT NULL`.
        negated: bool,
    },
    /// Built-in scalar function or lowered SQL expression.
    Scalar {
        /// Scalar operation.
        function: ScalarFunction,
        /// Ordered operands.
        args: Vec<BoundExpr>,
    },
}

/// Built-in scalar operations supported by the v1 executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalarFunction {
    /// Concatenate strings, returning NULL when any argument is NULL.
    Concat,
    /// Extract a one-based string slice.
    Substring,
    /// Unicode lowercase conversion.
    Lower,
    /// Unicode uppercase conversion.
    Upper,
    /// Trim surrounding whitespace.
    Trim,
    /// UTF-8 byte length.
    Length,
    /// Unicode scalar-value count.
    CharLength,
    /// Replace every substring occurrence.
    Replace,
    /// Return the leftmost characters.
    Left,
    /// Return the rightmost characters.
    Right,
    /// Find a substring using one-based positions.
    Locate,
    /// `MySQL` truth-valued conditional.
    If,
    /// Return the first non-NULL argument.
    Coalesce,
    /// Return NULL when two arguments compare equal.
    NullIf,
    /// Round a numeric value to an optional decimal precision.
    Round,
    /// `CEIL(x)` / `CEILING(x)`.
    Ceil,
    /// `FLOOR(x)`.
    Floor,
    /// `ABS(x)`: exact for integers and decimals, f64 otherwise. `decimal`
    /// marks a canonical-decimal-text operand whose sign strips exactly.
    Abs {
        /// Whether the operand is DECIMAL-typed canonical text.
        decimal: bool,
    },
    /// `SIGN(x)`: -1, 0, or 1 as an integer.
    Sign,
    /// `POWER(base, exponent)` / `POW`.
    Power,
    /// `SQRT(x)`: NULL for negative input, like `MySQL`.
    Sqrt,
    /// `EXP(x)`.
    Exp,
    /// `LN(x)` / single-argument `LOG(x)`: NULL for non-positive input.
    Ln,
    /// `LOG(base, x)`: NULL outside the valid domain.
    LogBase,
    /// `LOG2(x)`.
    Log2,
    /// `LOG10(x)`.
    Log10,
    /// `TRUNCATE(x, digits)`: toward zero at the given precision.
    Truncate,
    /// `GREATEST(...)`; `decimal` selects numeric comparison for canonical
    /// decimal text operands.
    Greatest {
        /// Whether operands unified to DECIMAL and compare numerically.
        decimal: bool,
    },
    /// `LEAST(...)`; see [`ScalarFunction::Greatest`].
    Least {
        /// Whether operands unified to DECIMAL and compare numerically.
        decimal: bool,
    },
    /// `CONCAT_WS(separator, ...)`: NULL arguments are skipped, a NULL
    /// separator returns NULL.
    ConcatWs,
    /// `REVERSE(str)` by characters.
    Reverse,
    /// `REPEAT(str, count)`; results are capped at 4096 bytes.
    Repeat,
    /// `SPACE(n)`; capped like `REPEAT`.
    Space,
    /// `LPAD(str, len, pad)`; capped like `REPEAT`.
    Lpad,
    /// `RPAD(str, len, pad)`; capped like `REPEAT`.
    Rpad,
    /// `INSTR(str, substr)`: `LOCATE` with swapped arguments.
    Instr,
    /// `FIND_IN_SET(needle, comma_list)`.
    FindInSet,
    /// `ASCII(str)`: first byte, 0 for empty.
    Ascii,
    /// `ORD(str)`: numeric value of the leading character's bytes.
    Ord,
    /// `HEX(value)`: uppercase hex of string bytes or integer value.
    Hex,
    /// `UNHEX(str)`: binary from hex text, NULL when malformed.
    Unhex,
    /// `ELT(n, ...)`: 1-based pick, NULL out of range.
    Elt,
    /// `FIELD(needle, ...)`: 1-based position, 0 when absent.
    Field,
    /// `FORMAT(x, d)`: `en_US` grouping with `d` fraction digits.
    Format,
    /// `TO_BASE64(str)`.
    ToBase64,
    /// `FROM_BASE64(str)`: NULL when malformed.
    FromBase64,
    /// `DAYNAME(date)`.
    DayName,
    /// `MONTHNAME(date)`.
    MonthName,
    /// `LAST_DAY(date)`: last day of the month.
    LastDay,
    /// `TO_DAYS(date)`: days since year 0.
    ToDays,
    /// `FROM_DAYS(n)`: inverse of `TO_DAYS`.
    FromDays,
    /// `YEARWEEK(date)` default mode 0.
    YearWeek,
    /// `TIME_TO_SEC(time)`.
    TimeToSec,
    /// `SEC_TO_TIME(seconds)`, clamped to `MySQL`'s TIME range.
    SecToTime,
    /// `MAKEDATE(year, dayofyear)`: NULL when day < 1.
    MakeDate,
    /// `CURTIME()`.
    Curtime,
    /// `STR_TO_DATE(text, format)`: NULL when the text does not match.
    StrToDate,
    /// `expr REGEXP pattern` / `REGEXP_LIKE`, case-insensitive by default
    /// like the ci collations.
    RegexpLike {
        /// Whether the match is negated (`NOT REGEXP`).
        negated: bool,
    },
    /// `REGEXP_SUBSTR(expr, pattern)`: first match or NULL.
    RegexpSubstr,
    /// `REGEXP_INSTR(expr, pattern)`: 1-based match position or 0.
    RegexpInstr,
    /// `REGEXP_REPLACE(expr, pattern, replacement)`.
    RegexpReplace,
    /// `JSON_EXTRACT(json, path)` / the `->` operator; `unquote` marks the
    /// `->>` / `JSON_UNQUOTE(JSON_EXTRACT(...))` form.
    JsonExtract {
        /// Whether a scalar string result is unquoted (`->>`).
        unquote: bool,
    },
    /// `JSON_UNQUOTE(json)`.
    JsonUnquote,
    /// `TIMESTAMPDIFF(unit, from, to)`: complete units from `from` to
    /// `to`, truncated toward zero, matching `MySQL`.
    TimestampDiff {
        /// Calendar or clock unit being counted.
        unit: IntervalUnit,
    },
    /// Case-insensitive SQL pattern matching.
    Like {
        /// Whether the result is negated.
        negated: bool,
        /// Optional single-character escape.
        escape: Option<char>,
    },
    /// SQL list membership.
    InList {
        /// Whether the result is negated.
        negated: bool,
    },
    /// Inclusive range membership.
    Between {
        /// Whether the result is negated.
        negated: bool,
    },
    /// Explicit scalar conversion.
    Cast(DataType),
    /// Current local date and time.
    Now,
    /// Current local date.
    CurrentDate,
    /// Extract the date component.
    Date,
    /// Extract a calendar/time component.
    DatePart(DatePart),
    /// Format a date/time with a `MySQL` format string.
    DateFormat,
    /// Add or subtract one date/time interval.
    DateInterval {
        /// Interval unit.
        unit: IntervalUnit,
        /// Whether the interval is subtracted.
        subtract: bool,
    },
    /// Whole-day difference between two dates.
    DateDiff,
    /// Convert date/time to a Unix timestamp.
    UnixTimestamp,
    /// Convert a Unix timestamp to local date/time.
    FromUnixTime,
}

/// Calendar component extracted from a `MySQL` date/time value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatePart {
    /// Four-digit year.
    Year,
    /// Month number.
    Month,
    /// Day of month.
    Day,
    /// Hour.
    Hour,
    /// Minute.
    Minute,
    /// Second.
    Second,
    /// Quarter (1-4).
    Quarter,
    /// `DAYOFWEEK`: 1 = Sunday ... 7 = Saturday.
    DayOfWeek,
    /// `WEEKDAY`: 0 = Monday ... 6 = Sunday.
    WeekDay,
    /// Day of year (1-366).
    DayOfYear,
    /// `WEEK` default mode 0: Sunday-start weeks, range 0-53.
    Week,
    /// ISO 8601 week (`WEEK` mode 3 / `WEEKOFYEAR`).
    IsoWeek,
}

/// Single-field `MySQL` interval units supported by date arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntervalUnit {
    /// Calendar years.
    Year,
    /// Calendar months.
    Month,
    /// Calendar days.
    Day,
    /// Hours.
    Hour,
    /// Minutes.
    Minute,
    /// Seconds.
    Second,
}

/// Aggregate functions supported by the v1 query engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateFunction {
    /// Count all rows or non-NULL argument values.
    Count,
    /// Sum non-NULL numeric values.
    Sum,
    /// Average non-NULL numeric values.
    Average,
    /// Minimum non-NULL value.
    Minimum,
    /// Maximum non-NULL value.
    Maximum,
    /// Concatenate non-NULL string values.
    GroupConcat,
}

/// One deduplicated aggregate computation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundAggregate {
    /// Aggregate operation.
    pub function: AggregateFunction,
    /// Optional input expression. `COUNT(*)` has no expression.
    pub expr: Option<BoundExpr>,
    /// Whether duplicate non-NULL input values are ignored.
    pub distinct: bool,
    /// Aggregate result type.
    pub data_type: Option<DataType>,
    /// Whether an empty input can produce `NULL`.
    pub nullable: bool,
    /// `GROUP_CONCAT` separator; `None` is `MySQL`'s default comma.
    pub separator: Option<String>,
    /// `GROUP_CONCAT ... ORDER BY` keys as `(expression, ascending)`.
    pub order_within: Vec<(BoundExpr, bool)>,
}

/// One window ordering key evaluated against source rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWindowOrderKey {
    /// Key expression.
    pub expr: BoundExpr,
    /// Whether smaller values appear first.
    pub ascending: bool,
    /// Whether NULL values appear before non-NULL values.
    pub nulls_first: bool,
}

/// Supported window computations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowFunction {
    /// 1-based position within the partition in window order.
    RowNumber,
    /// Rank with gaps.
    Rank,
    /// Rank without gaps.
    DenseRank,
    /// An aggregate evaluated over the window frame: the whole partition
    /// without `ORDER BY`, or the running frame up to the current row's
    /// peers with it (`MySQL`'s default frame).
    Aggregate(BoundAggregate),
}

/// One window computation over partitioned, ordered source rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWindow {
    /// Window computation.
    pub function: WindowFunction,
    /// Partition expressions evaluated against source rows.
    pub partition_by: Vec<BoundExpr>,
    /// In-partition ordering.
    pub order_by: Vec<BoundWindowOrderKey>,
    /// Result type.
    pub data_type: Option<DataType>,
    /// Whether the result can be NULL.
    pub nullable: bool,
}

/// Set operations beyond UNION.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundSetOpKind {
    /// Distinct rows present in both inputs.
    Intersect,
    /// Distinct rows of the left input absent from the right.
    Except,
    /// Each row repeated `min(left_count, right_count)` times.
    IntersectAll,
    /// Each row repeated `max(0, left_count - right_count)` times.
    ExceptAll,
}

/// Supported unary scalar operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    /// Numeric identity.
    Plus,
    /// Numeric negation.
    Minus,
    /// `MySQL` truth-value negation.
    Not,
}

/// Supported binary scalar operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    /// Addition.
    Add,
    /// Subtraction.
    Subtract,
    /// Multiplication.
    Multiply,
    /// Floating-point division.
    Divide,
    /// Integer division.
    IntegerDivide,
    /// Remainder.
    Modulo,
    /// Equality comparison.
    Equal,
    /// Inequality comparison.
    NotEqual,
    /// Less-than comparison.
    Less,
    /// Less-than-or-equal comparison.
    LessOrEqual,
    /// Greater-than comparison.
    Greater,
    /// Greater-than-or-equal comparison.
    GreaterOrEqual,
    /// Boolean conjunction.
    And,
    /// Boolean disjunction.
    Or,
    /// Boolean exclusive-or.
    Xor,
}

/// One named output expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundProjection {
    /// Output name exposed to clients.
    pub name: String,
    /// Expression producing the output column.
    pub expr: BoundExpr,
}

/// A normalized non-negative row limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundLimit {
    /// Number of rows to skip.
    pub offset: u64,
    /// Maximum number of rows to return.
    pub count: u64,
}

/// One ordering key resolved against the projected result layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundOrderKey {
    /// Zero-based projected output position.
    pub index: usize,
    /// Whether smaller values appear first.
    pub ascending: bool,
    /// Whether NULL values appear before non-NULL values.
    pub nulls_first: bool,
    /// Whether the key is DECIMAL-typed: canonical decimal text must order
    /// numerically, not lexically.
    pub decimal: bool,
}

/// One comma-separated `FROM` item and its left-deep explicit join chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundFrom {
    /// First table in the chain.
    pub base: BoundTable,
    /// Explicit joins applied from left to right.
    pub joins: Vec<BoundJoin>,
}

/// A resolved explicit join.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundJoin {
    /// Join semantics.
    pub kind: BoundJoinKind,
    /// Right-hand table.
    pub table: BoundTable,
    /// Optional ON predicate. Cross joins have no predicate.
    pub condition: Option<BoundExpr>,
}

/// Join semantics supported by the v1 executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundJoinKind {
    /// Emit matching row pairs.
    Inner,
    /// Emit matches and unmatched left rows.
    Left,
    /// Emit each left row with at least one match.
    Semi,
    /// Emit each left row with no match.
    Anti,
    /// Emit every row pair.
    Cross,
}

/// A first-stage bound query ready for logical planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundQuery {
    /// Comma-separated source items with explicit join chains.
    pub from: Vec<BoundFrom>,
    /// Catalog tables referenced by the query.
    pub tables: Vec<BoundTable>,
    /// Ordered client-visible expressions.
    pub projection: Vec<BoundProjection>,
    /// Optional row predicate.
    pub filter: Option<BoundExpr>,
    /// Ordered grouping expressions evaluated against source rows.
    pub group_by: Vec<BoundExpr>,
    /// Deduplicated aggregate computations.
    pub aggregates: Vec<BoundAggregate>,
    /// Window computations referenced by the projection.
    pub windows: Vec<BoundWindow>,
    /// Optional post-aggregation predicate.
    pub having: Option<BoundExpr>,
    /// Whether duplicate output rows must be removed.
    pub distinct: bool,
    /// Ordered result-layout sort keys.
    pub order_by: Vec<BoundOrderKey>,
    /// Trailing projection columns that exist only so ORDER BY can sort by
    /// unprojected source columns; they are trimmed after the sort.
    pub hidden_sort_columns: usize,
    /// Additional type-compatible SELECT branches concatenated in source order.
    pub union_all: Vec<BoundQuery>,
    /// Whether the union chain deduplicates rows (`UNION [DISTINCT]`).
    pub union_distinct: bool,
    /// `INTERSECT` / `EXCEPT` chain applied left-associatively after the
    /// union chain (`MySQL` distinct set semantics).
    pub set_ops: Vec<(BoundSetOpKind, BoundQuery)>,
    /// Optional normalized row limit.
    pub limit: Option<BoundLimit>,
}
