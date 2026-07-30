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
    /// Typed scalar literal.
    Literal(Value),
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
    /// Optional post-aggregation predicate.
    pub having: Option<BoundExpr>,
    /// Whether duplicate output rows must be removed.
    pub distinct: bool,
    /// Ordered result-layout sort keys.
    pub order_by: Vec<BoundOrderKey>,
    /// Additional type-compatible SELECT branches concatenated in source order.
    pub union_all: Vec<BoundQuery>,
    /// Optional normalized row limit.
    pub limit: Option<BoundLimit>,
}
