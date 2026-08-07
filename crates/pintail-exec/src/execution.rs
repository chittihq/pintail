use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet, hash_map::Entry},
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    mem::{size_of, size_of_val},
    time::Instant,
};

const HASH_ENTRY_OVERHEAD: usize = 3 * size_of::<usize>();

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundJoinKind, BoundOrderKey, BoundProjection, BoundQuery, BoundWindow, DatePart,
    ScalarFunction, WindowFunction,
};
use pintail_types::{DataType, Value};
use rayon::prelude::*;

use crate::{
    BatchError, ColumnVector, DEFAULT_BATCH_ROWS, LogicalPlan, LogicalPlanner, Optimizer,
    RecordBatch, Scan,
    expression::{
        CompiledExpr, bound_regex_memory_upper_bound, compare_mysql, compare_utf8_mysql, mysql_f64,
        mysql_i64, mysql_u64, predicate_truth,
    },
    spill,
};

const DEFAULT_GROUP_CONCAT_MAX_LEN: usize = 1024;

thread_local! {
    static SESSION_GROUP_CONCAT_MAX_LEN: std::cell::Cell<usize> =
        const { std::cell::Cell::new(DEFAULT_GROUP_CONCAT_MAX_LEN) };
    static SESSION_GROUP_CONCAT_WARNINGS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
    static SESSION_CTE_MAX_RECURSION_DEPTH: std::cell::Cell<u64> =
        const { std::cell::Cell::new(DEFAULT_CTE_MAX_RECURSION_DEPTH) };
}

/// Installs the current connection's `group_concat_max_len` on this
/// synchronous execution thread. `None` restores `MySQL`'s default of 1024.
pub fn set_session_group_concat_max_len(limit: Option<usize>) {
    SESSION_GROUP_CONCAT_MAX_LEN.set(limit.unwrap_or(DEFAULT_GROUP_CONCAT_MAX_LEN).max(4));
    SESSION_GROUP_CONCAT_WARNINGS.set(0);
}

/// Takes the number of `GROUP_CONCAT` results truncated by the last statement.
#[must_use]
pub fn take_session_group_concat_warnings() -> u64 {
    SESSION_GROUP_CONCAT_WARNINGS.replace(0)
}

/// Installs the recursive-CTE iteration cap for queries on this thread.
/// `None` restores `MySQL`'s default of 1000 iterations.
pub fn set_session_cte_max_recursion_depth(limit: Option<u64>) {
    SESSION_CTE_MAX_RECURSION_DEPTH.set(limit.unwrap_or(DEFAULT_CTE_MAX_RECURSION_DEPTH));
}

/// Maximum estimated result rows accepted by the unqualified cross-join
/// operator.
pub const MAX_CROSS_JOIN_ROWS: u64 = 1_000_000;

/// Client-visible output field metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputField {
    /// Result-column name.
    pub name: String,
    /// Result scalar type, or `None` for an untyped `NULL`.
    pub data_type: Option<DataType>,
    /// Whether the result can be `NULL`.
    pub nullable: bool,
}

/// Physical pull operators selected for an executable query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhysicalPlan {
    /// Produces no rows.
    Empty,
    /// Produces one row with no input columns.
    OneRow,
    /// Reads a projected and pruned storage relation.
    Scan(Scan),
    /// Relabels a derived query's positional result columns.
    Derived {
        /// Complete inner query.
        input: Box<Self>,
        /// Synthetic layout visible to the containing query.
        columns: Vec<BoundColumn>,
    },
    /// Guarded Cartesian product.
    CrossJoin {
        /// Inputs in physical execution order.
        inputs: Vec<Self>,
        /// Catalog-derived result cardinality.
        estimated_rows: u64,
    },
    /// Streaming branch concatenation.
    UnionAll {
        /// Inputs in SQL source order.
        inputs: Vec<Self>,
    },
    /// Set operation: keep or drop left rows by membership in the
    /// materialized right input.
    SetOp {
        /// `INTERSECT` keeps matches, `EXCEPT` drops them.
        keep_matching: bool,
        /// `ALL` multiset semantics: each match consumes one right-side
        /// occurrence; the left input arrives without deduplication.
        all: bool,
        /// Left input (deduplicated upstream unless `all`).
        left: Box<Self>,
        /// Membership input.
        right: Box<Self>,
    },
    /// Recursive-CTE fixpoint materialized eagerly at operator build.
    Recursive {
        /// Synthetic working-table identity recursive scans reference.
        working: (DatabaseId, TableId),
        /// `UNION [DISTINCT]` recursion deduplicates accumulated rows.
        distinct: bool,
        /// Anchor plan; also fixes the output layout.
        anchor: Box<Self>,
        /// Recursive member template, rebuilt per iteration over the delta.
        member: Box<Self>,
    },
    /// Build-right equi hash join.
    HashJoin {
        /// Probe input.
        left: Box<Self>,
        /// Build input.
        right: Box<Self>,
        /// Join semantics.
        kind: BoundJoinKind,
        /// Probe-side key.
        left_key: BoundExpr,
        /// Additional equality key pairs beyond the primary; empty for the
        /// common single-key join.
        extra_keys: Vec<(BoundExpr, BoundExpr)>,
        /// Build-side key.
        right_key: BoundExpr,
    },
    /// Bounded nested-loop join for ON predicates containing a dependent
    /// subquery that cannot be represented as hash keys alone.
    NestedLoopJoin {
        /// Left input.
        left: Box<Self>,
        /// Right input.
        right: Box<Self>,
        /// Join semantics.
        kind: BoundJoinKind,
        /// Complete ON predicate.
        condition: BoundExpr,
    },
    /// Applies a row-selection mask.
    Filter {
        /// Input operator.
        input: Box<Self>,
        /// Bound row predicate.
        predicate: BoundExpr,
    },
    /// Memory-capped hash grouping and aggregate computation.
    HashAggregate {
        /// Input operator.
        input: Box<Self>,
        /// Ordered grouping expressions.
        group_by: Vec<BoundExpr>,
        /// Deduplicated aggregate computations.
        aggregates: Vec<BoundAggregate>,
    },
    /// Evaluates ordered result expressions.
    Project {
        /// Input operator.
        input: Box<Self>,
        /// Named result expressions.
        expressions: Vec<BoundProjection>,
    },
    /// Removes duplicate selected rows.
    Distinct {
        /// Input operator.
        input: Box<Self>,
    },
    /// Materialized full or top-K result sort.
    Sort {
        /// Projected input operator.
        input: Box<Self>,
        /// Result-layout ordering keys.
        keys: Vec<BoundOrderKey>,
        /// Maximum prefix retained before the downstream LIMIT.
        top_k: Option<usize>,
        /// Trailing hidden sort-only columns dropped after ordering.
        trim: usize,
    },
    /// Window computations appended as extra columns over sorted partitions.
    Window {
        /// Input operator.
        input: Box<Self>,
        /// Window computations in output order.
        windows: Vec<BoundWindow>,
        /// Synthetic output columns appended after the input's columns.
        outputs: Vec<BoundColumn>,
    },
    /// Skips and caps selected rows.
    Limit {
        /// Input operator.
        input: Box<Self>,
        /// Rows to skip.
        offset: u64,
        /// Rows to produce.
        count: u64,
    },
}

impl PhysicalPlan {
    /// Returns client-visible result fields.
    #[must_use]
    pub fn output_fields(&self) -> Vec<OutputField> {
        match self {
            Self::Project { expressions, .. } => expressions
                .iter()
                .map(|expression| OutputField {
                    name: expression.name.clone(),
                    data_type: expression.expr.data_type,
                    nullable: expression.expr.nullable,
                })
                .collect(),
            Self::SetOp { left: input, .. }
            | Self::Recursive { anchor: input, .. }
            | Self::Filter { input, .. }
            | Self::Distinct { input }
            | Self::Limit { input, .. } => input.output_fields(),
            Self::Sort { input, trim, .. } => {
                let mut fields = input.output_fields();
                fields.truncate(fields.len().saturating_sub(*trim));
                fields
            }
            Self::Window { input, outputs, .. } => {
                let mut fields = input.output_fields();
                fields.extend(outputs.iter().map(|column| OutputField {
                    name: column.name.clone(),
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                }));
                fields
            }
            Self::HashAggregate {
                group_by,
                aggregates,
                ..
            } => group_by
                .iter()
                .map(|expression| OutputField {
                    name: String::new(),
                    data_type: expression.data_type,
                    nullable: expression.nullable,
                })
                .chain(aggregates.iter().map(|aggregate| OutputField {
                    name: String::new(),
                    data_type: aggregate.data_type,
                    nullable: aggregate.nullable,
                }))
                .collect(),
            Self::CrossJoin { inputs, .. } => inputs.iter().flat_map(Self::output_fields).collect(),
            Self::UnionAll { inputs } => inputs
                .first()
                .map_or_else(Vec::new, PhysicalPlan::output_fields),
            Self::HashJoin {
                left, right, kind, ..
            }
            | Self::NestedLoopJoin {
                left, right, kind, ..
            } => {
                let mut fields = left.output_fields();
                if !matches!(kind, BoundJoinKind::Semi | BoundJoinKind::Anti) {
                    let mut right_fields = right.output_fields();
                    if matches!(kind, BoundJoinKind::Left | BoundJoinKind::Scalar) {
                        for field in &mut right_fields {
                            field.nullable = true;
                        }
                    }
                    fields.extend(right_fields);
                }
                fields
            }
            Self::Scan(scan) => scan
                .projected_column_ids
                .iter()
                .filter_map(|id| {
                    scan.table
                        .columns
                        .iter()
                        .find(|column| column.column_id == *id)
                })
                .map(|column| OutputField {
                    name: column.name.clone(),
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                })
                .collect(),
            Self::Derived { columns, .. } => columns
                .iter()
                .map(|column| OutputField {
                    name: column.name.clone(),
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                })
                .collect(),
            Self::Empty | Self::OneRow => Vec::new(),
        }
    }
}

/// Selects physical implementations for supported logical operators.
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysicalPlanner;

impl PhysicalPlanner {
    /// Lowers an optimized logical plan.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnsupportedOperator`] for logical operators whose
    /// physical implementation is not available yet.
    #[allow(clippy::too_many_lines)]
    pub fn plan(logical: LogicalPlan) -> Result<PhysicalPlan, ExecError> {
        match logical {
            LogicalPlan::Empty => Ok(PhysicalPlan::Empty),
            LogicalPlan::OneRow => Ok(PhysicalPlan::OneRow),
            LogicalPlan::Scan(scan) => Ok(PhysicalPlan::Scan(scan)),
            LogicalPlan::Derived { input, columns } => Ok(PhysicalPlan::Derived {
                input: Box::new(Self::plan(*input)?),
                columns,
            }),
            LogicalPlan::Filter { input, predicate } => Ok(PhysicalPlan::Filter {
                input: Box::new(Self::plan(*input)?),
                predicate,
            }),
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => Ok(PhysicalPlan::HashAggregate {
                input: Box::new(Self::plan(*input)?),
                group_by,
                aggregates,
            }),
            LogicalPlan::Project { input, expressions } => Ok(PhysicalPlan::Project {
                input: Box::new(Self::plan(*input)?),
                expressions,
            }),
            LogicalPlan::Limit { input, limit } => plan_limit(*input, limit.offset, limit.count),
            LogicalPlan::CrossJoin { inputs } => {
                let estimated_rows = inputs
                    .iter()
                    .try_fold(1_u64, |rows, input| {
                        rows.checked_mul(input.estimated_rows()?)
                    })
                    .ok_or(ExecError::CrossJoinCardinalityUnknown)?;
                if estimated_rows > MAX_CROSS_JOIN_ROWS {
                    return Err(ExecError::CrossJoinGuardExceeded {
                        estimated_rows,
                        limit: MAX_CROSS_JOIN_ROWS,
                    });
                }
                Ok(PhysicalPlan::CrossJoin {
                    inputs: inputs
                        .into_iter()
                        .map(Self::plan)
                        .collect::<Result<Vec<_>, _>>()?,
                    estimated_rows,
                })
            }
            LogicalPlan::SetOp {
                keep_matching,
                all,
                left,
                right,
            } => Ok(PhysicalPlan::SetOp {
                keep_matching,
                all,
                left: Box::new(Self::plan(*left)?),
                right: Box::new(Self::plan(*right)?),
            }),
            LogicalPlan::Recursive {
                working_database,
                working_table,
                distinct,
                anchor,
                member,
            } => Ok(PhysicalPlan::Recursive {
                working: (working_database, working_table),
                distinct,
                anchor: Box::new(Self::plan(*anchor)?),
                member: Box::new(Self::plan(*member)?),
            }),
            LogicalPlan::UnionAll { inputs } => Ok(PhysicalPlan::UnionAll {
                inputs: inputs
                    .into_iter()
                    .map(Self::plan)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            LogicalPlan::Distinct { input } => Ok(PhysicalPlan::Distinct {
                input: Box::new(Self::plan(*input)?),
            }),
            LogicalPlan::Window {
                input,
                windows,
                outputs,
            } => Ok(PhysicalPlan::Window {
                input: Box::new(Self::plan(*input)?),
                windows,
                outputs,
            }),
            LogicalPlan::Sort { input, keys, trim } => Ok(PhysicalPlan::Sort {
                input: Box::new(Self::plan(*input)?),
                keys,
                top_k: None,
                trim,
            }),
            LogicalPlan::Join {
                left,
                right,
                kind,
                condition,
            } => {
                if kind == BoundJoinKind::Cross
                    || kind == BoundJoinKind::Inner && condition.is_none()
                {
                    let estimated_rows = left
                        .estimated_rows()
                        .and_then(|rows| rows.checked_mul(right.estimated_rows()?))
                        .ok_or(ExecError::CrossJoinCardinalityUnknown)?;
                    if estimated_rows > MAX_CROSS_JOIN_ROWS {
                        return Err(ExecError::CrossJoinGuardExceeded {
                            estimated_rows,
                            limit: MAX_CROSS_JOIN_ROWS,
                        });
                    }
                    return Ok(PhysicalPlan::CrossJoin {
                        inputs: vec![Self::plan(*left)?, Self::plan(*right)?],
                        estimated_rows,
                    });
                }
                let condition = condition.ok_or(ExecError::UnsupportedJoinCondition)?;
                if expression_has_subquery(&condition) {
                    return Ok(PhysicalPlan::NestedLoopJoin {
                        left: Box::new(Self::plan(*left)?),
                        right: Box::new(Self::plan(*right)?),
                        kind,
                        condition,
                    });
                }
                let mut pairs = equi_join_key_pairs(&condition, &left, &right)
                    .ok_or(ExecError::UnsupportedJoinCondition)?;
                let (left_key, right_key) = pairs.remove(0);
                Ok(PhysicalPlan::HashJoin {
                    left: Box::new(Self::plan(*left)?),
                    right: Box::new(Self::plan(*right)?),
                    kind,
                    left_key,
                    extra_keys: pairs,
                    right_key,
                })
            }
        }
    }
}

fn plan_limit(input: LogicalPlan, offset: u64, count: u64) -> Result<PhysicalPlan, ExecError> {
    let input = match input {
        LogicalPlan::Sort { input, keys, trim } => PhysicalPlan::Sort {
            input: Box::new(PhysicalPlanner::plan(*input)?),
            keys,
            top_k: usize::try_from(offset.saturating_add(count)).ok(),
            trim,
        },
        input => PhysicalPlanner::plan(input)?,
    };
    Ok(PhysicalPlan::Limit {
        input: Box::new(input),
        offset,
        count,
    })
}

/// Splits a join condition into oriented equality key pairs. Every AND-ed
/// conjunct must be a hashable equality spanning the two sides; anything
/// else rejects the whole condition.
fn equi_join_key_pairs(
    condition: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
) -> Option<Vec<(BoundExpr, BoundExpr)>> {
    fn conjuncts_of(expr: &BoundExpr, out: &mut Vec<BoundExpr>) {
        if let BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } = &expr.kind
        {
            conjuncts_of(left, out);
            conjuncts_of(right, out);
        } else {
            out.push(expr.clone());
        }
    }
    let left_tables = logical_tables(left);
    let right_tables = logical_tables(right);
    let mut conjuncts = Vec::new();
    conjuncts_of(condition, &mut conjuncts);
    let mut pairs = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: first,
            right: second,
        } = &conjunct.kind
        else {
            return None;
        };
        hash_join_key_mode(first.data_type, second.data_type)?;
        if expression_belongs_to(first, &left_tables)
            && expression_belongs_to(second, &right_tables)
        {
            pairs.push(((**first).clone(), (**second).clone()));
        } else if expression_belongs_to(first, &right_tables)
            && expression_belongs_to(second, &left_tables)
        {
            pairs.push(((**second).clone(), (**first).clone()));
        } else {
            return None;
        }
    }
    if pairs.is_empty() { None } else { Some(pairs) }
}

#[derive(Clone, Copy, Debug)]
enum JoinKeyMode {
    CollatedText,
    Binary,
    Boolean,
    Integer,
    MysqlNumber,
}

fn hash_join_key_mode(left: Option<DataType>, right: Option<DataType>) -> Option<JoinKeyMode> {
    match (left?.storage_type(), right?.storage_type()) {
        (DataType::Utf8, DataType::Utf8) => Some(JoinKeyMode::CollatedText),
        (DataType::Binary, DataType::Binary) => Some(JoinKeyMode::Binary),
        (DataType::Boolean, DataType::Boolean) => Some(JoinKeyMode::Boolean),
        (DataType::Int64 | DataType::UInt64, DataType::Int64 | DataType::UInt64) => {
            Some(JoinKeyMode::Integer)
        }
        (
            DataType::Boolean
            | DataType::Int64
            | DataType::UInt64
            | DataType::Float64
            | DataType::Utf8
            | DataType::Binary,
            DataType::Boolean
            | DataType::Int64
            | DataType::UInt64
            | DataType::Float64
            | DataType::Utf8
            | DataType::Binary,
        ) => Some(JoinKeyMode::MysqlNumber),
        _ => None,
    }
}

fn logical_tables(plan: &LogicalPlan) -> BTreeSet<(DatabaseId, TableId)> {
    let mut tables = BTreeSet::new();
    collect_logical_tables(plan, &mut tables);
    tables
}

fn collect_logical_tables(plan: &LogicalPlan, tables: &mut BTreeSet<(DatabaseId, TableId)>) {
    match plan {
        LogicalPlan::Recursive { anchor, member, .. } => {
            collect_logical_tables(anchor, tables);
            collect_logical_tables(member, tables);
        }
        LogicalPlan::Scan(scan) => {
            tables.insert((scan.table.database_id, scan.table.table_id));
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                collect_logical_tables(input, tables);
            }
        }
        LogicalPlan::SetOp { left, right, .. } | LogicalPlan::Join { left, right, .. } => {
            collect_logical_tables(left, tables);
            collect_logical_tables(right, tables);
        }
        LogicalPlan::Derived { columns, .. } => {
            for column in columns {
                tables.insert((column.database_id, column.table_id));
            }
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => collect_logical_tables(input, tables),
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn expression_belongs_to(expression: &BoundExpr, tables: &BTreeSet<(DatabaseId, TableId)>) -> bool {
    let mut references = BTreeSet::new();
    collect_expression_tables(expression, &mut references);
    !references.is_empty() && references.is_subset(tables)
}

fn collect_expression_tables(expression: &BoundExpr, tables: &mut BTreeSet<(DatabaseId, TableId)>) {
    match &expression.kind {
        BoundExprKind::Column(column) => {
            tables.insert((column.database_id, column.table_id));
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            collect_expression_tables(expr, tables);
        }
        BoundExprKind::Binary { left, right, .. } => {
            collect_expression_tables(left, tables);
            collect_expression_tables(right, tables);
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                collect_expression_tables(argument, tables);
            }
        }
        BoundExprKind::InSubquery { expr, .. } => collect_expression_tables(expr, tables),
        BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => {}
    }
}

/// Pull-based batch source opened for one physical scan.
pub trait BatchStream: Send {
    /// Produces the next batch, or `None` at end of stream.
    ///
    /// # Errors
    ///
    /// Returns a source-specific execution error.
    fn next_batch(&mut self, available_memory: usize) -> Result<Option<RecordBatch>, ExecError>;

    /// Returns bytes retained between pulls by this stream.
    #[must_use]
    fn retained_bytes(&self) -> usize;

    /// Returns an upper bound for additional memory allocated by the next
    /// pull while the stream's currently retained bytes are still live.
    ///
    /// `budget` is what the query has left. A stream that sizes its batches
    /// should plan one that fits rather than quoting a fixed maximum: a small
    /// ceiling is a reason to produce smaller batches, not to fail.
    #[must_use]
    fn next_batch_memory_upper_bound(&self, budget: usize) -> usize;

    /// Narrows the stream to rows whose value in the projected column at
    /// `position` lies within `[min, max]`. Best-effort: streams that cannot
    /// prune (already started, non-key column, type mismatch) ignore it.
    fn restrict_key_position_range(&mut self, _position: usize, _min: &Value, _max: &Value) {}

    /// `(table directory, manifest generation, scan signature)` over a
    /// settled snapshot — the exactness-preserving identity for the settled
    /// aggregate memo. Default: not settled.
    #[must_use]
    fn settled_identity(&self) -> Option<(std::path::PathBuf, u64, String)> {
        None
    }

    /// The insert-only memtable delta over the segment-resident identity,
    /// when the memo can merge it (bare full-table scan, pure inserts
    /// above the segment key space, bounded row count). Default: none.
    #[must_use]
    fn insert_only_delta(&self) -> Option<InsertOnlyDelta> {
        None
    }

    /// Per-segment SMAs plus residual memtable rows for a bare full-table
    /// scan whose fold is provably exact under merge-on-read (WS3-B).
    /// Default: none.
    #[must_use]
    fn sma_fold_input(&self) -> Option<SmaFoldInput> {
        None
    }
}

/// Persistent per-segment SMAs (manifest v2) plus the projected residual
/// memtable rows: bare COUNT/SUM/AVG/MIN/MAX fold the segment statistics
/// and aggregate only the residual, so answers stay fast DURING ingest and
/// across flushes — the case the settled memo cannot serve.
#[derive(Clone)]
pub struct SmaFoldInput {
    /// Projected column ids, index-aligned with aggregate column indexes.
    pub(crate) column_ids: Vec<u32>,
    pub(crate) segments: Vec<pintail_store::SegmentSmas>,
    /// Projected memtable rows above the whole segment key space.
    pub(crate) rows: Vec<Vec<Value>>,
}

/// Projected memtable rows riding above a generation-keyed aggregate memo
/// entry: aggregating these few rows and merging finished values onto the
/// memoized result keeps answers exact DURING ingest (issue #6 WS3).
#[derive(Clone)]
pub struct InsertOnlyDelta {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) generation: u64,
    /// Same scan signature the settled memo keys on (projection,
    /// predicates, limit), so the delta finds its base entry.
    pub(crate) scan: String,
    pub(crate) types: Vec<DataType>,
    pub(crate) rows: Vec<Vec<Value>>,
}

/// One-batch stream feeding the delta rows through the normal aggregate
/// machinery.
struct OneShotStream {
    batch: Option<RecordBatch>,
}

impl BatchStream for OneShotStream {
    fn next_batch(&mut self, _available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        Ok(self.batch.take())
    }

    fn retained_bytes(&self) -> usize {
        0
    }

    fn next_batch_memory_upper_bound(&self, _budget: usize) -> usize {
        0
    }
}

/// Opens storage scans for physical execution.
pub trait ScanProvider {
    /// Opens one scan whose batches contain exactly the scan's projected
    /// columns in the requested order.
    ///
    /// # Errors
    ///
    /// Returns a source-specific execution error.
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError>;
}

/// Hard per-query memory accounting.
#[derive(Debug)]
pub struct MemoryTracker {
    limit: usize,
    deadline: Option<Instant>,
    /// Atomic so parallel operators can reserve from worker threads through
    /// a shared `&MemoryTracker` (experiments/RESULTS.md e02: thread-local
    /// partial state + merge is the adopted parallel-aggregation shape).
    used: std::sync::atomic::AtomicUsize,
    spill: spill::QuerySpill,
}

impl Clone for MemoryTracker {
    fn clone(&self) -> Self {
        Self {
            limit: self.limit,
            deadline: self.deadline,
            used: std::sync::atomic::AtomicUsize::new(self.used()),
            spill: self.spill.clone(),
        }
    }
}

impl PartialEq for MemoryTracker {
    fn eq(&self, other: &Self) -> bool {
        self.limit == other.limit && self.used() == other.used()
    }
}

impl Eq for MemoryTracker {}

impl MemoryTracker {
    /// Constructs a tracker with a hard byte limit.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self::with_deadline(limit, None)
    }

    /// Constructs a tracker with a hard byte limit and optional monotonic
    /// execution deadline.
    #[must_use]
    pub fn with_deadline(limit: usize, deadline: Option<Instant>) -> Self {
        Self {
            limit,
            deadline,
            used: std::sync::atomic::AtomicUsize::new(0),
            spill: spill::QuerySpill::new(),
        }
    }

    /// Returns the hard byte limit.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns bytes currently reserved by stateful operators.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns bytes still available to persistent query state.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used())
    }

    fn spill(&self) -> &spill::QuerySpill {
        &self.spill
    }

    /// Returns query-local spill counters.
    #[must_use]
    pub fn spill_metrics(&self) -> spill::QuerySpillMetrics {
        self.spill.metrics()
    }

    /// Reserves persistent operator memory.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryLimitExceeded`] before exceeding the query
    /// limit.
    pub fn reserve(&self, bytes: usize) -> Result<(), ExecError> {
        self.check_deadline()?;
        let outcome = self.used.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |used| {
                let requested = used.saturating_add(bytes);
                (requested <= self.limit).then_some(requested)
            },
        );
        match outcome {
            Ok(_) => Ok(()),
            Err(used) => Err(ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit,
            }),
        }
    }

    /// Releases persistent operator memory.
    pub fn release(&self, bytes: usize) {
        let _ = self.used.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |used| Some(used.saturating_sub(bytes)),
        );
    }

    fn ensure_transient(&self, bytes: usize) -> Result<(), ExecError> {
        self.check_deadline()?;
        let used = self.used();
        if used.saturating_add(bytes) > self.limit {
            return Err(ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit,
            });
        }
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), ExecError> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Err(ExecError::QueryTimedOut)
        } else {
            Ok(())
        }
    }
}

/// Running pull-based query execution.
pub struct Execution {
    root: PullOperator,
    memory: MemoryTracker,
    output_fields: Vec<OutputField>,
}

fn aggregate_regex_memory_upper_bound(aggregate: &BoundAggregate) -> usize {
    aggregate
        .expr
        .as_ref()
        .map_or(0, bound_regex_memory_upper_bound)
        .saturating_add(
            aggregate
                .order_within
                .iter()
                .fold(0_usize, |bytes, (expression, _)| {
                    bytes.saturating_add(bound_regex_memory_upper_bound(expression))
                }),
        )
}

fn window_regex_memory_upper_bound(window: &BoundWindow) -> usize {
    let function = match &window.function {
        WindowFunction::Offset { expr, default, .. } => bound_regex_memory_upper_bound(expr)
            .saturating_add(default.as_deref().map_or(0, bound_regex_memory_upper_bound)),
        WindowFunction::Extreme { expr, .. } => bound_regex_memory_upper_bound(expr),
        WindowFunction::Aggregate(aggregate) => aggregate_regex_memory_upper_bound(aggregate)
            .saturating_add(
                aggregate
                    .expr
                    .as_ref()
                    .map_or(0, bound_regex_memory_upper_bound),
            ),
        WindowFunction::RowNumber
        | WindowFunction::Rank
        | WindowFunction::DenseRank
        | WindowFunction::NTile(_) => 0,
    };
    function
        .saturating_add(
            window
                .partition_by
                .iter()
                .fold(0_usize, |bytes, expression| {
                    bytes.saturating_add(bound_regex_memory_upper_bound(expression))
                }),
        )
        .saturating_add(window.order_by.iter().fold(0_usize, |bytes, key| {
            bytes.saturating_add(bound_regex_memory_upper_bound(&key.expr))
        }))
}

fn plan_regex_memory_upper_bound(plan: &PhysicalPlan) -> usize {
    let nested = |input: &PhysicalPlan| plan_regex_memory_upper_bound(input);
    match plan {
        PhysicalPlan::Empty | PhysicalPlan::OneRow => 0,
        PhysicalPlan::Scan(scan) => scan.predicates.iter().fold(0_usize, |bytes, expression| {
            bytes.saturating_add(bound_regex_memory_upper_bound(expression))
        }),
        PhysicalPlan::Derived { input, .. }
        | PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => nested(input),
        PhysicalPlan::CrossJoin { inputs, .. } | PhysicalPlan::UnionAll { inputs } => inputs
            .iter()
            .fold(0_usize, |bytes, input| bytes.saturating_add(nested(input))),
        PhysicalPlan::SetOp { left, right, .. } => nested(left).saturating_add(nested(right)),
        PhysicalPlan::Recursive { anchor, member, .. } => {
            nested(anchor).saturating_add(nested(member))
        }
        PhysicalPlan::HashJoin {
            left,
            right,
            left_key,
            right_key,
            extra_keys,
            ..
        } => nested(left)
            .saturating_add(nested(right))
            .saturating_add(bound_regex_memory_upper_bound(left_key))
            .saturating_add(bound_regex_memory_upper_bound(right_key))
            .saturating_add(extra_keys.iter().fold(0_usize, |bytes, (left, right)| {
                bytes
                    .saturating_add(bound_regex_memory_upper_bound(left))
                    .saturating_add(bound_regex_memory_upper_bound(right))
            })),
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            condition,
            ..
        } => nested(left)
            .saturating_add(nested(right))
            .saturating_add(bound_regex_memory_upper_bound(condition)),
        PhysicalPlan::Filter { input, predicate } => {
            nested(input).saturating_add(bound_regex_memory_upper_bound(predicate))
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => nested(input)
            .saturating_add(group_by.iter().fold(0_usize, |bytes, expression| {
                bytes.saturating_add(bound_regex_memory_upper_bound(expression))
            }))
            .saturating_add(aggregates.iter().fold(0_usize, |bytes, aggregate| {
                bytes.saturating_add(aggregate_regex_memory_upper_bound(aggregate))
            })),
        PhysicalPlan::Project { input, expressions } => {
            nested(input).saturating_add(expressions.iter().fold(0_usize, |bytes, projection| {
                bytes.saturating_add(bound_regex_memory_upper_bound(&projection.expr))
            }))
        }
        PhysicalPlan::Window { input, windows, .. } => {
            nested(input).saturating_add(windows.iter().fold(0_usize, |bytes, window| {
                bytes.saturating_add(window_regex_memory_upper_bound(window))
            }))
        }
    }
}

impl Execution {
    /// Opens every scan and prepares a physical plan for pulling.
    ///
    /// # Errors
    ///
    /// Returns an error when a scan cannot open or an expression references a
    /// column absent from its physical input.
    pub fn start(
        plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
    ) -> Result<Self, ExecError> {
        Self::start_with_deadline(plan, provider, memory_limit, None)
    }

    /// Opens a physical plan with an optional monotonic execution deadline.
    /// Stateful operators check it at every pull and memory reservation, so
    /// timeout enforcement remains cooperative without a timer thread.
    ///
    /// # Errors
    ///
    /// Returns an execution error when setup fails or the deadline has
    /// already elapsed.
    pub fn start_with_deadline(
        mut plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
        deadline: Option<Instant>,
    ) -> Result<Self, ExecError> {
        let mut subquery_bytes = 0;
        resolve_plan_subqueries(
            &mut plan,
            provider,
            memory_limit,
            deadline,
            &mut subquery_bytes,
        )?;
        let output_fields = plan.output_fields();
        let memory = MemoryTracker::with_deadline(memory_limit, deadline);
        memory.check_deadline()?;
        memory.reserve(subquery_bytes.saturating_add(plan_regex_memory_upper_bound(&plan)))?;
        let (root, _) = build_operator(plan, provider, &memory)?;
        Ok(Self {
            root,
            memory,
            output_fields,
        })
    }

    /// Returns client-visible result fields.
    #[must_use]
    pub fn output_fields(&self) -> &[OutputField] {
        &self.output_fields
    }

    /// Pulls the next non-empty selected batch.
    ///
    /// # Errors
    ///
    /// Returns a source, expression, batch-invariant, or memory-limit error.
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        self.root.next_batch(&self.memory)
    }

    /// Returns current hard-cap accounting.
    #[must_use]
    pub const fn memory(&self) -> &MemoryTracker {
        &self.memory
    }

    /// Returns disk-spill counters accumulated by this execution.
    #[must_use]
    pub fn spill_metrics(&self) -> spill::QuerySpillMetrics {
        self.memory.spill_metrics()
    }
}

#[allow(clippy::too_many_lines)]
fn resolve_plan_subqueries(
    plan: &mut PhysicalPlan,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<Instant>,
    retained_bytes: &mut usize,
) -> Result<(), ExecError> {
    match plan {
        PhysicalPlan::Recursive { anchor, member, .. } => {
            resolve_plan_subqueries(anchor, provider, memory_limit, deadline, retained_bytes)?;
            resolve_plan_subqueries(member, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::Scan(scan) => {
            for predicate in &mut scan.predicates {
                resolve_expr_subqueries(
                    predicate,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
            }
        }
        PhysicalPlan::Derived { input, .. }
        | PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => {
            resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::SetOp { left, right, .. } => {
            resolve_plan_subqueries(left, provider, memory_limit, deadline, retained_bytes)?;
            resolve_plan_subqueries(right, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::Window { input, windows, .. } => {
            for window in windows {
                if let WindowFunction::Aggregate(aggregate) = &mut window.function
                    && let Some(expr) = &mut aggregate.expr
                {
                    resolve_expr_subqueries(
                        expr,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                    )?;
                }
                for expr in &mut window.partition_by {
                    resolve_expr_subqueries(
                        expr,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                    )?;
                }
                for key in &mut window.order_by {
                    resolve_expr_subqueries(
                        &mut key.expr,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                    )?;
                }
            }
            resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::CrossJoin { inputs, .. } | PhysicalPlan::UnionAll { inputs } => {
            for input in inputs {
                resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
            }
        }
        PhysicalPlan::HashJoin {
            left,
            right,
            left_key,
            right_key,
            extra_keys,
            ..
        } => {
            resolve_plan_subqueries(left, provider, memory_limit, deadline, retained_bytes)?;
            resolve_plan_subqueries(right, provider, memory_limit, deadline, retained_bytes)?;
            resolve_expr_subqueries(left_key, provider, memory_limit, deadline, retained_bytes)?;
            resolve_expr_subqueries(right_key, provider, memory_limit, deadline, retained_bytes)?;
            for (extra_left, extra_right) in extra_keys {
                resolve_expr_subqueries(
                    extra_left,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
                resolve_expr_subqueries(
                    extra_right,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
            }
        }
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            condition,
            ..
        } => {
            resolve_plan_subqueries(left, provider, memory_limit, deadline, retained_bytes)?;
            resolve_plan_subqueries(right, provider, memory_limit, deadline, retained_bytes)?;
            resolve_expr_subqueries(condition, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::Filter { input, predicate } => {
            resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
            resolve_expr_subqueries(predicate, provider, memory_limit, deadline, retained_bytes)?;
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => {
            resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
            for expression in group_by {
                resolve_expr_subqueries(
                    expression,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
            }
            for aggregate in aggregates {
                if let Some(expression) = &mut aggregate.expr {
                    resolve_expr_subqueries(
                        expression,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                    )?;
                }
                for (key, _) in &mut aggregate.order_within {
                    resolve_expr_subqueries(key, provider, memory_limit, deadline, retained_bytes)?;
                }
            }
        }
        PhysicalPlan::Project { input, expressions } => {
            resolve_plan_subqueries(input, provider, memory_limit, deadline, retained_bytes)?;
            for projection in expressions {
                resolve_expr_subqueries(
                    &mut projection.expr,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
            }
        }
        PhysicalPlan::Empty | PhysicalPlan::OneRow => {}
    }
    Ok(())
}

fn resolve_expr_subqueries(
    expression: &mut BoundExpr,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<Instant>,
    retained_bytes: &mut usize,
) -> Result<(), ExecError> {
    match &mut expression.kind {
        BoundExprKind::ScalarSubquery(query) if bound_query_has_outer_refs(query) => {}
        BoundExprKind::ScalarSubquery(query) => {
            let values = materialize_subquery(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
                deadline,
                Some(2),
            )?;
            let value = match values.as_slice() {
                [] => Value::Null,
                [value] => value.clone(),
                _ => {
                    return Err(ExecError::ScalarSubqueryRows { rows: values.len() });
                }
            };
            reserve_subquery_values(std::slice::from_ref(&value), memory_limit, retained_bytes)?;
            expression.kind = BoundExprKind::Literal(value);
        }
        BoundExprKind::ExistsSubquery { query, .. } if bound_query_has_outer_refs(query) => {}
        BoundExprKind::ExistsSubquery { query, negated } => {
            let values = materialize_subquery(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
                deadline,
                Some(1),
            )?;
            let exists = !values.is_empty();
            expression.kind = BoundExprKind::Literal(Value::Boolean(exists != *negated));
        }
        BoundExprKind::InSubquery {
            expr,
            query,
            negated,
        } => {
            resolve_expr_subqueries(expr, provider, memory_limit, deadline, retained_bytes)?;
            if bound_query_has_outer_refs(query) {
                return Ok(());
            }
            let projection_type = query
                .projection
                .first()
                .and_then(|projection| projection.expr.data_type);
            let values = materialize_subquery(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
                deadline,
                None,
            )?;
            reserve_subquery_values(&values, memory_limit, retained_bytes)?;
            let mut args = Vec::with_capacity(values.len() + 1);
            args.push((**expr).clone());
            args.extend(values.into_iter().map(|value| BoundExpr {
                data_type: projection_type.or_else(|| value.data_type()),
                nullable: matches!(value, Value::Null),
                kind: BoundExprKind::Literal(value),
            }));
            expression.kind = BoundExprKind::Scalar {
                function: pintail_sql::ScalarFunction::InList { negated: *negated },
                args,
            };
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            resolve_expr_subqueries(expr, provider, memory_limit, deadline, retained_bytes)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            resolve_expr_subqueries(left, provider, memory_limit, deadline, retained_bytes)?;
            resolve_expr_subqueries(right, provider, memory_limit, deadline, retained_bytes)?;
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                resolve_expr_subqueries(
                    argument,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                )?;
            }
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
    Ok(())
}

fn bound_query_has_outer_refs(query: &BoundQuery) -> bool {
    query
        .projection
        .iter()
        .any(|projection| bound_expr_has_outer_refs(&projection.expr))
        || query.filter.as_ref().is_some_and(bound_expr_has_outer_refs)
        || query.group_by.iter().any(bound_expr_has_outer_refs)
        || query.aggregates.iter().any(|aggregate| {
            aggregate
                .expr
                .as_ref()
                .is_some_and(bound_expr_has_outer_refs)
                || aggregate
                    .order_within
                    .iter()
                    .any(|(expression, _)| bound_expr_has_outer_refs(expression))
        })
        || query.windows.iter().any(bound_window_has_outer_refs)
        || query.having.as_ref().is_some_and(bound_expr_has_outer_refs)
        || query.from.iter().any(|source| {
            source
                .base
                .input
                .as_deref()
                .is_some_and(bound_query_has_outer_refs)
                || source.joins.iter().any(|join| {
                    join.table
                        .input
                        .as_deref()
                        .is_some_and(bound_query_has_outer_refs)
                        || join
                            .condition
                            .as_ref()
                            .is_some_and(bound_expr_has_outer_refs)
                })
        })
        || query.union_all.iter().any(bound_query_has_outer_refs)
        || query
            .set_ops
            .iter()
            .any(|(_, right)| bound_query_has_outer_refs(right))
        || query
            .recursive
            .as_deref()
            .is_some_and(|recursive| bound_query_has_outer_refs(&recursive.member))
}

fn bound_window_has_outer_refs(window: &BoundWindow) -> bool {
    let function = match &window.function {
        WindowFunction::Aggregate(aggregate) => aggregate
            .expr
            .as_ref()
            .is_some_and(bound_expr_has_outer_refs),
        WindowFunction::Offset { expr, default, .. } => {
            bound_expr_has_outer_refs(expr)
                || default.as_deref().is_some_and(bound_expr_has_outer_refs)
        }
        WindowFunction::Extreme { expr, .. } => bound_expr_has_outer_refs(expr),
        WindowFunction::RowNumber
        | WindowFunction::Rank
        | WindowFunction::DenseRank
        | WindowFunction::NTile(_) => false,
    };
    function
        || window.partition_by.iter().any(bound_expr_has_outer_refs)
        || window
            .order_by
            .iter()
            .any(|key| bound_expr_has_outer_refs(&key.expr))
}

fn bound_expr_has_outer_refs(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Column(column) => column.outer,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            bound_expr_has_outer_refs(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            bound_expr_has_outer_refs(left) || bound_expr_has_outer_refs(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(bound_expr_has_outer_refs),
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            bound_query_has_outer_refs(query)
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            bound_expr_has_outer_refs(expr) || bound_query_has_outer_refs(query)
        }
        BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

fn expression_has_dependent_subquery(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            bound_query_has_outer_refs(query)
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            bound_query_has_outer_refs(query) || expression_has_dependent_subquery(expr)
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            expression_has_dependent_subquery(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            expression_has_dependent_subquery(left) || expression_has_dependent_subquery(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(expression_has_dependent_subquery),
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

fn expression_has_subquery(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::InSubquery { .. } => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            expression_has_subquery(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            expression_has_subquery(left) || expression_has_subquery(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(expression_has_subquery),
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

fn substitute_outer_query(
    query: &mut BoundQuery,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
) -> Result<(), ExecError> {
    for projection in &mut query.projection {
        substitute_outer_expr(&mut projection.expr, batch, row, columns)?;
    }
    if let Some(filter) = &mut query.filter {
        substitute_outer_expr(filter, batch, row, columns)?;
    }
    for expression in &mut query.group_by {
        substitute_outer_expr(expression, batch, row, columns)?;
    }
    for aggregate in &mut query.aggregates {
        if let Some(expression) = &mut aggregate.expr {
            substitute_outer_expr(expression, batch, row, columns)?;
        }
        for (expression, _) in &mut aggregate.order_within {
            substitute_outer_expr(expression, batch, row, columns)?;
        }
    }
    for window in &mut query.windows {
        match &mut window.function {
            WindowFunction::Aggregate(aggregate) => {
                if let Some(expression) = &mut aggregate.expr {
                    substitute_outer_expr(expression, batch, row, columns)?;
                }
            }
            WindowFunction::Offset { expr, default, .. } => {
                substitute_outer_expr(expr, batch, row, columns)?;
                if let Some(default) = default {
                    substitute_outer_expr(default, batch, row, columns)?;
                }
            }
            WindowFunction::Extreme { expr, .. } => {
                substitute_outer_expr(expr, batch, row, columns)?;
            }
            WindowFunction::RowNumber
            | WindowFunction::Rank
            | WindowFunction::DenseRank
            | WindowFunction::NTile(_) => {}
        }
        for expression in &mut window.partition_by {
            substitute_outer_expr(expression, batch, row, columns)?;
        }
        for key in &mut window.order_by {
            substitute_outer_expr(&mut key.expr, batch, row, columns)?;
        }
    }
    if let Some(having) = &mut query.having {
        substitute_outer_expr(having, batch, row, columns)?;
    }
    for source in &mut query.from {
        if let Some(input) = &mut source.base.input {
            substitute_outer_query(input, batch, row, columns)?;
        }
        for join in &mut source.joins {
            if let Some(input) = &mut join.table.input {
                substitute_outer_query(input, batch, row, columns)?;
            }
            if let Some(condition) = &mut join.condition {
                substitute_outer_expr(condition, batch, row, columns)?;
            }
        }
    }
    for branch in &mut query.union_all {
        substitute_outer_query(branch, batch, row, columns)?;
    }
    for (_, right) in &mut query.set_ops {
        substitute_outer_query(right, batch, row, columns)?;
    }
    if let Some(recursive) = &mut query.recursive {
        substitute_outer_query(&mut recursive.member, batch, row, columns)?;
    }
    Ok(())
}

fn substitute_outer_expr(
    expression: &mut BoundExpr,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
) -> Result<(), ExecError> {
    match &mut expression.kind {
        BoundExprKind::Column(column) if column.outer => {
            let position = columns.iter().position(|candidate| {
                candidate.database_id == column.database_id
                    && candidate.table_id == column.table_id
                    && candidate.column_id == column.column_id
                    && candidate
                        .relation_name
                        .eq_ignore_ascii_case(&column.relation_name)
            });
            if let Some(position) = position {
                let value = batch
                    .column(position)
                    .and_then(|values| values.value(row))
                    .cloned()
                    .ok_or(ExecError::InvalidBatch(
                        "dependent subquery outer row is outside its input",
                    ))?;
                expression.nullable = matches!(value, Value::Null);
                expression.kind = BoundExprKind::Literal(value);
            }
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            substitute_outer_expr(expr, batch, row, columns)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            substitute_outer_expr(left, batch, row, columns)?;
            substitute_outer_expr(right, batch, row, columns)?;
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                substitute_outer_expr(argument, batch, row, columns)?;
            }
        }
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            substitute_outer_query(query, batch, row, columns)?;
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            substitute_outer_expr(expr, batch, row, columns)?;
            substitute_outer_query(query, batch, row, columns)?;
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
    Ok(())
}

fn resolve_dependent_expr_subqueries(
    expression: &mut BoundExpr,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    memory.check_deadline()?;
    match &mut expression.kind {
        BoundExprKind::ScalarSubquery(query) => {
            let mut query = (**query).clone();
            substitute_outer_query(&mut query, batch, row, columns)?;
            let values = materialize_subquery(
                query,
                provider,
                dependent_subquery_memory_limit(memory, batch)?,
                memory.deadline,
                Some(2),
            )?;
            let value = match values.as_slice() {
                [] => Value::Null,
                [value] => value.clone(),
                _ => return Err(ExecError::ScalarSubqueryRows { rows: values.len() }),
            };
            expression.nullable = matches!(value, Value::Null);
            expression.kind = BoundExprKind::Literal(value);
        }
        BoundExprKind::ExistsSubquery { query, negated } => {
            let mut query = (**query).clone();
            substitute_outer_query(&mut query, batch, row, columns)?;
            let values = materialize_subquery(
                query,
                provider,
                dependent_subquery_memory_limit(memory, batch)?,
                memory.deadline,
                Some(1),
            )?;
            expression.kind = BoundExprKind::Literal(Value::Boolean(values.is_empty() == *negated));
            expression.nullable = false;
        }
        BoundExprKind::InSubquery {
            expr,
            query,
            negated,
        } => {
            resolve_dependent_expr_subqueries(expr, batch, row, columns, provider, memory)?;
            let projection_type = query
                .projection
                .first()
                .and_then(|projection| projection.expr.data_type);
            let mut query = (**query).clone();
            substitute_outer_query(&mut query, batch, row, columns)?;
            let values = materialize_subquery(
                query,
                provider,
                dependent_subquery_memory_limit(memory, batch)?,
                memory.deadline,
                None,
            )?;
            let mut args = Vec::with_capacity(values.len().saturating_add(1));
            args.push((**expr).clone());
            args.extend(values.into_iter().map(|value| BoundExpr {
                data_type: projection_type.or_else(|| value.data_type()),
                nullable: matches!(value, Value::Null),
                kind: BoundExprKind::Literal(value),
            }));
            expression.kind = BoundExprKind::Scalar {
                function: ScalarFunction::InList { negated: *negated },
                args,
            };
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            resolve_dependent_expr_subqueries(expr, batch, row, columns, provider, memory)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            resolve_dependent_expr_subqueries(left, batch, row, columns, provider, memory)?;
            resolve_dependent_expr_subqueries(right, batch, row, columns, provider, memory)?;
        }
        BoundExprKind::Scalar { function, args } => {
            resolve_dependent_scalar_args(*function, args, batch, row, columns, provider, memory)?;
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
    Ok(())
}

fn resolve_dependent_scalar_args(
    function: ScalarFunction,
    args: &mut [BoundExpr],
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    match function {
        ScalarFunction::If if args.len() == 3 => {
            resolve_dependent_expr_subqueries(&mut args[0], batch, row, columns, provider, memory)?;
            let condition = evaluate_and_literalize(&mut args[0], batch, row, columns)?;
            let selected = if predicate_truth(&condition)? { 1 } else { 2 };
            let skipped = if selected == 1 { 2 } else { 1 };
            resolve_dependent_expr_subqueries(
                &mut args[selected],
                batch,
                row,
                columns,
                provider,
                memory,
            )?;
            args[skipped].nullable = true;
            args[skipped].kind = BoundExprKind::Literal(Value::Null);
        }
        ScalarFunction::Coalesce => {
            for index in 0..args.len() {
                resolve_dependent_expr_subqueries(
                    &mut args[index],
                    batch,
                    row,
                    columns,
                    provider,
                    memory,
                )?;
                let value = evaluate_and_literalize(&mut args[index], batch, row, columns)?;
                if !matches!(value, Value::Null) {
                    for skipped in &mut args[index + 1..] {
                        skipped.nullable = true;
                        skipped.kind = BoundExprKind::Literal(Value::Null);
                    }
                    break;
                }
            }
        }
        _ => {
            for argument in args {
                resolve_dependent_expr_subqueries(argument, batch, row, columns, provider, memory)?;
            }
        }
    }
    Ok(())
}

fn evaluate_and_literalize(
    expression: &mut BoundExpr,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
) -> Result<Value, ExecError> {
    let value = CompiledExpr::compile(expression, columns)?.evaluate(batch, row)?;
    expression.nullable = matches!(value, Value::Null);
    expression.kind = BoundExprKind::Literal(value.clone());
    Ok(value)
}

fn dependent_subquery_memory_limit(
    memory: &MemoryTracker,
    outer_batch: &RecordBatch,
) -> Result<usize, ExecError> {
    let outer_bytes = outer_batch.estimated_bytes();
    memory.ensure_transient(outer_bytes)?;
    Ok(memory.remaining().saturating_sub(outer_bytes))
}

fn materialize_subquery(
    query: pintail_sql::BoundQuery,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<Instant>,
    maximum_rows: Option<usize>,
) -> Result<Vec<Value>, ExecError> {
    let logical = Optimizer::optimize(LogicalPlanner::plan(query));
    let physical = PhysicalPlanner::plan(logical)?;
    if physical.output_fields().len() != 1 {
        return Err(ExecError::InvalidPhysicalPlan(
            "scalar or IN subquery must produce exactly one column",
        ));
    }
    let mut execution = Execution::start_with_deadline(physical, provider, memory_limit, deadline)?;
    let mut values = Vec::new();
    let mut used = size_of::<Vec<Value>>();
    while let Some(batch) = execution.next_batch()? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            let value = batch
                .column(0)
                .and_then(|column| column.value(row))
                .cloned()
                .ok_or(ExecError::InvalidBatch(
                    "subquery result is missing its scalar column",
                ))?;
            let bytes = size_of::<Value>().saturating_add(value.heap_bytes());
            let live_bytes = execution
                .memory()
                .used()
                .saturating_add(batch_bytes)
                .saturating_add(used);
            if live_bytes.saturating_add(bytes) > memory_limit {
                return Err(ExecError::MemoryLimitExceeded {
                    used: live_bytes,
                    requested: bytes,
                    limit: memory_limit,
                });
            }
            used += bytes;
            values.push(value);
            if maximum_rows.is_some_and(|maximum| values.len() >= maximum) {
                return Ok(values);
            }
        }
    }
    Ok(values)
}

fn reserve_subquery_values(
    values: &[Value],
    memory_limit: usize,
    retained_bytes: &mut usize,
) -> Result<(), ExecError> {
    let bytes = values.iter().fold(0_usize, |bytes, value| {
        bytes
            .saturating_add(size_of::<BoundExpr>())
            .saturating_add(value.heap_bytes())
    });
    if retained_bytes.saturating_add(bytes) > memory_limit {
        return Err(ExecError::MemoryLimitExceeded {
            used: *retained_bytes,
            requested: bytes,
            limit: memory_limit,
        });
    }
    *retained_bytes += bytes;
    Ok(())
}

enum PullOperator {
    Empty,
    OneRow {
        emitted: bool,
    },
    Scan {
        stream: Box<dyn BatchStream>,
        expected_types: Vec<DataType>,
    },
    CrossJoin {
        inputs: Vec<Self>,
        column_types: Vec<DataType>,
        state: Option<CrossJoinState>,
    },
    UnionAll {
        inputs: Vec<Self>,
        current: usize,
    },
    HashJoin {
        left: Box<Self>,
        right: Box<Self>,
        kind: BoundJoinKind,
        left_key: CompiledExpr,
        right_key: CompiledExpr,
        extra_keys: Vec<(CompiledExpr, CompiledExpr, JoinKeyMode)>,
        key_mode: JoinKeyMode,
        column_types: Vec<DataType>,
        right_width: usize,
        state: Option<Box<HashJoinState>>,
    },
    Filter {
        input: Box<Self>,
        predicate: CompiledExpr,
    },
    HashAggregate {
        input: Box<Self>,
        group_by: Vec<CompiledExpr>,
        aggregates: Vec<CompiledAggregate>,
        column_types: Vec<DataType>,
        state: Option<MaterializedRows>,
    },
    Project {
        input: Box<Self>,
        expressions: Vec<(CompiledExpr, Option<DataType>)>,
    },
    Distinct {
        input: Box<Self>,
        column_types: Vec<DataType>,
        state: Option<DistinctRows>,
    },
    SetOp {
        left: Option<Box<Self>>,
        right: Option<Box<Self>>,
        keep_matching: bool,
        all: bool,
        column_types: Vec<DataType>,
        state: Option<SetOpRows>,
    },
    /// Pre-materialized rows (recursive-CTE fixpoint output).
    Rows {
        rows: Vec<Vec<Value>>,
        cursor: usize,
        column_types: Vec<DataType>,
    },
    Sort {
        input: Box<Self>,
        keys: Vec<BoundOrderKey>,
        column_types: Vec<DataType>,
        top_k: Option<usize>,
        trim: usize,
        state: Option<SortedRows>,
    },
    Window {
        input: Box<Self>,
        windows: Vec<CompiledWindow>,
        column_types: Vec<DataType>,
        state: Option<MaterializedRows>,
    },
    Limit {
        input: Box<Self>,
        skip: u64,
        take: u64,
    },
}

impl PullOperator {
    /// Forwards a probe-side key restriction to the underlying scan, passing
    /// through filters only — any other operator changes row identity or
    /// layout and stops the pushdown.
    fn restrict_probe_range(&mut self, position: usize, min: &Value, max: &Value) {
        match self {
            Self::Scan { stream, .. } => stream.restrict_key_position_range(position, min, max),
            Self::Filter { input, .. } => input.restrict_probe_range(position, min, max),
            _ => {}
        }
    }

    /// Transient headroom the underlying scan needs to pull one more batch.
    /// Operators that buffer their whole input (the two-pass aggregate) must
    /// keep at least this much budget free or the scan itself stops pulling.
    fn scan_transient_floor(&self) -> usize {
        match self {
            Self::Scan { stream, .. } => stream.next_batch_memory_upper_bound(usize::MAX),
            Self::Filter { input, .. } => input.scan_transient_floor(),
            _ => 0,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, memory: &MemoryTracker) -> Result<Option<RecordBatch>, ExecError> {
        memory.check_deadline()?;
        match self {
            Self::Empty => Ok(None),
            Self::OneRow { emitted } => {
                if *emitted {
                    return Ok(None);
                }
                *emitted = true;
                let batch = RecordBatch::new(1, Vec::new())?;
                memory.ensure_transient(batch.estimated_bytes())?;
                Ok(Some(batch))
            }
            Self::Scan {
                stream,
                expected_types,
            } => {
                memory
                    .ensure_transient(stream.next_batch_memory_upper_bound(memory.remaining()))?;
                let retained_before = stream.retained_bytes();
                let batch = stream.next_batch(memory.remaining())?;
                let retained_after = stream.retained_bytes();
                if retained_after > retained_before {
                    memory.reserve(retained_after - retained_before)?;
                } else {
                    memory.release(retained_before - retained_after);
                }
                let Some(batch) = batch else {
                    return Ok(None);
                };
                validate_scan_batch(&batch, expected_types)?;
                memory.ensure_transient(batch.estimated_bytes())?;
                Ok(Some(batch))
            }
            Self::CrossJoin {
                inputs,
                column_types,
                state,
            } => {
                if state.is_none() {
                    let mut materialized = Vec::with_capacity(inputs.len());
                    for input in inputs {
                        materialized.push(materialize(input, memory)?);
                    }
                    *state = Some(CrossJoinState::new(materialized));
                }
                let state = state.as_mut().expect("initialized above");
                let transient =
                    state.next_batch_memory_upper_bound(DEFAULT_BATCH_ROWS, column_types.len());
                memory.ensure_transient(transient)?;
                let rows = state.next_rows(DEFAULT_BATCH_ROWS);
                if rows.is_empty() {
                    return Ok(None);
                }
                let columns = rows_to_columns(&rows, column_types)?;
                let batch = RecordBatch::new(rows.len(), columns)?;
                memory.ensure_transient(batch.estimated_bytes())?;
                Ok(Some(batch))
            }
            Self::UnionAll { inputs, current } => {
                while let Some(input) = inputs.get_mut(*current) {
                    if let Some(batch) = input.next_batch(memory)? {
                        return Ok(Some(batch));
                    }
                    *current = current.saturating_add(1);
                }
                Ok(None)
            }
            Self::HashJoin {
                left,
                right,
                kind,
                left_key,
                right_key,
                extra_keys,
                key_mode,
                column_types,
                right_width,
                state,
            } => {
                if state.is_none() {
                    let built =
                        build_hash_join_state(right, right_key, *key_mode, extra_keys, memory)?;
                    // Inner and semi joins cannot match probe rows outside
                    // the build side's key range, so the probe scan can prune
                    // storage before decoding anything. Left/anti joins need
                    // every probe row.
                    if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Semi)
                        && let Some((minimum, maximum)) = &built.key_bounds
                        && let Some(position) = left_key.column_index()
                    {
                        left.restrict_probe_range(position, minimum, maximum);
                    }
                    *state = Some(Box::new(built));
                }
                next_hash_join_batch(
                    left,
                    *kind,
                    left_key,
                    *key_mode,
                    extra_keys,
                    *right_width,
                    column_types,
                    state.as_mut().expect("initialized above"),
                    memory,
                )
            }
            Self::Filter { input, predicate } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                // Typed batch kernel: comparison predicates over packed
                // columns resolve in one pass; anything else falls back to
                // the row-at-a-time path below.
                if let Some(mask) = predicate.evaluate_filter_mask(&batch)? {
                    batch.selection_mut().intersect(&mask)?;
                    if batch.visible_row_count() > 0 {
                        return Ok(Some(batch));
                    }
                    continue;
                }
                let batch_bytes = batch.estimated_bytes();
                for row in 0..batch.row_count() {
                    if !batch.selection().is_selected(row) {
                        continue;
                    }
                    let keep =
                        if let Some(keep) = predicate.evaluate_predicate_direct(&batch, row)? {
                            keep
                        } else {
                            memory
                                .ensure_transient(batch_bytes.saturating_add(
                                    predicate.allocation_upper_bound(&batch, row),
                                ))?;
                            predicate_truth(&predicate.evaluate(&batch, row)?)?
                        };
                    if !keep {
                        batch.selection_mut().set(row, false)?;
                    }
                }
                if batch.visible_row_count() > 0 {
                    return Ok(Some(batch));
                }
            },
            Self::HashAggregate {
                input,
                group_by,
                aggregates,
                column_types,
                state,
            } => {
                if state.is_none() {
                    *state = Some(build_hash_aggregate(input, group_by, aggregates, memory)?);
                }
                next_materialized_batch(
                    state.as_mut().expect("initialized above"),
                    column_types,
                    memory,
                )
            }
            Self::Project { input, expressions } => {
                let Some(batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                let batch_bytes = batch.estimated_bytes();
                let expression_memory = expressions
                    .iter()
                    .map(|(expression, _)| {
                        batch
                            .selection()
                            .selected_rows()
                            .map(|row| expression.allocation_upper_bound(&batch, row))
                            .fold(0_usize, usize::saturating_add)
                    })
                    .fold(0_usize, usize::saturating_add);
                let projected_memory = size_of::<RecordBatch>()
                    .saturating_add(
                        expressions
                            .len()
                            .saturating_mul(size_of::<ColumnVector>().saturating_mul(2)),
                    )
                    .saturating_add(
                        expressions
                            .len()
                            .saturating_mul(batch.row_count())
                            .saturating_mul(size_of::<Value>()),
                    )
                    .saturating_add(
                        batch
                            .row_count()
                            .div_ceil(64)
                            .saturating_mul(size_of::<u64>()),
                    )
                    .saturating_add(expression_memory);
                memory.ensure_transient(batch_bytes.saturating_add(projected_memory))?;
                let mut columns = Vec::with_capacity(expressions.len());
                for (expression, data_type) in expressions {
                    let mut values = Vec::with_capacity(batch.row_count());
                    for row in 0..batch.row_count() {
                        if batch.selection().is_selected(row) {
                            values.push(expression.evaluate(&batch, row)?);
                        } else {
                            values.push(Value::Null);
                        }
                    }
                    let data_type = data_type.unwrap_or(DataType::Utf8);
                    columns.push(ColumnVector::new(data_type, values)?);
                }
                let mut output = RecordBatch::new(batch.row_count(), columns)?;
                output.set_selection(batch.selection().clone())?;
                memory.ensure_transient(batch_bytes.saturating_add(output.estimated_bytes()))?;
                Ok(Some(output))
            }
            Self::Rows {
                rows,
                cursor,
                column_types,
            } => {
                if *cursor >= rows.len() {
                    return Ok(None);
                }
                let end = (*cursor + DEFAULT_BATCH_ROWS).min(rows.len());
                let chunk = &rows[*cursor..end];
                *cursor = end;
                let chunk_bytes = chunk
                    .iter()
                    .map(|row| estimated_row_payload_bytes(row))
                    .fold(0_usize, usize::saturating_add);
                memory.ensure_transient(chunk_bytes)?;
                let columns = rows_to_columns(chunk, column_types)?;
                Ok(Some(RecordBatch::new(chunk.len(), columns)?))
            }
            Self::SetOp {
                left,
                right,
                keep_matching,
                all,
                column_types,
                state,
            } => {
                if state.is_none() {
                    let mut left = left.take().ok_or(ExecError::InvalidPhysicalPlan(
                        "set operation has no left input",
                    ))?;
                    let mut right = right.take().ok_or(ExecError::InvalidPhysicalPlan(
                        "set operation has no right input",
                    ))?;
                    *state = Some(build_set_operation(
                        &mut left,
                        &mut right,
                        column_types,
                        *keep_matching,
                        *all,
                        memory,
                    )?);
                }
                state
                    .as_mut()
                    .expect("initialized above")
                    .next_batch(column_types, memory)
            }
            Self::Distinct {
                input,
                column_types,
                state,
            } => {
                if state.is_none() {
                    *state = Some(build_distinct(input, column_types, memory)?);
                }
                state
                    .as_mut()
                    .expect("initialized above")
                    .next_batch(column_types, memory)
            }
            Self::Window {
                input,
                windows,
                column_types,
                state,
            } => {
                if state.is_none() {
                    *state = Some(build_window(input, windows, memory)?);
                }
                next_materialized_batch(
                    state.as_mut().expect("initialized above"),
                    column_types,
                    memory,
                )
            }
            Self::Sort {
                input,
                keys,
                column_types,
                top_k,
                trim,
                state,
            } => {
                if state.is_none() {
                    let trim_to = if *trim > 0 {
                        // Hidden sort-only columns ordered the rows; the
                        // output layout never contains them.
                        Some(column_types.len())
                    } else {
                        None
                    };
                    *state = Some(build_sort(input, keys, *top_k, trim_to, memory)?);
                }
                state
                    .as_mut()
                    .expect("initialized above")
                    .next_batch(column_types, memory)
            }
            Self::Limit { input, skip, take } => {
                if *take == 0 {
                    return Ok(None);
                }
                loop {
                    let Some(mut batch) = input.next_batch(memory)? else {
                        return Ok(None);
                    };
                    let selected = batch.selection().selected_rows().collect::<Vec<_>>();
                    for row in selected {
                        if *skip > 0 {
                            *skip -= 1;
                            batch.selection_mut().set(row, false)?;
                        } else if *take > 0 {
                            *take -= 1;
                        } else {
                            batch.selection_mut().set(row, false)?;
                        }
                    }
                    if batch.visible_row_count() > 0 {
                        return Ok(Some(batch));
                    }
                    if *take == 0 {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn build_operator(
    plan: PhysicalPlan,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
) -> Result<(PullOperator, Vec<BoundColumn>), ExecError> {
    match plan {
        PhysicalPlan::Empty => Ok((PullOperator::Empty, Vec::new())),
        PhysicalPlan::OneRow => Ok((PullOperator::OneRow { emitted: false }, Vec::new())),
        PhysicalPlan::Scan(scan) => {
            let columns = scan
                .projected_column_ids
                .iter()
                .map(|id| {
                    scan.table
                        .columns
                        .iter()
                        .find(|column| column.column_id == *id)
                        .cloned()
                        .ok_or(ExecError::InvalidPhysicalPlan(
                            "scan projection references an unknown stable column ID",
                        ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let expected_types = columns.iter().map(|column| column.data_type).collect();
            let predicates = scan
                .predicates
                .iter()
                .map(|predicate| CompiledExpr::compile(predicate, &columns))
                .collect::<Result<Vec<_>, _>>()?;
            let stream = provider.open_scan(&scan, memory.remaining())?;
            memory.reserve(stream.retained_bytes())?;
            let mut operator = PullOperator::Scan {
                stream,
                expected_types,
            };
            for predicate in predicates {
                operator = PullOperator::Filter {
                    input: Box::new(operator),
                    predicate,
                };
            }
            Ok((operator, columns))
        }
        PhysicalPlan::Derived { input, columns } => {
            let fields = input.output_fields();
            if fields.len() != columns.len()
                || fields
                    .iter()
                    .zip(&columns)
                    .any(|(field, column)| field.data_type != Some(column.data_type))
            {
                return Err(ExecError::InvalidPhysicalPlan(
                    "derived input layout does not match its bound columns",
                ));
            }
            let (input, _) = build_operator(*input, provider, memory)?;
            Ok((input, columns))
        }
        PhysicalPlan::CrossJoin {
            inputs,
            estimated_rows: _,
        } => {
            let mut built = Vec::with_capacity(inputs.len());
            for input in inputs {
                built.push(build_operator(input, provider, memory)?);
            }
            let mut operators = Vec::with_capacity(built.len());
            let mut columns = Vec::new();
            for (operator, input_columns) in built {
                operators.push(operator);
                columns.extend(input_columns);
            }
            let column_types = columns.iter().map(|column| column.data_type).collect();
            Ok((
                PullOperator::CrossJoin {
                    inputs: operators,
                    column_types,
                    state: None,
                },
                columns,
            ))
        }
        PhysicalPlan::UnionAll { inputs } => {
            let layouts = inputs
                .iter()
                .map(PhysicalPlan::output_fields)
                .collect::<Vec<_>>();
            validate_union_fields(&layouts)?;
            let mut built = Vec::with_capacity(inputs.len());
            for input in inputs {
                built.push(build_operator(input, provider, memory)?);
            }
            let (operators, columns): (Vec<_>, Vec<_>) = built.into_iter().unzip();
            let columns = columns.into_iter().next().unwrap_or_default();
            Ok((
                PullOperator::UnionAll {
                    inputs: operators,
                    current: 0,
                },
                columns,
            ))
        }
        PhysicalPlan::HashJoin {
            left,
            right,
            kind,
            left_key,
            right_key,
            extra_keys,
        } => {
            let (left, left_columns) = build_operator(*left, provider, memory)?;
            let (right, right_columns) = build_operator(*right, provider, memory)?;
            let key_mode = hash_join_key_mode(left_key.data_type, right_key.data_type).ok_or(
                ExecError::InvalidPhysicalPlan("hash join keys have incompatible scalar types"),
            )?;
            let extra_keys = extra_keys
                .into_iter()
                .map(|(extra_left, extra_right)| {
                    let mode = hash_join_key_mode(extra_left.data_type, extra_right.data_type)
                        .ok_or(ExecError::InvalidPhysicalPlan(
                            "hash join keys have incompatible scalar types",
                        ))?;
                    Ok((
                        CompiledExpr::compile(&extra_left, &left_columns)?,
                        CompiledExpr::compile(&extra_right, &right_columns)?,
                        mode,
                    ))
                })
                .collect::<Result<Vec<_>, ExecError>>()?;
            let left_key = CompiledExpr::compile(&left_key, &left_columns)?;
            let right_key = CompiledExpr::compile(&right_key, &right_columns)?;
            let right_width = right_columns.len();
            let mut output_columns = left_columns;
            if !matches!(kind, BoundJoinKind::Semi | BoundJoinKind::Anti) {
                output_columns.extend(right_columns);
            }
            let column_types = output_columns
                .iter()
                .map(|column| column.data_type)
                .collect();
            Ok((
                PullOperator::HashJoin {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind,
                    left_key,
                    right_key,
                    extra_keys,
                    key_mode,
                    column_types,
                    right_width,
                    state: None,
                },
                output_columns,
            ))
        }
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            kind,
            condition,
        } => {
            let (mut left, left_columns) = build_operator(*left, provider, memory)?;
            let (mut right, right_columns) = build_operator(*right, provider, memory)?;
            let left_rows = materialize(&mut left, memory)?;
            let right_rows = materialize(&mut right, memory)?;
            let mut output_columns = left_columns.clone();
            if !matches!(kind, BoundJoinKind::Semi | BoundJoinKind::Anti) {
                output_columns.extend(right_columns.clone());
            }
            let column_types = output_columns
                .iter()
                .map(|column| column.data_type)
                .collect::<Vec<_>>();
            let rows = execute_nested_loop_join(
                &left_rows,
                &right_rows,
                &left_columns,
                &right_columns,
                kind,
                &condition,
                provider,
                memory,
            )?;
            Ok((
                PullOperator::Rows {
                    rows,
                    cursor: 0,
                    column_types,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Filter { input, predicate } => {
            let dependent = expression_has_dependent_subquery(&predicate);
            let (mut input, columns) = build_operator(*input, provider, memory)?;
            if dependent {
                let column_types = columns
                    .iter()
                    .map(|column| column.data_type)
                    .collect::<Vec<_>>();
                let mut rows = Vec::new();
                while let Some(batch) = input.next_batch(memory)? {
                    let batch_bytes = batch.estimated_bytes();
                    for row in batch.selection().selected_rows() {
                        let mut expression = predicate.clone();
                        resolve_dependent_expr_subqueries(
                            &mut expression,
                            &batch,
                            row,
                            &columns,
                            provider,
                            memory,
                        )?;
                        let compiled = CompiledExpr::compile(&expression, &columns)?;
                        if !predicate_truth(&compiled.evaluate(&batch, row)?)? {
                            continue;
                        }
                        let values = batch_row(&batch, row)?;
                        let row_bytes = estimated_row_payload_bytes(&values);
                        memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
                        memory.reserve(row_bytes)?;
                        rows.push(values);
                    }
                }
                return Ok((
                    PullOperator::Rows {
                        rows,
                        cursor: 0,
                        column_types,
                    },
                    columns,
                ));
            }
            let predicate = CompiledExpr::compile(&predicate, &columns)?;
            Ok((
                PullOperator::Filter {
                    input: Box::new(input),
                    predicate,
                },
                columns,
            ))
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => {
            let (input, columns) = build_operator(*input, provider, memory)?;
            let column_types = group_by
                .iter()
                .map(|expression| expression.data_type.unwrap_or(DataType::Utf8))
                .chain(
                    aggregates
                        .iter()
                        .map(|aggregate| aggregate.data_type.unwrap_or(DataType::Utf8)),
                )
                .collect();
            // The aggregate's output schema is positional (group keys, then
            // aggregate results). Downstream operators that sit between the
            // aggregate and the final projection — the window operator — need
            // real column entries so their own type layout and appended
            // outputs line up; synthetic identities keep Column resolution
            // unambiguous, matching the window-output convention.
            let synthetic =
                |index: usize, data_type: Option<DataType>, nullable: bool| BoundColumn {
                    database_id: DatabaseId::new(u64::MAX),
                    table_id: TableId::new(u64::MAX - 1),
                    column_id: u32::try_from(index).unwrap_or(u32::MAX),
                    relation_name: "<aggregate>".to_owned(),
                    name: format!("<aggregate-{index}>"),
                    data_type: data_type.unwrap_or(DataType::Utf8),
                    nullable,
                    collation: None,
                    outer: false,
                    using_shadowed: false,
                };
            let mut output_columns = group_by
                .iter()
                .enumerate()
                .map(|(index, expression)| match &expression.kind {
                    BoundExprKind::Column(column) => {
                        let mut column = column.clone();
                        column.outer = false;
                        column.nullable = expression.nullable;
                        column
                    }
                    _ => synthetic(index, expression.data_type, expression.nullable),
                })
                .collect::<Vec<_>>();
            output_columns.extend(aggregates.iter().enumerate().map(|(offset, aggregate)| {
                synthetic(
                    group_by.len().saturating_add(offset),
                    aggregate.data_type,
                    aggregate.nullable,
                )
            }));
            let group_by = group_by
                .iter()
                .map(|expression| CompiledExpr::compile(expression, &columns))
                .collect::<Result<Vec<_>, _>>()?;
            let aggregates = aggregates
                .iter()
                .map(|aggregate| CompiledAggregate::compile(aggregate, &columns))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                PullOperator::HashAggregate {
                    input: Box::new(input),
                    group_by,
                    aggregates,
                    column_types,
                    state: None,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Project { input, expressions } => {
            let dependent = expressions
                .iter()
                .any(|projection| expression_has_dependent_subquery(&projection.expr));
            let (mut input, columns) = build_operator(*input, provider, memory)?;
            let output_columns = expressions
                .iter()
                .enumerate()
                .map(|(index, projection)| match &projection.expr.kind {
                    BoundExprKind::Column(column) => {
                        let mut column = column.clone();
                        column.outer = false;
                        column.nullable = projection.expr.nullable;
                        column
                    }
                    _ => BoundColumn {
                        database_id: DatabaseId::new(u64::MAX),
                        table_id: TableId::new(u64::MAX - 2),
                        column_id: u32::try_from(index).unwrap_or(u32::MAX),
                        relation_name: "<projection>".to_owned(),
                        name: projection.name.clone(),
                        data_type: projection.expr.data_type.unwrap_or(DataType::Utf8),
                        nullable: projection.expr.nullable,
                        collation: None,
                        outer: false,
                        using_shadowed: false,
                    },
                })
                .collect::<Vec<_>>();
            if dependent {
                let column_types = expressions
                    .iter()
                    .map(|projection| projection.expr.data_type.unwrap_or(DataType::Utf8))
                    .collect::<Vec<_>>();
                let mut rows = Vec::new();
                while let Some(batch) = input.next_batch(memory)? {
                    let batch_bytes = batch.estimated_bytes();
                    for row in batch.selection().selected_rows() {
                        let mut values = Vec::with_capacity(expressions.len());
                        for projection in &expressions {
                            let mut expression = projection.expr.clone();
                            resolve_dependent_expr_subqueries(
                                &mut expression,
                                &batch,
                                row,
                                &columns,
                                provider,
                                memory,
                            )?;
                            let compiled = CompiledExpr::compile(&expression, &columns)?;
                            values.push(compiled.evaluate(&batch, row)?);
                        }
                        let row_bytes = estimated_row_payload_bytes(&values);
                        memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
                        memory.reserve(row_bytes)?;
                        rows.push(values);
                    }
                }
                return Ok((
                    PullOperator::Rows {
                        rows,
                        cursor: 0,
                        column_types,
                    },
                    output_columns,
                ));
            }
            let expressions = expressions
                .iter()
                .map(|projection| {
                    Ok((
                        CompiledExpr::compile(&projection.expr, &columns)?,
                        projection.expr.data_type,
                    ))
                })
                .collect::<Result<Vec<_>, ExecError>>()?;
            Ok((
                PullOperator::Project {
                    input: Box::new(input),
                    expressions,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Recursive {
            working,
            distinct,
            anchor,
            member,
        } => {
            // The working-table layout comes from the anchor's client-visible
            // fields, independent from internal synthetic projection IDs.
            let column_types: Vec<DataType> = anchor
                .output_fields()
                .iter()
                .map(|field| field.data_type.unwrap_or(DataType::Utf8))
                .collect();
            let (mut anchor_op, columns) = build_operator(*anchor, provider, memory)?;
            let mut seen: HashSet<Vec<Value>> = HashSet::new();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let mut delta =
                drain_recursive_rows(&mut anchor_op, distinct, &mut seen, &mut rows, memory)?;
            let recursion_limit = SESSION_CTE_MAX_RECURSION_DEPTH.get();
            let mut iterations: u64 = 0;
            while !delta.is_empty() {
                iterations += 1;
                if iterations > recursion_limit {
                    return Err(ExecError::RecursionDepthExceeded {
                        limit: recursion_limit,
                    });
                }
                let overlay = RecursiveWorkingProvider {
                    base: provider,
                    working,
                    column_types: &column_types,
                    delta: &delta,
                };
                let (mut member_op, _) = build_operator((*member).clone(), &overlay, memory)?;
                delta =
                    drain_recursive_rows(&mut member_op, distinct, &mut seen, &mut rows, memory)?;
            }
            Ok((
                PullOperator::Rows {
                    rows,
                    cursor: 0,
                    column_types,
                },
                columns,
            ))
        }
        PhysicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => {
            let (left, columns) = build_operator(*left, provider, memory)?;
            let (right, _) = build_operator(*right, provider, memory)?;
            let column_types = columns
                .iter()
                .map(|column| column.data_type)
                .collect::<Vec<_>>();
            Ok((
                PullOperator::SetOp {
                    left: Some(Box::new(left)),
                    right: Some(Box::new(right)),
                    keep_matching,
                    all,
                    column_types,
                    state: None,
                },
                columns,
            ))
        }
        PhysicalPlan::Distinct { input } => {
            let column_types = input
                .output_fields()
                .into_iter()
                .map(|field| field.data_type.unwrap_or(DataType::Utf8))
                .collect();
            let (input, columns) = build_operator(*input, provider, memory)?;
            Ok((
                PullOperator::Distinct {
                    input: Box::new(input),
                    column_types,
                    state: None,
                },
                columns,
            ))
        }
        PhysicalPlan::Window {
            input,
            windows,
            outputs,
        } => {
            let (input_op, mut columns) = build_operator(*input, provider, memory)?;
            let compiled = windows
                .iter()
                .map(|window| CompiledWindow::compile(window, &columns))
                .collect::<Result<Vec<_>, _>>()?;
            let mut column_types = columns
                .iter()
                .map(|column| column.data_type)
                .collect::<Vec<_>>();
            column_types.extend(outputs.iter().map(|column| column.data_type));
            columns.extend(outputs);
            Ok((
                PullOperator::Window {
                    input: Box::new(input_op),
                    windows: compiled,
                    column_types,
                    state: None,
                },
                columns,
            ))
        }
        PhysicalPlan::Sort {
            input,
            keys,
            top_k,
            trim,
        } => {
            let mut column_types = input
                .output_fields()
                .into_iter()
                .map(|field| field.data_type.unwrap_or(DataType::Utf8))
                .collect::<Vec<_>>();
            if keys.iter().any(|key| key.index >= column_types.len()) {
                return Err(ExecError::InvalidPhysicalPlan(
                    "sort key is outside the projected result layout",
                ));
            }
            let visible = column_types.len().saturating_sub(trim);
            column_types.truncate(visible);
            let (input, mut columns) = build_operator(*input, provider, memory)?;
            columns.truncate(visible);
            Ok((
                PullOperator::Sort {
                    input: Box::new(input),
                    keys,
                    column_types,
                    top_k,
                    trim,
                    state: None,
                },
                columns,
            ))
        }
        PhysicalPlan::Limit {
            input,
            offset,
            count,
        } => {
            let (input, columns) = build_operator(*input, provider, memory)?;
            Ok((
                PullOperator::Limit {
                    input: Box::new(input),
                    skip: offset,
                    take: count,
                },
                columns,
            ))
        }
    }
}

fn validate_union_fields(layouts: &[Vec<OutputField>]) -> Result<(), ExecError> {
    let Some(first) = layouts.first() else {
        return Err(ExecError::InvalidPhysicalPlan(
            "UNION ALL requires at least one input",
        ));
    };
    if layouts.iter().skip(1).any(|layout| {
        layout.len() != first.len()
            || layout
                .iter()
                .zip(first)
                .any(|(field, expected)| field.data_type != expected.data_type)
    }) {
        return Err(ExecError::InvalidPhysicalPlan(
            "UNION ALL input layouts are incompatible",
        ));
    }
    Ok(())
}

/// Dense direct-address join table: (minimum key, per-offset build buckets).
type DenseJoinTable<'a> = (i128, Vec<Option<&'a Vec<Vec<Value>>>>);

/// Group identity resolved ONCE from the build side. Group columns of a
/// fused join are build-side by construction, so the complete group set is
/// known before probing: workers then index groups directly instead of
/// hashing and comparing group values per probe row (the Q8 profile's
/// dominant cost, 2026-08-02).
struct JoinGroupPlan {
    /// Group key values in index order.
    values: Vec<Vec<Value>>,
    /// Per build bucket (keyed by its address), the group index of each row.
    buckets: HashMap<usize, Vec<usize>>,
}

fn resolve_join_group_plan(
    build: &HashMap<JoinHashKey, Vec<Vec<Value>>>,
    right_group_columns: &[usize],
) -> Result<JoinGroupPlan, ExecError> {
    let mut values = Vec::new();
    let mut index = HashMap::<Vec<Value>, usize>::new();
    let mut buckets = HashMap::with_capacity(build.len());
    for bucket in build.values() {
        let mut indexes = Vec::with_capacity(bucket.len());
        for row in bucket {
            let group_values = right_group_columns
                .iter()
                .map(|column| {
                    row.get(*column)
                        .cloned()
                        .ok_or(ExecError::InvalidPhysicalPlan(
                            "join aggregate group is outside the build-side layout",
                        ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let key = group_values
                .iter()
                .cloned()
                .map(normalized_collation_value)
                .collect::<Vec<_>>();
            let position = *index.entry(key).or_insert_with(|| {
                values.push(group_values);
                values.len() - 1
            });
            indexes.push(position);
        }
        buckets.insert(std::ptr::from_ref(bucket) as usize, indexes);
    }
    Ok(JoinGroupPlan { values, buckets })
}

/// Widest key span the dense join table will materialize (~4M slots).
const MAX_DENSE_SPAN: i128 = 1 << 22;

struct HashJoinState {
    build: HashMap<JoinHashKey, Vec<Vec<Value>>>,
    /// Engaged when the build side overflowed: partitioned files replace
    /// the resident map and probing runs partition by partition.
    grace: Option<GraceJoin>,
    /// Min/max of non-null build keys, for probe-side scan restriction.
    key_bounds: Option<(Value, Value)>,
    batch: Option<RecordBatch>,
    batch_reserved: usize,
    row: usize,
    match_index: usize,
    left_values: Option<Vec<Value>>,
    left_key: Option<JoinHashKey>,
    left_reserved: usize,
}

impl HashJoinState {
    fn clear_left(&mut self, memory: &MemoryTracker) {
        self.left_values = None;
        self.left_key = None;
        self.match_index = 0;
        memory.release(self.left_reserved);
        self.left_reserved = 0;
    }

    fn clear_batch(&mut self, memory: &MemoryTracker) {
        self.clear_left(memory);
        self.batch = None;
        self.row = 0;
        memory.release(self.batch_reserved);
        self.batch_reserved = 0;
    }
}

#[allow(clippy::too_many_lines)] // one linear build walk with the spill valve
fn build_hash_join_state(
    right: &mut PullOperator,
    right_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    memory: &MemoryTracker,
) -> Result<HashJoinState, ExecError> {
    let mut build: HashMap<JoinHashKey, Vec<Vec<Value>>> = HashMap::new();
    let mut grace: Option<GraceJoin> = None;
    // Bytes reserved for the resident map, measured through used()
    // snapshots so entry, bucket, and payload reserves all count.
    let mut build_reserved = 0_usize;
    let mut key_bounds: Option<(Value, Value)> = None;
    let bound_order = BoundOrderKey {
        index: 0,
        ascending: true,
        nulls_first: true,
        decimal: false,
    };
    while let Some(batch) = right.next_batch(memory)? {
        let used_before_batch = memory.used();
        let batch_bytes = batch.estimated_bytes();
        reserve_hash_map_entries(
            &mut build,
            batch.visible_row_count(),
            size_of::<JoinHashKey>()
                .saturating_add(size_of::<Vec<Vec<Value>>>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            batch_bytes,
            memory,
        )?;
        for row in batch.selection().selected_rows() {
            let row_bytes = estimated_batch_row_bytes(&batch, row)?;
            let key_memory = right_key
                .allocation_upper_bound(&batch, row)
                .saturating_mul(12);
            memory.ensure_transient(
                batch_bytes
                    .saturating_add(row_bytes)
                    .saturating_add(key_memory),
            )?;
            let value = right_key.evaluate(&batch, row)?;
            if !matches!(value, Value::Null) {
                match &mut key_bounds {
                    None => {
                        memory.reserve(value.heap_bytes().saturating_mul(2))?;
                        key_bounds = Some((value.clone(), value.clone()));
                    }
                    Some((minimum, maximum)) => {
                        if compare_sort_values(&value, minimum, bound_order) == Ordering::Less {
                            *minimum = value.clone();
                        }
                        if compare_sort_values(&value, maximum, bound_order) == Ordering::Greater {
                            *maximum = value.clone();
                        }
                    }
                }
            }
            let Some(key) = normalized_join_key(value, key_mode)? else {
                continue;
            };
            let Some(key) = composite_join_key(key, &batch, row, extra_keys, JoinSide::Build)?
            else {
                continue;
            };
            let key_bytes = if build.contains_key(&key) {
                0
            } else {
                key.heap_bytes()
            };
            let row_payload = row_bytes.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(
                batch_bytes
                    .saturating_add(key_memory)
                    .saturating_add(row_payload)
                    .saturating_add(64_usize.saturating_mul(size_of::<Vec<Value>>()))
                    .saturating_add(key_bytes),
            )?;
            if let Some(grace) = grace.as_mut() {
                let values = batch_row(&batch, row)?;
                grace.build_files[grace_partition(&key, 0)].append(&key, &values)?;
                continue;
            }
            memory.reserve(key_bytes)?;
            let bucket = build.entry(key).or_default();
            reserve_vec_elements(bucket, 1, 64, memory)?;
            memory.reserve(row_payload)?;
            let values = batch_row(&batch, row)?;
            bucket.push(values);
        }
        build_reserved =
            build_reserved.saturating_add(memory.used().saturating_sub(used_before_batch));
        // Proactive spill at half the ceiling, like sort and aggregation:
        // drain the resident map into partition files and route the rest
        // of the build (and later the probe) through them.
        if grace.is_none() && build_reserved > memory.limit() / 2 && !build.is_empty() {
            let mut partitions = GraceJoin::create(memory)?;
            for (key, bucket) in build.drain() {
                let target = grace_partition(&key, 0);
                for values in bucket {
                    partitions.build_files[target].append(&key, &values)?;
                }
            }
            memory.release(build_reserved);
            build_reserved = 0;
            grace = Some(partitions);
        }
    }
    Ok(HashJoinState {
        build,
        grace,
        key_bounds,
        batch: None,
        batch_reserved: 0,
        row: 0,
        match_index: 0,
        left_values: None,
        left_key: None,
        left_reserved: 0,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
/// Number of grace-join partitions; key hashes route rows uniformly, so
/// each partition holds roughly build-bytes / 16.
const GRACE_PARTITIONS: usize = 16;

/// One append-mode spill file of `(join key, row values)` pairs.
struct GraceRun {
    writer: Option<std::io::BufWriter<std::fs::File>>,
    path: tempfile::TempPath,
    reservation: spill::SpillReservation,
    entries: u64,
}

impl GraceRun {
    fn create(memory: &MemoryTracker) -> Result<Self, ExecError> {
        let file = spill::spill_file("pintail-join-spill-", memory.spill())
            .map_err(|error| ExecError::Source(format!("join spill create: {error}")))?;
        let (file, path, reservation) = file.into_parts();
        Ok(Self {
            writer: Some(std::io::BufWriter::new(file)),
            path,
            reservation,
            entries: 0,
        })
    }

    fn append(&mut self, key: &JoinHashKey, row: &[Value]) -> Result<(), ExecError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or(ExecError::InvalidPhysicalPlan("grace run already sealed"))?;
        let mut encoder = spill::Encoder::with_capacity(64);
        encode_join_key(&mut encoder, key);
        encoder.values(row);
        spill::write_record_quota(writer, &encoder.finish(), &mut self.reservation)
            .map_err(|error| ExecError::Source(format!("join spill write: {error}")))?;
        self.entries += 1;
        Ok(())
    }

    fn reader(&mut self) -> Result<GraceRunReader, ExecError> {
        use std::io::Seek as _;
        let writer = self
            .writer
            .take()
            .ok_or(ExecError::InvalidPhysicalPlan("grace run read twice"))?;
        let mut file = writer
            .into_inner()
            .map_err(|error| ExecError::Source(format!("join spill flush: {error}")))?;
        file.rewind()
            .map_err(|error| ExecError::Source(format!("join spill rewind: {error}")))?;
        let _ = &self.path;
        Ok(GraceRunReader {
            reader: std::io::BufReader::new(file),
            payload: Vec::new(),
        })
    }
}

/// Streams back one grace-join spill file.
struct GraceRunReader {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
}

impl GraceRunReader {
    fn next_entry(&mut self) -> Result<Option<(JoinHashKey, Vec<Value>)>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("join spill read: {error}")))?
        {
            return Ok(None);
        }
        let mut decoder = spill::Decoder::new(&self.payload);
        let entry = decode_join_key(&mut decoder)
            .and_then(|key| Ok((key, decoder.values()?)))
            .map_err(|error| ExecError::Source(format!("join spill decode: {error}")))?;
        Ok(Some(entry))
    }
}

const JOIN_KEY_NEGATIVE: u8 = 0;
const JOIN_KEY_NON_NEGATIVE: u8 = 1;
const JOIN_KEY_MYSQL_NUMBER: u8 = 2;
const JOIN_KEY_SCALAR: u8 = 3;
const JOIN_KEY_COMPOSITE: u8 = 4;

fn encode_join_key(encoder: &mut spill::Encoder, key: &JoinHashKey) {
    match key {
        JoinHashKey::NegativeInteger(value) => {
            encoder.u8(JOIN_KEY_NEGATIVE);
            encoder.i64(*value);
        }
        JoinHashKey::NonNegativeInteger(value) => {
            encoder.u8(JOIN_KEY_NON_NEGATIVE);
            encoder.u64(*value);
        }
        JoinHashKey::MysqlNumber(value) => {
            encoder.u8(JOIN_KEY_MYSQL_NUMBER);
            encoder.f64(value.get());
        }
        JoinHashKey::Scalar(value) => {
            encoder.u8(JOIN_KEY_SCALAR);
            encoder.value(value);
        }
        JoinHashKey::Composite(parts) => {
            encoder.u8(JOIN_KEY_COMPOSITE);
            encoder.count(parts.len());
            for part in parts {
                encode_join_key(encoder, part);
            }
        }
    }
}

fn decode_join_key(decoder: &mut spill::Decoder<'_>) -> Result<JoinHashKey, String> {
    match decoder.u8()? {
        JOIN_KEY_NEGATIVE => Ok(JoinHashKey::NegativeInteger(decoder.i64()?)),
        JOIN_KEY_NON_NEGATIVE => Ok(JoinHashKey::NonNegativeInteger(decoder.u64()?)),
        JOIN_KEY_MYSQL_NUMBER => Ok(JoinHashKey::MysqlNumber(pintail_types::Float64::new(
            decoder.f64()?,
        ))),
        JOIN_KEY_SCALAR => Ok(JoinHashKey::Scalar(decoder.value()?)),
        JOIN_KEY_COMPOSITE => {
            let count = decoder.count()?;
            let mut parts = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                parts.push(decode_join_key(decoder)?);
            }
            Ok(JoinHashKey::Composite(parts))
        }
        other => Err(format!("spilled join key holds unknown tag {other}")),
    }
}

/// How many times one partition may be split again before a build side that
/// still will not fit is reported as unjoinable skew.
const MAX_GRACE_DEPTH: usize = 3;

fn grace_partition(key: &JoinHashKey, seed: u64) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    key.hash(&mut hasher);
    #[allow(clippy::cast_possible_truncation)] // modulo 16 keeps any width
    {
        (hasher.finish() as usize) % GRACE_PARTITIONS
    }
}

/// Partitioned join state once the build side overflowed the ceiling.
struct GraceJoin {
    build_files: Vec<GraceRun>,
    probe_files: Vec<GraceRun>,
    /// How many times each partition has been re-partitioned. Parallel to
    /// the file vectors, which grow as oversized partitions are split.
    depths: Vec<usize>,
    /// Probe routing finished; partitions are being served.
    probing_done: bool,
    /// Next partition to load in the serve phase.
    current: usize,
    /// The loaded partition's probe entries being replayed.
    replay: Option<GraceRunReader>,
    /// Bytes reserved for the loaded partition's build map.
    partition_reserved: usize,
}

impl GraceJoin {
    fn create(memory: &MemoryTracker) -> Result<Self, ExecError> {
        let mut build_files = Vec::with_capacity(GRACE_PARTITIONS);
        let mut probe_files = Vec::with_capacity(GRACE_PARTITIONS);
        for _ in 0..GRACE_PARTITIONS {
            build_files.push(GraceRun::create(memory)?);
            probe_files.push(GraceRun::create(memory)?);
        }
        Ok(Self {
            build_files,
            probe_files,
            depths: vec![0; GRACE_PARTITIONS],
            probing_done: false,
            current: 0,
            replay: None,
            partition_reserved: 0,
        })
    }
}

/// Splits one partition whose build side did not fit into a fresh round of
/// partitions under a different hash seed, and appends them to the work
/// list. A different seed is the point: rows that collided under the
/// previous one are spread by this one, so a partition that was merely
/// unlucky becomes joinable. Rows sharing a single key follow each other
/// into the same piece no matter the seed, which is why the depth bound
/// exists to end the recursion.
fn split_grace_partition(
    grace: &mut GraceJoin,
    index: usize,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let depth = grace.depths[index];
    if depth >= MAX_GRACE_DEPTH {
        return Err(ExecError::Source(
            "grace join partition still exceeds the memory ceiling after re-partitioning \
             (one join key holds more rows than the ceiling); raise the limit"
                .to_owned(),
        ));
    }
    let first = grace.build_files.len();
    for _ in 0..GRACE_PARTITIONS {
        grace.build_files.push(GraceRun::create(memory)?);
        grace.probe_files.push(GraceRun::create(memory)?);
        grace.depths.push(depth + 1);
    }
    let seed = u64::try_from(depth).unwrap_or(0).saturating_add(1);
    // Move each source file out so its replacement can be written to while
    // the original is read; the emptied slot is never served again.
    let mut build = std::mem::replace(&mut grace.build_files[index], GraceRun::create(memory)?);
    let mut entries = build.reader()?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.build_files[target].append(&key, &values)?;
    }
    let mut probe = std::mem::replace(&mut grace.probe_files[index], GraceRun::create(memory)?);
    let mut entries = probe.reader()?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.probe_files[target].append(&key, &values)?;
    }
    Ok(())
}

/// One join-emit step shared by the in-memory probe loop and the grace
/// replay: produces at most one output row and reports whether this left
/// row is finished.
fn join_emit(
    kind: BoundJoinKind,
    left_values: &[Value],
    matches: Option<&Vec<Vec<Value>>>,
    match_index: &mut usize,
    right_width: usize,
) -> Result<(Option<Vec<Value>>, bool), ExecError> {
    if kind == BoundJoinKind::Scalar && matches.is_some_and(|rows| rows.len() > 1) {
        return Err(ExecError::ScalarSubqueryRows {
            rows: matches.map_or(0, Vec::len),
        });
    }
    let output = match kind {
        BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Scalar => {
            if let Some(right_values) = matches.and_then(|matches| matches.get(*match_index)) {
                *match_index += 1;
                let mut output = left_values.to_vec();
                output.extend(right_values.iter().cloned());
                Some(output)
            } else if matches!(kind, BoundJoinKind::Left | BoundJoinKind::Scalar)
                && *match_index == 0
            {
                *match_index = 1;
                let mut output = left_values.to_vec();
                output.extend(std::iter::repeat_n(Value::Null, right_width));
                Some(output)
            } else {
                None
            }
        }
        BoundJoinKind::Semi if matches.is_some() => Some(left_values.to_vec()),
        BoundJoinKind::Anti if matches.is_none() => Some(left_values.to_vec()),
        BoundJoinKind::Semi | BoundJoinKind::Anti => None,
        BoundJoinKind::Cross => {
            return Err(ExecError::InvalidPhysicalPlan(
                "cross semantics reached hash join",
            ));
        }
    };
    let complete = match kind {
        BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Scalar => {
            *match_index >= matches.map_or(1, Vec::len)
        }
        BoundJoinKind::Semi | BoundJoinKind::Anti => true,
        BoundJoinKind::Cross => unreachable!("handled above"),
    };
    Ok((output, complete))
}

#[allow(clippy::too_many_arguments)]
fn next_hash_join_batch(
    left: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    right_width: usize,
    column_types: &[DataType],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    if state.grace.is_some() {
        return next_grace_join_batch(
            left,
            kind,
            left_key,
            key_mode,
            extra_keys,
            right_width,
            column_types,
            state,
            memory,
        );
    }
    let mut rows = Vec::<Vec<Value>>::with_capacity(DEFAULT_BATCH_ROWS);
    let mut buffered_bytes = 0_usize;
    while rows.len() < DEFAULT_BATCH_ROWS {
        if state.left_values.is_none()
            && !prepare_hash_join_left(left, left_key, key_mode, extra_keys, state, memory)?
        {
            break;
        }
        let left_values = state
            .left_values
            .as_ref()
            .expect("prepared join row is present");
        let matches = state.left_key.as_ref().and_then(|key| state.build.get(key));
        let (output, complete) = join_emit(
            kind,
            left_values,
            matches,
            &mut state.match_index,
            right_width,
        )?;
        let emitted = output.is_some();
        if let Some(output) = output {
            let output_bytes = estimated_row_payload_bytes(&output);
            memory.ensure_transient(
                buffered_bytes
                    .saturating_add(output_bytes)
                    .saturating_add(size_of::<Vec<Value>>()),
            )?;
            buffered_bytes = buffered_bytes
                .saturating_add(output_bytes)
                .saturating_add(size_of::<Vec<Value>>());
            rows.push(output);
        }
        if complete || !emitted {
            state.clear_left(memory);
        }
    }
    if rows.is_empty() {
        state.clear_batch(memory);
        return Ok(None);
    }
    memory.ensure_transient(
        buffered_bytes.saturating_add(estimated_record_batch_bytes(&rows, column_types.len())),
    )?;
    let columns = rows_to_columns(&rows, column_types)?;
    Ok(Some(RecordBatch::new(rows.len(), columns)?))
}

/// Serves a grace-partitioned join: routes remaining probe rows to their
/// partition files (NULL-key rows resolve immediately), then loads each
/// build partition and replays its probe file through the shared emit
/// logic.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn next_grace_join_batch(
    left: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    right_width: usize,
    column_types: &[DataType],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    let mut rows = Vec::<Vec<Value>>::with_capacity(DEFAULT_BATCH_ROWS);
    let mut buffered_bytes = 0_usize;
    let push = |rows: &mut Vec<Vec<Value>>,
                buffered_bytes: &mut usize,
                output: Vec<Value>|
     -> Result<(), ExecError> {
        let output_bytes = estimated_row_payload_bytes(&output);
        memory.ensure_transient(
            buffered_bytes
                .saturating_add(output_bytes)
                .saturating_add(size_of::<Vec<Value>>()),
        )?;
        *buffered_bytes = buffered_bytes
            .saturating_add(output_bytes)
            .saturating_add(size_of::<Vec<Value>>());
        rows.push(output);
        Ok(())
    };

    // Phase B: route the probe side to partition files.
    loop {
        let probing_done = state.grace.as_ref().is_some_and(|grace| grace.probing_done);
        if probing_done {
            break;
        }
        if rows.len() >= DEFAULT_BATCH_ROWS {
            break;
        }
        if state.left_values.is_none()
            && !prepare_hash_join_left(left, left_key, key_mode, extra_keys, state, memory)?
        {
            let grace = state.grace.as_mut().expect("grace state engaged");
            grace.probing_done = true;
            break;
        }
        let left_values = state
            .left_values
            .take()
            .expect("prepared join row is present");
        let key = state.left_key.take();
        match key {
            Some(key) => {
                let grace = state.grace.as_mut().expect("grace state engaged");
                grace.probe_files[grace_partition(&key, 0)].append(&key, &left_values)?;
            }
            None => match kind {
                // NULL keys never match: inner/semi drop the row, left
                // emits it null-extended, anti passes it through.
                BoundJoinKind::Inner | BoundJoinKind::Semi => {}
                BoundJoinKind::Left | BoundJoinKind::Scalar => {
                    let mut output = left_values.clone();
                    output.extend(std::iter::repeat_n(Value::Null, right_width));
                    push(&mut rows, &mut buffered_bytes, output)?;
                }
                BoundJoinKind::Anti => {
                    push(&mut rows, &mut buffered_bytes, left_values.clone())?;
                }
                BoundJoinKind::Cross => {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "cross semantics reached hash join",
                    ));
                }
            },
        }
        state.match_index = 0;
        memory.release(state.left_reserved);
        state.left_reserved = 0;
    }

    // Phase C: serve partitions.
    while rows.len() < DEFAULT_BATCH_ROWS {
        let grace = state.grace.as_mut().expect("grace state engaged");
        if !grace.probing_done {
            break;
        }
        if grace.replay.is_none() {
            if grace.current >= grace.build_files.len() {
                break;
            }
            let index = grace.current;
            grace.current += 1;
            // Load this partition's build rows into the resident map.
            state.build.clear();
            memory.release(grace.partition_reserved);
            grace.partition_reserved = 0;
            let used_before = memory.used();
            let mut overflowed = false;
            let mut entries = grace.build_files[index].reader()?;
            while let Some((key, values)) = entries.next_entry()? {
                if state.build.len() == state.build.capacity() {
                    let growth = state.build.capacity().max(64);
                    if reserve_hash_map_entries(
                        &mut state.build,
                        growth,
                        size_of::<JoinHashKey>()
                            .saturating_add(size_of::<Vec<Vec<Value>>>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        0,
                        memory,
                    )
                    .is_err()
                    {
                        overflowed = true;
                        break;
                    }
                }
                if memory
                    .reserve(
                        key.heap_bytes()
                            .saturating_add(estimated_row_payload_bytes(&values)),
                    )
                    .is_err()
                {
                    overflowed = true;
                    break;
                }
                state.build.entry(key).or_default().push(values);
            }
            if overflowed {
                // This partition's build side does not fit. Give back what it
                // took and split it again rather than failing the query.
                drop(entries);
                state.build.clear();
                memory.release(memory.used().saturating_sub(used_before));
                split_grace_partition(grace, index, memory)?;
                continue;
            }
            grace.partition_reserved = memory.used().saturating_sub(used_before);
            grace.replay = Some(grace.probe_files[index].reader()?);
        }
        let Some(replay) = grace.replay.as_mut() else {
            break;
        };
        let Some((key, left_values)) = replay.next_entry()? else {
            grace.replay = None;
            continue;
        };
        let matches = state.build.get(&key);
        let mut match_index = 0_usize;
        loop {
            let (output, complete) =
                join_emit(kind, &left_values, matches, &mut match_index, right_width)?;
            let emitted = output.is_some();
            if let Some(output) = output {
                push(&mut rows, &mut buffered_bytes, output)?;
            }
            // Mirrors the resident loop: an unmatched row is finished even
            // when the completion test says otherwise (inner, no matches).
            if complete || !emitted {
                break;
            }
        }
    }

    if rows.is_empty() {
        let grace = state.grace.as_mut().expect("grace state engaged");
        memory.release(grace.partition_reserved);
        grace.partition_reserved = 0;
        state.build.clear();
        state.clear_batch(memory);
        return Ok(None);
    }
    memory.ensure_transient(
        buffered_bytes.saturating_add(estimated_record_batch_bytes(&rows, column_types.len())),
    )?;
    let columns = rows_to_columns(&rows, column_types)?;
    Ok(Some(RecordBatch::new(rows.len(), columns)?))
}

fn prepare_hash_join_left(
    left: &mut PullOperator,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    loop {
        let exhausted = state
            .batch
            .as_ref()
            .is_some_and(|batch| state.row >= batch.row_count());
        if state.batch.is_none() || exhausted {
            state.clear_batch(memory);
            let Some(batch) = left.next_batch(memory)? else {
                return Ok(false);
            };
            let batch_bytes = batch.estimated_bytes();
            memory.reserve(batch_bytes)?;
            state.batch_reserved = batch_bytes;
            state.batch = Some(batch);
        }
        let batch = state.batch.as_ref().expect("left batch initialized");
        let row = state.row;
        state.row += 1;
        if !batch.selection().is_selected(row) {
            continue;
        }
        let row_bytes = estimated_batch_row_bytes(batch, row)?;
        let key_memory = left_key
            .allocation_upper_bound(batch, row)
            .saturating_mul(12);
        memory.ensure_transient(row_bytes.saturating_add(key_memory))?;
        state.left_key = match normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? {
            Some(primary) => composite_join_key(primary, batch, row, extra_keys, JoinSide::Probe)?,
            None => None,
        };
        state.left_reserved = row_bytes.saturating_sub(size_of::<Vec<Value>>());
        memory.reserve(state.left_reserved)?;
        state.left_values = Some(batch_row(batch, row)?);
        state.match_index = 0;
        return Ok(true);
    }
}

struct CompiledAggregate {
    function: AggregateFunction,
    expr: Option<CompiledExpr>,
    input_type: Option<DataType>,
    distinct: bool,
    data_type: Option<DataType>,
    /// `GROUP_CONCAT` separator (`MySQL` defaults to a comma).
    separator: String,
    /// `GROUP_CONCAT ... ORDER BY` keys as `(expr, ascending, decimal)`.
    order_within: Vec<(CompiledExpr, bool, bool)>,
}

impl CompiledAggregate {
    fn compile(aggregate: &BoundAggregate, columns: &[BoundColumn]) -> Result<Self, ExecError> {
        let input_type = aggregate
            .expr
            .as_ref()
            .and_then(|expression| expression.data_type);
        Ok(Self {
            function: aggregate.function,
            expr: aggregate
                .expr
                .as_ref()
                .map(|expression| CompiledExpr::compile(expression, columns))
                .transpose()?,
            input_type,
            distinct: aggregate.distinct,
            data_type: aggregate.data_type,
            separator: aggregate
                .separator
                .clone()
                .unwrap_or_else(|| ",".to_owned()),
            order_within: aggregate
                .order_within
                .iter()
                .map(|(expression, ascending)| {
                    Ok::<_, ExecError>((
                        CompiledExpr::compile(expression, columns)?,
                        *ascending,
                        matches!(expression.data_type, Some(DataType::Decimal { .. })),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

struct AggregateGroup {
    values: Vec<Value>,
    states: Vec<AggregateState>,
}

#[derive(Clone)]
/// DISTINCT key set. Integer-keyed values dedup through a plain i128 set
/// (no Value allocation, no enum-cell hashing — e16 measured 2.6x); the
/// first non-integer key migrates the set to normalized Values.
enum DistinctSeen {
    Ints(HashSet<i128, std::hash::BuildHasherDefault<IntKeyHasher>>),
    Values(HashSet<Value>),
}

/// splitmix-style hasher for raw integer distinct keys: `SipHash` cost is
/// pure overhead here — the keys are column data in a per-query set, not
/// a persistent attacker-fed table.
#[derive(Default)]
struct IntKeyHasher(u64);

impl std::hash::Hasher for IntKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("integer distinct keys hash through write_i128");
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn write_i128(&mut self, value: i128) {
        let low = value as u64;
        let high = (value >> 64) as u64;
        self.0 = crate::batch::mix64(low ^ high.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
}

/// splitmix-style hasher for the two-pass `(group sentinel, seen)` map keys:
/// like [`IntKeyHasher`], the keys are per-query column data, so `SipHash`'s
/// `DoS` resistance buys nothing.
#[derive(Default)]
struct GroupKeyHasher(u64);

impl std::hash::Hasher for GroupKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("two-pass group keys hash through write_u64/write_u8");
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = crate::batch::mix64(value ^ self.0);
    }

    fn write_u8(&mut self, value: u8) {
        self.0 ^= u64::from(value).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type GroupKeyMap =
    HashMap<(u64, bool), Vec<AggregateState>, std::hash::BuildHasherDefault<GroupKeyHasher>>;

fn int_distinct_key(value: &Value) -> Option<i128> {
    match value {
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        _ => None,
    }
}

fn int_key_value(key: i128) -> Value {
    i64::try_from(key).map_or_else(
        |_| Value::UInt64(u64::try_from(key).expect("distinct int keys fit u64")),
        Value::Int64,
    )
}

impl DistinctSeen {
    /// Returns whether the key is new. `false` means the caller must skip
    /// the aggregate update (already counted).
    fn insert_value(&mut self, value: &Value, memory: &MemoryTracker) -> Result<bool, ExecError> {
        if let Some(key) = int_distinct_key(value) {
            return self.insert_int(key, memory);
        }
        if let Self::Ints(_) = self {
            self.migrate_to_values(memory)?;
        }
        let Self::Values(set) = self else {
            unreachable!()
        };
        let key = normalized_hash_key(value.clone()).unwrap_or(Value::Null);
        reserve_hash_set_entries(
            set,
            1,
            size_of::<Value>().saturating_add(HASH_ENTRY_OVERHEAD),
            0,
            memory,
        )?;
        if set.contains(&key) {
            return Ok(false);
        }
        memory.reserve(key.heap_bytes())?;
        set.insert(key);
        Ok(true)
    }

    fn insert_int(&mut self, key: i128, memory: &MemoryTracker) -> Result<bool, ExecError> {
        match self {
            Self::Ints(set) => {
                reserve_hash_set_entries(
                    set,
                    1,
                    size_of::<i128>().saturating_add(HASH_ENTRY_OVERHEAD),
                    0,
                    memory,
                )?;
                Ok(set.insert(key))
            }
            Self::Values(_) => self.insert_value(&int_key_value(key), memory),
        }
    }

    fn migrate_to_values(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        if let Self::Ints(ints) = self {
            let ints = std::mem::take(ints);
            let mut set = HashSet::with_capacity(ints.len());
            memory.reserve(
                ints.len()
                    .saturating_mul(size_of::<Value>().saturating_add(HASH_ENTRY_OVERHEAD)),
            )?;
            for key in ints {
                if let Some(key) = normalized_hash_key(int_key_value(key)) {
                    set.insert(key);
                }
            }
            *self = Self::Values(set);
        }
        Ok(())
    }

    fn drain_values(self) -> Vec<Value> {
        match self {
            Self::Ints(set) => set.into_iter().map(int_key_value).collect(),
            Self::Values(set) => set.into_iter().collect(),
        }
    }
}

#[derive(Clone)]
struct AggregateState {
    value: AggregateValue,
    seen: Option<DistinctSeen>,
    /// f64 of the current Minimum/Maximum extreme when known (typed path).
    /// Guides comparisons: strict f64 inequality between correctly-rounded
    /// values transfers to the exact ordering (rounding is monotone), so only
    /// f64 ties pay the full text/value comparison. Invalidated on merge.
    extreme_number: Option<f64>,
    /// Scaled integer units of the current extreme when every update so far
    /// arrived through `update_extreme_units` (same column, same scale, so
    /// unit ordering IS the value ordering). Invalidated on merge.
    extreme_units: Option<i128>,
}

#[derive(Clone)]
enum AggregateValue {
    Count(u64),
    Sum(Option<Value>),
    /// Exact decimal SUM carried as scaled integer units — accumulating
    /// i128 units replaces a parse-add-format round trip per row on the
    /// canonical text carrier (2026-08-02 phase-0 profile residue).
    DecimalSum {
        units: i128,
        scale: u8,
        /// Emit Float64 at finish (the bound aggregate type): the exact
        /// total converts with ONE correct rounding, unlike per-row f64
        /// accumulation (the Q4 canonical mismatch, 2026-08-02).
        float_output: bool,
    },
    Average {
        sum: f64,
        count: u64,
    },
    /// Exact `MySQL` AVG over exact-numeric inputs: the running total is
    /// carried as integer units already widened to the RESULT scale
    /// (input scale + `div_precision_increment`), so `finish` is a single
    /// half-away-from-zero division by the row count.
    DecimalAverage {
        units: i128,
        scale: u8,
        count: u64,
    },
    Minimum(Option<Value>),
    Maximum(Option<Value>),
    /// `ANY_VALUE`: the first non-NULL value the group sees. `MySQL` does not
    /// define which row wins; clients emit this only for columns they know
    /// are functionally dependent on the grouping key.
    AnyValue(Option<Value>),
    /// Welford moments for `STDDEV`/`VARIANCE`. Summing squares and
    /// subtracting the mean at the end is one operation cheaper per row and
    /// catastrophically worse: on values far from zero the subtraction
    /// cancels away most of the significand. Welford costs an extra subtract
    /// and keeps full precision, which matters when the result lands in a
    /// dashboard beside the same column read from `MySQL`.
    Moments {
        count: u64,
        mean: f64,
        m2: f64,
        sample: bool,
        stddev: bool,
    },
    /// `JSON_OBJECTAGG`: each row contributes a one-member object, merged
    /// left to right. A repeated key keeps the last value, as `MySQL` does.
    JsonObjectAgg {
        members: serde_json::Map<String, serde_json::Value>,
    },
    /// `BIT_AND`/`BIT_OR`/`BIT_XOR`. `seen` exists because `MySQL` folds an
    /// empty group to the operation's identity rather than to NULL, so
    /// `BIT_AND` over no rows is all ones.
    BitFold {
        accumulator: u64,
        seen: bool,
    },
    GroupConcat {
        /// Collected `(order keys, rendered value)` rows.
        items: Vec<(Vec<Value>, String)>,
        /// Join separator resolved at state creation.
        separator: String,
        /// Per-key `(ascending, decimal)` sort spec.
        order: Vec<(bool, bool)>,
    },
    JsonArrayAgg {
        /// Pre-rendered JSON fragments in input order (NULLs included).
        items: Vec<String>,
    },
}

impl AggregateState {
    fn new(aggregate: &CompiledAggregate) -> Self {
        let value = match aggregate.function {
            AggregateFunction::Count => AggregateValue::Count(0),
            AggregateFunction::Sum => AggregateValue::Sum(None),
            AggregateFunction::Average => match decimal_average_scale(aggregate) {
                Some(scale) => AggregateValue::DecimalAverage {
                    units: 0,
                    scale,
                    count: 0,
                },
                None => AggregateValue::Average { sum: 0.0, count: 0 },
            },
            AggregateFunction::Minimum => AggregateValue::Minimum(None),
            AggregateFunction::Maximum => AggregateValue::Maximum(None),
            AggregateFunction::GroupConcat => AggregateValue::GroupConcat {
                items: Vec::new(),
                separator: aggregate.separator.clone(),
                order: aggregate
                    .order_within
                    .iter()
                    .map(|(_, ascending, decimal)| (*ascending, *decimal))
                    .collect(),
            },
            AggregateFunction::JsonArrayAgg => AggregateValue::JsonArrayAgg { items: Vec::new() },
            AggregateFunction::JsonObjectAgg => AggregateValue::JsonObjectAgg {
                members: serde_json::Map::new(),
            },
            AggregateFunction::AnyValue => AggregateValue::AnyValue(None),
            AggregateFunction::StdDev { sample } => AggregateValue::Moments {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                sample,
                stddev: true,
            },
            AggregateFunction::Variance { sample } => AggregateValue::Moments {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                sample,
                stddev: false,
            },
            AggregateFunction::BitAnd => AggregateValue::BitFold {
                accumulator: u64::MAX,
                seen: false,
            },
            AggregateFunction::BitOr | AggregateFunction::BitXor => AggregateValue::BitFold {
                accumulator: 0,
                seen: false,
            },
        };
        Self {
            value,
            seen: aggregate
                .distinct
                .then(|| DistinctSeen::Ints(HashSet::default())),
            extreme_number: None,
            extreme_units: None,
        }
    }

    fn update(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        self.update_with_number(aggregate, value, None, memory)
    }

    #[allow(clippy::too_many_lines)]
    fn update_with_number(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        number: Option<f64>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        // JSON_ARRAYAGG collects NULLs as JSON nulls, so it intercepts
        // ahead of the NULL skip every other aggregate relies on.
        // Like JSON_ARRAYAGG, this runs ahead of the NULL skip: MySQL keeps a
        // NULL value in the object rather than dropping the member.
        if let AggregateValue::JsonObjectAgg { members } = &mut self.value {
            // The bound expression already rendered JSON_OBJECT(k, v), so
            // this text IS the object. Running it back through
            // json_value_of would wrap it as a JSON *string*, the object
            // parse would fail, and every group answered {}.
            let fragment = match value {
                Value::Utf8(text) => text.clone(),
                other => {
                    crate::expression::mysql_json_text(&crate::expression::json_value_of(other))
                }
            };
            if let Ok(serde_json::Value::Object(pair)) =
                serde_json::from_str::<serde_json::Value>(&fragment)
            {
                for (key, entry) in pair {
                    memory.reserve(key.len().saturating_add(16))?;
                    members.insert(key, entry);
                }
            }
            return Ok(());
        }
        if let AggregateValue::JsonArrayAgg { items } = &mut self.value {
            let fragment = crate::expression::mysql_json_text(
                &crate::expression::json_value_of_typed(value, aggregate.input_type)?,
            );
            reserve_vec_elements(items, 1, 64, memory)?;
            memory.reserve(fragment.len())?;
            items.push(fragment);
            return Ok(());
        }
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert_value(value, memory)?
        {
            return Ok(());
        }
        // Decimal-typed SUM accumulates scaled units exactly: morph into the
        // unit state on the first value instead of parsing and reformatting
        // canonical text per row.
        if aggregate.function == AggregateFunction::Sum
            && let Some(DataType::Decimal { scale, .. }) = aggregate.data_type
        {
            let units = match value {
                Value::Utf8(text) => crate::batch::parse_decimal_scaled(text, scale),
                Value::Boolean(flag) => decimal_units_from_int(i128::from(*flag), scale),
                Value::Int64(signed) => decimal_units_from_int(i128::from(*signed), scale),
                Value::UInt64(unsigned) => decimal_units_from_int(i128::from(*unsigned), scale),
                _ => None,
            }
            .ok_or(ExecError::NumericOverflow)?;
            return self.update_decimal_sum_units(units, scale, false);
        }
        match &mut self.value {
            // Handled by the early return above, before the NULL skip.
            AggregateValue::JsonObjectAgg { .. } => {}
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::AnyValue(slot) => {
                if slot.is_none() {
                    replace_retained_value(slot, value.clone(), memory)?;
                }
            }
            AggregateValue::Moments {
                count, mean, m2, ..
            } => {
                let observation = number.map_or_else(|| mysql_f64(value), Ok)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                #[allow(clippy::cast_precision_loss)]
                let n = *count as f64;
                let delta = observation - *mean;
                *mean += delta / n;
                *m2 = delta.mul_add(observation - *mean, *m2);
                // Values near the f64 ceiling drive delta to infinity and m2
                // to -infinity, which would surface as a negative variance
                // or a NaN standard deviation. Average and Sum already
                // reject non-finite intermediates; so does this.
                if !mean.is_finite() || !m2.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
            }
            AggregateValue::BitFold { accumulator, seen } => {
                // MySQL coerces the argument to BIGINT UNSIGNED before
                // folding, so a signed or textual input reinterprets rather
                // than erroring.
                let bits = mysql_u64(value).unwrap_or_else(|_| {
                    mysql_i64(value).map_or(0, |signed| u64::from_ne_bytes(signed.to_ne_bytes()))
                });
                *accumulator = match aggregate.function {
                    AggregateFunction::BitAnd => *accumulator & bits,
                    AggregateFunction::BitXor => *accumulator ^ bits,
                    _ => *accumulator | bits,
                };
                *seen = true;
            }
            AggregateValue::DecimalSum { units, scale, .. } => {
                let text = match value {
                    Value::Utf8(text) => text.as_str(),
                    _ => {
                        return Err(ExecError::InvalidPhysicalPlan(
                            "decimal sum updated with a non-text value",
                        ));
                    }
                };
                let scaled = crate::batch::parse_decimal_scaled(text, *scale)
                    .ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Sum(sum) => {
                *sum = Some(if let Some(number) = number {
                    let result = sum.take().map_or(Ok(0.0), |value| mysql_f64(&value))? + number;
                    if !result.is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                    Value::float64(result)
                } else {
                    add_aggregate_value(sum.take(), value, aggregate.data_type)?
                });
            }
            AggregateValue::Average { sum, count } => {
                *sum += number.map_or_else(|| mysql_f64(value), Ok)?;
                if !sum.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::DecimalAverage {
                units,
                scale,
                count,
            } => {
                // Typed lanes deliver the row through `number` with a
                // sentinel value; everything else arrives as the real Value.
                let scaled = if let Some(number) = number {
                    exact_decimal_units_from_f64(number, *scale)
                } else {
                    match value {
                        Value::Utf8(text) => crate::batch::parse_decimal_scaled(text, *scale),
                        Value::Boolean(flag) => decimal_units_from_int(i128::from(*flag), *scale),
                        Value::Int64(signed) => decimal_units_from_int(i128::from(*signed), *scale),
                        Value::UInt64(unsigned) => {
                            decimal_units_from_int(i128::from(*unsigned), *scale)
                        }
                        _ => None,
                    }
                };
                let scaled = scaled.ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Minimum(minimum) => {
                let replace = match minimum.as_ref() {
                    Some(current) => match (number, self.extreme_number) {
                        (Some(candidate), Some(extreme)) if candidate < extreme => true,
                        (Some(candidate), Some(extreme)) if candidate > extreme => false,
                        _ => {
                            compare_aggregate_values(value, current, aggregate.data_type)?
                                == Ordering::Less
                        }
                    },
                    None => true,
                };
                if replace {
                    replace_retained_value(minimum, value.clone(), memory)?;
                    self.extreme_number = number;
                }
            }
            AggregateValue::Maximum(maximum) => {
                let replace = match maximum.as_ref() {
                    Some(current) => match (number, self.extreme_number) {
                        (Some(candidate), Some(extreme)) if candidate > extreme => true,
                        (Some(candidate), Some(extreme)) if candidate < extreme => false,
                        _ => {
                            compare_aggregate_values(value, current, aggregate.data_type)?
                                == Ordering::Greater
                        }
                    },
                    None => true,
                };
                if replace {
                    replace_retained_value(maximum, value.clone(), memory)?;
                    self.extreme_number = number;
                }
            }
            AggregateValue::GroupConcat { items, .. } => {
                let value_bytes = scalar_string_memory_upper_bound(value);
                reserve_vec_elements(items, 1, 64, memory)?;
                memory.reserve(value_bytes)?;
                let value = aggregate_string(value)?;
                items.push((Vec::new(), value));
            }
            // Handled by the intercept above the NULL skip.
            AggregateValue::JsonArrayAgg { .. } => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "json-array-agg update bypassed its intercept",
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn merge(
        &mut self,
        aggregate: &CompiledAggregate,
        mut other: Self,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        // Merging may replace the extreme through the Value path; the cached
        // f64 guide is conservative-invalidated rather than tracked.
        self.extreme_number = None;
        if aggregate.distinct {
            if let Some(seen) = other.seen.take() {
                for value in seen.drain_values() {
                    self.update(aggregate, &value, memory)?;
                }
            }
            return Ok(());
        }
        match (&mut self.value, other.value) {
            (AggregateValue::Count(left), AggregateValue::Count(right)) => {
                *left = left.checked_add(right).ok_or(ExecError::NumericOverflow)?;
            }
            (AggregateValue::Sum(left), AggregateValue::Sum(Some(right))) => {
                *left = Some(add_aggregate_value(
                    left.take(),
                    &right,
                    aggregate.data_type,
                )?);
            }
            (
                AggregateValue::Sum(_) | AggregateValue::DecimalSum { .. },
                AggregateValue::Sum(None),
            )
            | (AggregateValue::Minimum(_), AggregateValue::Minimum(None))
            | (AggregateValue::Maximum(_), AggregateValue::Maximum(None)) => {}
            (
                AggregateValue::DecimalSum { units: left, .. },
                AggregateValue::DecimalSum { units: right, .. },
            ) => {
                *left = left.checked_add(right).ok_or(ExecError::NumericOverflow)?;
            }
            (
                value @ AggregateValue::Sum(None),
                AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                },
            ) => {
                *value = AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                };
            }
            (AggregateValue::DecimalSum { units, scale, .. }, AggregateValue::Sum(Some(right))) => {
                let scaled = crate::batch::parse_decimal_scaled(
                    match &right {
                        Value::Utf8(text) => text,
                        _ => {
                            return Err(ExecError::InvalidPhysicalPlan(
                                "decimal sum merged with a non-text sum",
                            ));
                        }
                    },
                    *scale,
                )
                .ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (
                AggregateValue::DecimalAverage {
                    units: left_units,
                    scale: left_scale,
                    count: left_count,
                },
                AggregateValue::DecimalAverage {
                    units: right_units,
                    scale: right_scale,
                    count: right_count,
                },
            ) => {
                if left_scale != &right_scale {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "decimal average merged across scales",
                    ));
                }
                *left_units = left_units
                    .checked_add(right_units)
                    .ok_or(ExecError::NumericOverflow)?;
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (
                AggregateValue::Average {
                    sum: left_sum,
                    count: left_count,
                },
                AggregateValue::Average {
                    sum: right_sum,
                    count: right_count,
                },
            ) => {
                *left_sum += right_sum;
                if !left_sum.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (AggregateValue::Minimum(left), AggregateValue::Minimum(Some(right))) => {
                let replace = match left.as_ref() {
                    Some(current) => {
                        compare_aggregate_values(&right, current, aggregate.data_type)?
                            == Ordering::Less
                    }
                    None => true,
                };
                if replace {
                    replace_retained_value(left, right, memory)?;
                }
            }
            (AggregateValue::Maximum(left), AggregateValue::Maximum(Some(right))) => {
                let replace = match left.as_ref() {
                    Some(current) => {
                        compare_aggregate_values(&right, current, aggregate.data_type)?
                            == Ordering::Greater
                    }
                    None => true,
                };
                if replace {
                    replace_retained_value(left, right, memory)?;
                }
            }
            (AggregateValue::AnyValue(left), AggregateValue::AnyValue(right)) => {
                // Any one value satisfies ANY_VALUE, so an existing one wins
                // and an empty side takes the other's.
                if left.is_none()
                    && let Some(value) = right
                {
                    replace_retained_value(left, value, memory)?;
                }
            }
            (
                AggregateValue::BitFold { accumulator, seen },
                AggregateValue::BitFold {
                    accumulator: other,
                    seen: other_seen,
                },
            ) => {
                // The identity of an unseen side would corrupt the fold —
                // all-ones for BIT_AND — so an empty side is skipped rather
                // than combined.
                if other_seen {
                    if *seen {
                        *accumulator = match aggregate.function {
                            AggregateFunction::BitAnd => *accumulator & other,
                            AggregateFunction::BitXor => *accumulator ^ other,
                            _ => *accumulator | other,
                        };
                    } else {
                        *accumulator = other;
                        *seen = true;
                    }
                }
            }
            (
                AggregateValue::Moments {
                    count, mean, m2, ..
                },
                AggregateValue::Moments {
                    count: other_count,
                    mean: other_mean,
                    m2: other_m2,
                    ..
                },
            ) => {
                // Chan's parallel form. Adding m2 directly would be wrong:
                // the combined spread includes the distance between the two
                // means, which neither partial state carries.
                if other_count > 0 {
                    let total = count
                        .checked_add(other_count)
                        .ok_or(ExecError::NumericOverflow)?;
                    #[allow(clippy::cast_precision_loss)]
                    let (left_n, right_n, total_n) =
                        (*count as f64, other_count as f64, total as f64);
                    let delta = other_mean - *mean;
                    *mean = (left_n * *mean + right_n * other_mean) / total_n;
                    *m2 += other_m2 + delta * delta * left_n * right_n / total_n;
                    *count = total;
                    if !mean.is_finite() || !m2.is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                }
            }
            _ => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "aggregate states have incompatible merge shapes",
                ));
            }
        }
        Ok(())
    }

    /// Exact decimal SUM on scaled integer units: no text parse, no text
    /// format until `finish`. The state lazily morphs from `Sum(None)` on
    /// the first unit-borne update.
    fn update_decimal_sum_units(
        &mut self,
        units: i128,
        scale: u8,
        float_output: bool,
    ) -> Result<(), ExecError> {
        match &mut self.value {
            AggregateValue::DecimalSum {
                units: total,
                scale: existing,
                ..
            } if *existing == scale => {
                *total = total.checked_add(units).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            value @ AggregateValue::Sum(None) => {
                *value = AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                };
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "decimal unit sum applied to an incompatible aggregate state",
            )),
        }
    }

    /// `GROUP_CONCAT` update carrying the aggregate-local ORDER BY keys
    /// evaluated for this row.
    fn update_group_concat(
        &mut self,
        value: &Value,
        keys: Vec<Value>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert_value(value, memory)?
        {
            return Ok(());
        }
        let AggregateValue::GroupConcat { items, .. } = &mut self.value else {
            return Err(ExecError::InvalidPhysicalPlan(
                "group-concat update applied to an incompatible aggregate state",
            ));
        };
        let key_bytes = keys.iter().map(Value::heap_bytes).sum::<usize>();
        reserve_vec_elements(items, 1, 64, memory)?;
        memory.reserve(scalar_string_memory_upper_bound(value).saturating_add(key_bytes))?;
        items.push((keys, aggregate_string(value)?));
        Ok(())
    }

    /// Exact decimal AVG on scaled integer units already widened to the
    /// result scale.
    fn update_decimal_average_units(&mut self, units: i128, scale: u8) -> Result<(), ExecError> {
        match &mut self.value {
            AggregateValue::DecimalAverage {
                units: total,
                scale: existing,
                count,
            } if *existing == scale => {
                *total = total.checked_add(units).ok_or(ExecError::NumericOverflow)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "decimal unit average applied to an incompatible aggregate state",
            )),
        }
    }

    /// COUNT(DISTINCT) on a raw integer key: dedup in the i128 set and
    /// bump the count only for new keys — no Value cell is built.
    fn update_distinct_count_int(
        &mut self,
        key: i128,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let Some(seen) = &mut self.seen else {
            return Err(ExecError::InvalidPhysicalPlan(
                "distinct update on a non-distinct aggregate state",
            ));
        };
        if !seen.insert_int(key, memory)? {
            return Ok(());
        }
        match &mut self.value {
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "distinct int count applied to a non-count aggregate",
            )),
        }
    }

    /// MIN/MAX on packed units: comparisons run on the integer units and
    /// the canonical text is formatted only when the extreme is replaced.
    /// A retained extreme without known units (state arrived via merge or a
    /// mixed path) pays one text comparison and then re-anchors the units.
    fn update_extreme_units(
        &mut self,
        aggregate: &CompiledAggregate,
        units: i128,
        format: impl Fn() -> Option<String>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let keep_less = match self.value {
            AggregateValue::Minimum(_) => true,
            AggregateValue::Maximum(_) => false,
            _ => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "unit extreme applied to a non-extreme aggregate state",
                ));
            }
        };
        let current_retained = match &self.value {
            AggregateValue::Minimum(slot) | AggregateValue::Maximum(slot) => slot.as_ref(),
            _ => unreachable!(),
        };
        let (replace, preformatted) = match (self.extreme_units, current_retained) {
            (_, None) => (true, None),
            (Some(current), Some(_)) => (
                if keep_less {
                    units < current
                } else {
                    units > current
                },
                None,
            ),
            (None, Some(current)) => {
                let candidate = Value::Utf8(format().ok_or(ExecError::NumericOverflow)?);
                let ordering = compare_aggregate_values(&candidate, current, aggregate.data_type)?;
                let replace = if keep_less {
                    ordering == Ordering::Less
                } else {
                    ordering == Ordering::Greater
                };
                (replace, Some(candidate))
            }
        };
        if replace {
            let value = match preformatted {
                Some(value) => value,
                None => Value::Utf8(format().ok_or(ExecError::NumericOverflow)?),
            };
            let (AggregateValue::Minimum(slot) | AggregateValue::Maximum(slot)) = &mut self.value
            else {
                unreachable!()
            };
            replace_retained_value(slot, value, memory)?;
            self.extreme_units = Some(units);
        }
        Ok(())
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn finish(self, memory: &MemoryTracker) -> Result<Value, ExecError> {
        Ok(match self.value {
            AggregateValue::Count(count) => Value::UInt64(count),
            AggregateValue::JsonObjectAgg { members } => Value::Utf8(
                crate::expression::mysql_json_text(&serde_json::Value::Object(members)),
            ),
            AggregateValue::BitFold { accumulator, .. } => Value::UInt64(accumulator),
            AggregateValue::Moments {
                count,
                m2,
                sample,
                stddev,
                ..
            } => {
                // MySQL returns NULL for an empty or all-NULL group, and for
                // the sample forms at n = 1 the divisor is zero, which is
                // also NULL rather than an error. The population forms report
                // 0 for a single row.
                let divisor = if sample {
                    count.saturating_sub(1)
                } else {
                    count
                };
                if count == 0 || divisor == 0 {
                    Value::Null
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let variance = m2 / divisor as f64;
                    Value::float64(if stddev { variance.sqrt() } else { variance })
                }
            }
            AggregateValue::DecimalSum {
                units,
                scale,
                float_output,
            } => {
                if float_output {
                    #[allow(clippy::cast_precision_loss)]
                    Value::float64(units as f64 / 10_f64.powi(i32::from(scale)))
                } else {
                    Value::Utf8(pintail_types::format_decimal_scaled(units, scale))
                }
            }
            AggregateValue::Sum(value)
            | AggregateValue::Minimum(value)
            | AggregateValue::Maximum(value)
            | AggregateValue::AnyValue(value) => value.unwrap_or(Value::Null),
            AggregateValue::Average { sum: _, count: 0 }
            | AggregateValue::DecimalAverage { count: 0, .. } => Value::Null,
            AggregateValue::Average { sum, count } => Value::float64(sum / count as f64),
            AggregateValue::DecimalAverage {
                units,
                scale,
                count,
            } => {
                let average = pintail_types::div_decimal_round_half_up(units, i128::from(count))
                    .ok_or(ExecError::NumericOverflow)?;
                Value::Utf8(pintail_types::format_decimal_scaled(average, scale))
            }
            AggregateValue::JsonArrayAgg { items } if items.is_empty() => Value::Null,
            AggregateValue::JsonArrayAgg { items } => {
                let joined_bytes = items.iter().map(String::len).fold(
                    items.len().saturating_mul(2).saturating_add(2),
                    usize::saturating_add,
                );
                memory.reserve(joined_bytes)?;
                Value::Utf8(format!("[{}]", items.join(", ")))
            }
            AggregateValue::GroupConcat { items, .. } if items.is_empty() => Value::Null,
            AggregateValue::GroupConcat {
                mut items,
                separator,
                order,
            } => {
                if !order.is_empty() {
                    items.sort_by(|left, right| {
                        for (position, (ascending, decimal)) in order.iter().enumerate() {
                            let ordering = compare_sort_values(
                                left.0.get(position).unwrap_or(&Value::Null),
                                right.0.get(position).unwrap_or(&Value::Null),
                                BoundOrderKey {
                                    index: 0,
                                    ascending: *ascending,
                                    // MySQL sorts NULL keys first ascending.
                                    nulls_first: *ascending,
                                    decimal: *decimal,
                                },
                            );
                            if ordering != Ordering::Equal {
                                return ordering;
                            }
                        }
                        Ordering::Equal
                    });
                }
                let joined_bytes = items.iter().map(|(_, text)| text.len()).fold(
                    items
                        .len()
                        .saturating_sub(1)
                        .saturating_mul(separator.len()),
                    usize::saturating_add,
                );
                memory.reserve(joined_bytes)?;
                let mut joined = items
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(&separator);
                // MySQL truncates at the session's byte ceiling and raises
                // warning 1260 for each group that was cut.
                let limit = SESSION_GROUP_CONCAT_MAX_LEN.get();
                if joined.len() > limit {
                    let mut cut = limit;
                    while !joined.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    joined.truncate(cut);
                    SESSION_GROUP_CONCAT_WARNINGS
                        .set(SESSION_GROUP_CONCAT_WARNINGS.get().saturating_add(1));
                }
                Value::Utf8(joined)
            }
        })
    }
}

fn replace_retained_value(
    current: &mut Option<Value>,
    replacement: Value,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let current_bytes = current.as_ref().map_or(0, Value::heap_bytes);
    let replacement_bytes = replacement.heap_bytes();
    if replacement_bytes > current_bytes {
        memory.reserve(replacement_bytes - current_bytes)?;
    } else {
        memory.release(current_bytes - replacement_bytes);
    }
    *current = Some(replacement);
    Ok(())
}

#[allow(clippy::too_many_lines)]
/// Settled aggregate memo (e18's product lever, exactness-first form):
/// bare full-table aggregates over a settled snapshot are pure functions
/// of `(table, manifest generation, plan signature)`. The memo stores the
/// engine's own exact result and is unreachable the moment any ingest
/// makes the snapshot unsettled, so served rows are provably fresh —
/// unlike TTL query caches. Persistent per-block SMAs remain follow-up.
type SettledMemoKey = (std::path::PathBuf, u64, String);
type SettledMemo = std::sync::Mutex<HashMap<SettledMemoKey, Vec<Vec<Value>>>>;
static SETTLED_AGGREGATE_MEMO: std::sync::LazyLock<SettledMemo> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

const SETTLED_MEMO_MAX_ENTRIES: usize = 32;
const SETTLED_MEMO_MAX_ROWS: usize = 1 << 17;

/// Deterministic plan signature, or `None` when any expression could
/// evaluate differently on identical data (volatile functions).
fn settled_signature(
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
) -> Option<String> {
    use std::fmt::Write;
    let mut signature = String::new();
    for expr in group_by {
        write!(signature, "g:{};", expr.deterministic_signature()?).ok()?;
    }
    for aggregate in aggregates {
        let expr = match &aggregate.expr {
            Some(expr) => expr.deterministic_signature()?,
            None => "*".to_owned(),
        };
        write!(
            signature,
            "a:{:?}:{}:{};",
            aggregate.function, aggregate.distinct, expr
        )
        .ok()?;
    }
    Some(signature)
}

#[allow(clippy::too_many_lines)]
/// Merges finished aggregate values of a memoized result with a freshly
/// aggregated insert-only delta, group by group. Only called for shapes
/// whose finished values merge exactly (COUNT/int-float SUM/MIN/MAX).
fn merge_finished_aggregate_rows(
    mut base: Vec<Vec<Value>>,
    delta: Vec<Vec<Value>>,
    group_len: usize,
    aggregates: &[CompiledAggregate],
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut index = HashMap::<Vec<Value>, usize>::new();
    for (position, row) in base.iter().enumerate() {
        let key = row[..group_len]
            .iter()
            .cloned()
            .map(normalized_collation_value)
            .collect::<Vec<_>>();
        index.insert(key, position);
    }
    for row in delta {
        let key = row[..group_len]
            .iter()
            .cloned()
            .map(normalized_collation_value)
            .collect::<Vec<_>>();
        if let Some(position) = index.get(&key) {
            for (offset, aggregate) in aggregates.iter().enumerate() {
                let column = group_len + offset;
                let current = std::mem::replace(&mut base[*position][column], Value::Null);
                base[*position][column] = merge_finished_value(aggregate, current, &row[column])?;
            }
        } else {
            index.insert(key, base.len());
            base.push(row);
        }
    }
    Ok(base)
}

fn merge_finished_value(
    aggregate: &CompiledAggregate,
    current: Value,
    delta: &Value,
) -> Result<Value, ExecError> {
    if matches!(delta, Value::Null) {
        return Ok(current);
    }
    if matches!(current, Value::Null) {
        return Ok(delta.clone());
    }
    match aggregate.function {
        // ANY_VALUE is satisfied by whichever side already has a value, and
        // the bit folds are associative, so both merge exactly.
        AggregateFunction::AnyValue => Ok(current),
        AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor => {
            let left = mysql_u64(&current).unwrap_or(0);
            let right = mysql_u64(delta).unwrap_or(0);
            Ok(Value::UInt64(match aggregate.function {
                AggregateFunction::BitAnd => left & right,
                AggregateFunction::BitXor => left ^ right,
                _ => left | right,
            }))
        }
        // Two finished standard deviations cannot be combined — the moments
        // they came from are gone. The eligibility gate keeps these off this
        // path; this arm exists so a future gate change fails loudly rather
        // than inventing a number.
        AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. } => Err(
            ExecError::InvalidPhysicalPlan("STDDEV/VARIANCE finished values cannot be merged"),
        ),
        // Merging rendered objects would need key-level precedence the
        // finished text no longer carries; the eligibility gate keeps this
        // unreachable.
        AggregateFunction::JsonObjectAgg => Err(ExecError::InvalidPhysicalPlan(
            "JSON_OBJECTAGG finished values cannot be merged",
        )),
        AggregateFunction::Count => {
            add_aggregate_value(Some(current), delta, Some(DataType::UInt64))
        }
        AggregateFunction::Sum => add_aggregate_value(Some(current), delta, aggregate.data_type),
        AggregateFunction::Minimum => Ok(
            if compare_aggregate_values(delta, &current, aggregate.data_type)? == Ordering::Less {
                delta.clone()
            } else {
                current
            },
        ),
        AggregateFunction::Maximum => Ok(
            if compare_aggregate_values(delta, &current, aggregate.data_type)? == Ordering::Greater
            {
                delta.clone()
            } else {
                current
            },
        ),
        AggregateFunction::Average
        | AggregateFunction::GroupConcat
        | AggregateFunction::JsonArrayAgg => Err(ExecError::InvalidPhysicalPlan(
            "unmergeable aggregate reached the delta merge",
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn build_hash_aggregate(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    // Storage predicates are ALSO compiled into Filter operators above the
    // scan (belt and braces), so a filtered plan is Filter(..(Scan)). Those
    // filters are exactly scan.predicates, which the scan signature already
    // covers — walking through them keeps the key sound.
    fn settled_scan(operator: &PullOperator) -> Option<&PullOperator> {
        match operator {
            PullOperator::Scan { .. } => Some(operator),
            PullOperator::Filter { input, .. } => settled_scan(input),
            _ => None,
        }
    }
    /// Data-version identity of a whole settled plan: scans directly,
    /// filters transparently (their predicates ARE the scan signature),
    /// and fresh inner joins when BOTH sides are settled — either table's
    /// ingest or flush changes its component of the key.
    fn settled_plan_key(operator: &PullOperator) -> Option<(std::path::PathBuf, u64, String)> {
        match operator {
            PullOperator::Scan { stream, .. } => stream.settled_identity(),
            PullOperator::Filter { input, .. } => settled_plan_key(input),
            PullOperator::HashJoin {
                left,
                right,
                kind,
                left_key,
                right_key,
                key_mode,
                right_width,
                state,
                ..
            } if state.is_none() => {
                let (left_dir, left_gen, left_sig) = settled_plan_key(left)?;
                let (right_dir, right_gen, right_sig) = settled_plan_key(right)?;
                Some((
                    left_dir,
                    left_gen,
                    format!(
                        "J{kind:?}|{key_mode:?}|{}|{}|{right_width}|L({left_sig})|R({}:{right_gen}:{right_sig})",
                        left_key.deterministic_signature()?,
                        right_key.deterministic_signature()?,
                        right_dir.display(),
                    ),
                ))
            }
            _ => None,
        }
    }
    // Profiling escape hatch: with the memo on, every settled re-run is a
    // replay and a sampling profiler only ever sees the first execution.
    let memo_key = if std::env::var_os("PINTAIL_DISABLE_SETTLED_MEMO").is_some() {
        None
    } else {
        settled_plan_key(input).and_then(|(directory, generation, scan)| {
            settled_signature(group_by, aggregates)
                .map(|signature| (directory, generation, format!("p{scan:?};{signature}")))
        })
    };
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        eprintln!(
            "[agg] memo key: {:?} (scan found: {})",
            memo_key.as_ref().map(|(_, generation, signature)| (
                generation,
                signature.chars().take(60).collect::<String>()
            )),
            settled_scan(input).is_some()
        );
    }
    if let Some(key) = &memo_key
        && let Some(rows) = SETTLED_AGGREGATE_MEMO
            .lock()
            .expect("settled memo lock")
            .get(key)
            .cloned()
    {
        let payload: usize = rows
            .iter()
            .map(|row| estimated_row_payload_bytes(row))
            .sum();
        memory.reserve(payload)?;
        return Ok(MaterializedRows { rows, position: 0 });
    }
    if memo_key.is_none()
        && let Some(PullOperator::Scan { stream, .. }) = settled_scan(input)
        && let Some(delta) = stream.insert_only_delta()
        && let Some(signature) = settled_signature(group_by, aggregates)
        && aggregates.iter().all(|aggregate| {
            !aggregate.distinct
                && match aggregate.function {
                    // COUNT/MIN/MAX, plus the associative folds, all merge
                    // exactly over the disjoint rows an insert-only delta
                    // contributes.
                    AggregateFunction::Count
                    | AggregateFunction::Minimum
                    | AggregateFunction::Maximum
                    | AggregateFunction::AnyValue
                    | AggregateFunction::BitAnd
                    | AggregateFunction::BitOr
                    | AggregateFunction::BitXor => true,
                    AggregateFunction::Sum => matches!(
                        aggregate.data_type,
                        Some(DataType::Int64 | DataType::UInt64 | DataType::Float64)
                    ),
                    AggregateFunction::Average
                    | AggregateFunction::GroupConcat
                    | AggregateFunction::JsonArrayAgg
                    | AggregateFunction::JsonObjectAgg
                    // Needs the moments, not the finished value.
                    | AggregateFunction::StdDev { .. }
                    | AggregateFunction::Variance { .. } => false,
                }
        })
    {
        let key = (
            delta.directory.clone(),
            delta.generation,
            format!("{};{signature}", delta.scan),
        );
        let base = SETTLED_AGGREGATE_MEMO
            .lock()
            .expect("settled memo lock")
            .get(&key)
            .cloned();
        if let Some(base) = base {
            let row_count = delta.rows.len();
            let columns = (0..delta.types.len())
                .map(|column| {
                    ColumnVector::new(
                        delta.types[column],
                        delta.rows.iter().map(|row| row[column].clone()).collect(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ExecError::InvalidBatch("delta rows do not match the scan types"))?;
            let batch = RecordBatch::new(row_count, columns)
                .map_err(|_| ExecError::InvalidBatch("delta rows do not form a batch"))?;
            let mut one_shot = PullOperator::Scan {
                stream: Box::new(OneShotStream { batch: Some(batch) }),
                expected_types: delta.types.clone(),
            };
            let delta_rows =
                build_hash_aggregate_scan(&mut one_shot, group_by, aggregates, memory)?;
            let merged =
                merge_finished_aggregate_rows(base, delta_rows.rows, group_by.len(), aggregates)?;
            let payload: usize = merged
                .iter()
                .map(|row| estimated_row_payload_bytes(row))
                .sum();
            memory.reserve(payload)?;
            return Ok(MaterializedRows {
                rows: merged,
                position: 0,
            });
        }
    }
    if group_by.is_empty()
        && !aggregates.is_empty()
        && let Some(rows) = try_sma_fold(input, aggregates, memory)?
    {
        if let Some(key) = &memo_key {
            let mut memo = SETTLED_AGGREGATE_MEMO.lock().expect("settled memo lock");
            if memo.len() >= SETTLED_MEMO_MAX_ENTRIES {
                memo.clear();
            }
            memo.insert(key.clone(), rows.clone());
        }
        return Ok(MaterializedRows { rows, position: 0 });
    }
    let result = build_hash_aggregate_scan(input, group_by, aggregates, memory)?;
    if let Some(key) = memo_key
        && result.rows.len() <= SETTLED_MEMO_MAX_ROWS
    {
        let mut memo = SETTLED_AGGREGATE_MEMO.lock().expect("settled memo lock");
        if memo.len() >= SETTLED_MEMO_MAX_ENTRIES {
            memo.clear();
        }
        memo.insert(key, result.rows.clone());
    }
    Ok(result)
}

// Successful SMA folds on this thread: proof of engagement for tests and
// `PINTAIL_AGG_DEBUG` diagnostics. Thread-local because the fold runs
// synchronously on the plan-building thread and the counter's only
// consumers are same-thread test assertions — a process-wide counter made
// those assertions race with folds from concurrently running tests.
thread_local! {
    static SMA_FOLD_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[allow(dead_code)] // test-and-diagnostics accessor; production reads go through PINTAIL_AGG_DEBUG
pub(crate) fn sma_fold_hits() -> u64 {
    SMA_FOLD_HITS.with(std::cell::Cell::get)
}

/// Folds per-segment SMAs into finished bare-aggregate states and merges
/// the residual memtable rows through the normal update path, so the whole
/// table never rescans while it ingests (WS3-B). Returns `None` whenever
/// any aggregate, column, or segment falls outside the provably-exact
/// envelope; the caller then runs the ordinary scan.
#[allow(clippy::too_many_lines)]
fn try_sma_fold(
    input: &mut PullOperator,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<Option<Vec<Vec<Value>>>, ExecError> {
    // Only a direct bare Scan qualifies: any Filter above it implies
    // residual predicates, and the stream then carries no SMA input.
    let PullOperator::Scan { stream, .. } = &*input else {
        return Ok(None);
    };
    let Some(sma) = stream.sma_fold_input() else {
        return Ok(None);
    };
    let live_rows: u64 = sma.segments.iter().map(|segment| segment.live_rows).sum();
    let mut states = Vec::with_capacity(aggregates.len());
    let mut columns = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        if aggregate.distinct {
            return Ok(None);
        }
        let column = match &aggregate.expr {
            None => {
                if aggregate.function != AggregateFunction::Count {
                    return Ok(None);
                }
                None
            }
            Some(expr) => match expr.column_index() {
                Some(index) => Some(index),
                None => return Ok(None),
            },
        };
        // Every segment must carry an SMA entry for the queried column;
        // a schema-evolved segment written before the column existed
        // declines the fold rather than guessing.
        let entries = match column {
            None => Vec::new(),
            Some(index) => {
                let Some(id) = sma.column_ids.get(index).copied() else {
                    return Ok(None);
                };
                let mut entries = Vec::with_capacity(sma.segments.len());
                for segment in &sma.segments {
                    let Some(entry) = segment.columns.iter().find(|entry| entry.column_id == id)
                    else {
                        return Ok(None);
                    };
                    entries.push(entry);
                }
                entries
            }
        };
        let mut state = AggregateState::new(aggregate);
        let synthetic = match aggregate.function {
            // Segment summaries carry count/sum/min/max only, so none of
            // these can be folded without reading rows. Declining the whole
            // optimization is the point: returning None here would only mean
            // "no synthetic value", and the fold would go on to answer from
            // an empty state — NULL for a variance, all-ones for BIT_AND.
            AggregateFunction::AnyValue
            | AggregateFunction::StdDev { .. }
            | AggregateFunction::Variance { .. }
            | AggregateFunction::BitAnd
            | AggregateFunction::BitOr
            | AggregateFunction::BitXor
            | AggregateFunction::JsonObjectAgg => return Ok(None),
            AggregateFunction::Count => {
                let total = match column {
                    None => live_rows,
                    Some(_) => entries.iter().map(|entry| entry.non_null).sum(),
                };
                Some(AggregateValue::Count(total))
            }
            AggregateFunction::Sum | AggregateFunction::Average => {
                let mut total: Option<pintail_store::SmaSum> = None;
                let mut count = 0_u64;
                let mut foldable = true;
                for entry in &entries {
                    if entry.non_null == 0 {
                        continue;
                    }
                    count += entry.non_null;
                    let Some(sum) = entry.sum else {
                        foldable = false;
                        break;
                    };
                    total = Some(match (total, sum) {
                        (None, sum) => sum,
                        (
                            Some(pintail_store::SmaSum::Int(left)),
                            pintail_store::SmaSum::Int(right),
                        ) => pintail_store::SmaSum::Int(
                            left.checked_add(right).ok_or(ExecError::NumericOverflow)?,
                        ),
                        (
                            Some(pintail_store::SmaSum::Float(left)),
                            pintail_store::SmaSum::Float(right),
                        ) => pintail_store::SmaSum::Float(left + right),
                        (
                            Some(pintail_store::SmaSum::DecimalUnits { units, scale }),
                            pintail_store::SmaSum::DecimalUnits {
                                units: right,
                                scale: right_scale,
                            },
                        ) if scale == right_scale => pintail_store::SmaSum::DecimalUnits {
                            units: units.checked_add(right).ok_or(ExecError::NumericOverflow)?,
                            scale,
                        },
                        _ => {
                            foldable = false;
                            break;
                        }
                    });
                }
                if !foldable {
                    return Ok(None);
                }
                match (aggregate.function, total) {
                    (_, None) => None,
                    (AggregateFunction::Sum, Some(total)) => match total {
                        pintail_store::SmaSum::Int(total) => {
                            let value = match aggregate.data_type.map(DataType::storage_type) {
                                Some(DataType::UInt64) => {
                                    Value::UInt64(match u64::try_from(total) {
                                        Ok(total) => total,
                                        Err(_) => return Ok(None),
                                    })
                                }
                                Some(DataType::Int64) => Value::Int64(match i64::try_from(total) {
                                    Ok(total) => total,
                                    Err(_) => return Ok(None),
                                }),
                                _ => return Ok(None),
                            };
                            Some(AggregateValue::Sum(Some(value)))
                        }
                        pintail_store::SmaSum::Float(total) => {
                            if !total.is_finite() {
                                return Ok(None);
                            }
                            Some(AggregateValue::Sum(Some(Value::float64(total))))
                        }
                        pintail_store::SmaSum::DecimalUnits { units, scale } => {
                            Some(AggregateValue::DecimalSum {
                                units,
                                scale,
                                float_output: aggregate_uses_float(aggregate),
                            })
                        }
                    },
                    (AggregateFunction::Average, Some(total)) => {
                        if let Some(result_scale) = decimal_average_scale(aggregate) {
                            // Exact decimal AVG: rescale the fold's exact
                            // totals to the widened result scale; decline
                            // the fold rather than round through f64.
                            let units = match total {
                                pintail_store::SmaSum::Int(total) => {
                                    decimal_units_from_int(total, result_scale)
                                }
                                pintail_store::SmaSum::DecimalUnits { units, scale }
                                    if scale <= result_scale =>
                                {
                                    decimal_units_from_int(units, result_scale - scale)
                                }
                                _ => None,
                            };
                            let Some(units) = units else {
                                return Ok(None);
                            };
                            Some(AggregateValue::DecimalAverage {
                                units,
                                scale: result_scale,
                                count,
                            })
                        } else {
                            #[allow(clippy::cast_precision_loss)]
                            let sum = match total {
                                pintail_store::SmaSum::Int(total) => total as f64,
                                pintail_store::SmaSum::Float(total) => total,
                                pintail_store::SmaSum::DecimalUnits { units, scale } => {
                                    units as f64 / 10_f64.powi(i32::from(scale))
                                }
                            };
                            if !sum.is_finite() {
                                return Ok(None);
                            }
                            Some(AggregateValue::Average { sum, count })
                        }
                    }
                    _ => unreachable!("outer match covers Sum and Average"),
                }
            }
            AggregateFunction::Minimum | AggregateFunction::Maximum => {
                let mut folded: Option<pintail_store::SmaExtremes> = None;
                for entry in &entries {
                    if entry.non_null == 0 {
                        continue;
                    }
                    let Some(extremes) = entry.extremes else {
                        return Ok(None);
                    };
                    folded = Some(match (folded, extremes) {
                        (None, extremes) => extremes,
                        (
                            Some(pintail_store::SmaExtremes::Int { min, max }),
                            pintail_store::SmaExtremes::Int {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::Int {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::UInt { min, max }),
                            pintail_store::SmaExtremes::UInt {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::UInt {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::Float { min, max }),
                            pintail_store::SmaExtremes::Float {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::Float {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::DecimalUnits { min, max, scale }),
                            pintail_store::SmaExtremes::DecimalUnits {
                                min: right_min,
                                max: right_max,
                                scale: right_scale,
                            },
                        ) if scale == right_scale => pintail_store::SmaExtremes::DecimalUnits {
                            min: min.min(right_min),
                            max: max.max(right_max),
                            scale,
                        },
                        (
                            Some(pintail_store::SmaExtremes::Temporal { min, max, units }),
                            pintail_store::SmaExtremes::Temporal {
                                min: right_min,
                                max: right_max,
                                units: right_units,
                            },
                        ) if units == right_units => pintail_store::SmaExtremes::Temporal {
                            min: min.min(right_min),
                            max: max.max(right_max),
                            units,
                        },
                        _ => return Ok(None),
                    });
                }
                match folded {
                    None => None,
                    Some(extremes) => {
                        let minimum = aggregate.function == AggregateFunction::Minimum;
                        let value = match extremes {
                            pintail_store::SmaExtremes::Int { min, max } => {
                                Value::Int64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::UInt { min, max } => {
                                Value::UInt64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::Float { min, max } => {
                                Value::float64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::DecimalUnits { min, max, scale } => {
                                Value::Utf8(pintail_types::format_decimal_scaled(
                                    if minimum { min } else { max },
                                    scale,
                                ))
                            }
                            pintail_store::SmaExtremes::Temporal { min, max, units } => {
                                match units.format(if minimum { min } else { max }) {
                                    Some(text) => Value::Utf8(text),
                                    None => return Ok(None),
                                }
                            }
                        };
                        Some(if minimum {
                            AggregateValue::Minimum(Some(value))
                        } else {
                            AggregateValue::Maximum(Some(value))
                        })
                    }
                }
            }
            AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg => {
                return Ok(None);
            }
        };
        if let Some(value) = synthetic {
            state.merge(
                aggregate,
                AggregateState {
                    value,
                    seen: None,
                    extreme_number: None,
                    extreme_units: None,
                },
                memory,
            )?;
        }
        states.push(state);
        columns.push(column);
    }
    for row in &sma.rows {
        for ((state, aggregate), column) in states.iter_mut().zip(aggregates).zip(&columns) {
            match column {
                None => state.update(aggregate, &Value::UInt64(1), memory)?,
                Some(index) => {
                    let Some(value) = row.get(*index) else {
                        return Err(ExecError::InvalidBatch(
                            "SMA residual row does not match the scan projection",
                        ));
                    };
                    state.update(aggregate, value, memory)?;
                }
            }
        }
    }
    let row = states
        .into_iter()
        .map(|state| state.finish(memory))
        .collect::<Result<Vec<_>, _>>()?;
    memory.reserve(estimated_row_payload_bytes(&row))?;
    SMA_FOLD_HITS.with(|hits| hits.set(hits.get() + 1));
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        eprintln!(
            "[agg] SMA fold: {} segments, {} residual rows",
            sma.segments.len(),
            sma.rows.len()
        );
    }
    Ok(Some(vec![row]))
}

#[allow(clippy::too_many_lines)]
fn build_hash_aggregate_scan(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    if group_by.is_empty()
        && !aggregates.is_empty()
        && aggregates.iter().all(|aggregate| {
            aggregate.function == AggregateFunction::Count
                && aggregate.expr.is_none()
                && !aggregate.distinct
        })
    {
        let mut count = 0_u64;
        while let Some(batch) = input.next_batch(memory)? {
            count = count
                .checked_add(
                    u64::try_from(batch.visible_row_count())
                        .map_err(|_| ExecError::NumericOverflow)?,
                )
                .ok_or(ExecError::NumericOverflow)?;
        }
        let row = vec![Value::UInt64(count); aggregates.len()];
        memory.reserve(estimated_row_payload_bytes(&row))?;
        return Ok(MaterializedRows {
            rows: vec![row],
            position: 0,
        });
    }
    if !group_by.is_empty() {
        let direct_columns = group_by
            .iter()
            .map(CompiledExpr::column_index)
            .collect::<Option<Vec<_>>>();
        if let Some(group_columns) = direct_columns.as_deref()
            && let Some(rows) =
                build_fused_inner_join_aggregate(input, group_columns, aggregates, memory)?
        {
            return Ok(rows);
        }
        if aggregates.iter().all(|aggregate| {
            !matches!(
                aggregate.function,
                AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
            )
        }) {
            return build_buffered_hash_aggregate(
                input,
                group_by,
                direct_columns.as_deref(),
                aggregates,
                memory,
            );
        }
        if let Some(group_columns) = direct_columns {
            return build_direct_column_aggregate(input, None, &group_columns, aggregates, memory);
        }
    }

    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    if group_by.is_empty() {
        reserve_hash_map_entries(
            &mut groups,
            1,
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            0,
            memory,
        )?;
        memory.reserve(aggregates.len().saturating_mul(size_of::<AggregateState>()))?;
        groups.insert(
            Vec::new(),
            AggregateGroup {
                values: Vec::new(),
                states: aggregates.iter().map(AggregateState::new).collect(),
            },
        );
    }

    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        reserve_hash_map_entries(
            &mut groups,
            batch.visible_row_count().min(64),
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            batch_bytes,
            memory,
        )?;
        for row in batch.selection().selected_rows() {
            let group_expression_memory = group_by
                .iter()
                .map(|expression| expression.allocation_upper_bound(&batch, row))
                .fold(0_usize, usize::saturating_add);
            let group_memory = group_expression_memory
                .saturating_mul(13)
                .saturating_add(
                    group_by
                        .len()
                        .saturating_mul(size_of::<Value>())
                        .saturating_mul(2),
                )
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
                .saturating_add(HASH_ENTRY_OVERHEAD);
            memory.ensure_transient(batch_bytes.saturating_add(group_memory))?;
            let values = group_by
                .iter()
                .map(|expression| expression.evaluate(&batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            let key = values
                .iter()
                .cloned()
                .map(|value| normalized_hash_key(value).unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            if groups.len() == groups.capacity() {
                let growth = groups.capacity().max(1);
                reserve_hash_map_entries(
                    &mut groups,
                    growth,
                    size_of::<Vec<Value>>()
                        .saturating_add(size_of::<AggregateGroup>())
                        .saturating_add(HASH_ENTRY_OVERHEAD),
                    batch_bytes,
                    memory,
                )?;
            }
            let group = match groups.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let bytes = estimated_row_payload_bytes(&values)
                        .saturating_add(estimated_row_payload_bytes(entry.key()))
                        .saturating_add(
                            aggregates.len().saturating_mul(size_of::<AggregateState>()),
                        );
                    memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                    memory.reserve(bytes)?;
                    entry.insert(AggregateGroup {
                        values,
                        states: aggregates.iter().map(AggregateState::new).collect(),
                    })
                }
            };
            update_aggregate_states(
                &batch,
                row,
                batch_bytes,
                aggregates,
                &mut group.states,
                memory,
            )?;
        }
    }

    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for (_, group) in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
/// One or two `DATEPART(column)` group expressions over packed temporal
/// columns, when every part is bounded enough for 20-bit packing.
fn date_part_key_source(
    group_by: &[CompiledExpr],
    batch: &RecordBatch,
) -> Option<TwoPassKeySource> {
    let part_of = |expr: &CompiledExpr| -> Option<(DatePart, usize)> {
        let CompiledExpr::Scalar {
            function: ScalarFunction::DatePart(part),
            args,
            ..
        } = expr
        else {
            return None;
        };
        let [CompiledExpr::Column(column)] = args.as_slice() else {
            return None;
        };
        let vector = batch.column(*column)?;
        if !matches!(
            vector.data_type(),
            DataType::Date32 | DataType::DateTime64 { .. }
        ) {
            return None;
        }
        let (typed, _) = vector.typed()?;
        matches!(typed, crate::batch::TypedValues::Temporal { .. }).then_some((*part, *column))
    };
    match group_by {
        [only] => Some(TwoPassKeySource::DateParts {
            parts: [Some(part_of(only)?), None],
        }),
        [first, second] => Some(TwoPassKeySource::DateParts {
            parts: [Some(part_of(first)?), Some(part_of(second)?)],
        }),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn build_buffered_hash_aggregate(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    direct_columns: Option<&[usize]>,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let Some(first_batch) = input.next_batch(memory)? else {
        return Ok(MaterializedRows {
            rows: Vec::new(),
            position: 0,
        });
    };
    // GROUP BY over date-part expressions (the Q5 shape): bounded int
    // domains ride the streaming two-pass without Value keys.
    if direct_columns.is_none()
        && let Some(keys) = date_part_key_source(group_by, &first_batch)
        && let Some(lanes) = two_pass_lanes(aggregates, &first_batch)
    {
        return build_streaming_two_pass_aggregate(
            input,
            first_batch,
            keys,
            &lanes,
            aggregates,
            memory,
        );
    }
    let utf8_column = |column: &usize| {
        first_batch
            .column(*column)
            .is_some_and(|values| values.data_type() == DataType::Utf8)
    };
    let direct_eligible = match direct_columns {
        Some([column]) => first_batch.column(*column).is_some_and(|values| {
            matches!(
                values.data_type().storage_type(),
                DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
            ) || (values.data_type() == DataType::Utf8
                && two_pass_lanes(aggregates, &first_batch).is_some())
        }),
        Some([first, second]) => {
            utf8_column(first)
                && utf8_column(second)
                && two_pass_lanes(aggregates, &first_batch).is_some()
        }
        _ => false,
    };
    if direct_eligible {
        // Single int-typed group columns take the sequential direct path:
        // its scalar index avoids the per-row Vec<Value> keys and the
        // per-round global merges that the buffered parallel path pays.
        // Routing them through the parallel path regressed Q6 (2M groups
        // over 20M rows) from seconds to minutes — e02's parallel win used
        // dense arrays at low cardinality and does not transfer to sparse
        // high-cardinality keys. Parallel high-cardinality aggregation
        // needs a partitioned design and its own experiment first.
        return build_direct_column_aggregate(
            input,
            Some(first_batch),
            direct_columns.expect("matched direct columns"),
            aggregates,
            memory,
        );
    }

    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    // Bytes reserved for the live group map (entries plus state growth),
    // measured through used() snapshots around the sequential merge section
    // so state-internal reserves (distinct sets) are included.
    let mut groups_reserved = 0_usize;
    let mut spill_runs: Vec<AggregateSpillRun> = Vec::new();
    let mut first_batch = Some(first_batch);
    let per_row_upper = group_by
        .len()
        .saturating_mul(size_of::<Value>())
        .saturating_mul(2)
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(size_of::<AggregateGroup>())
        .saturating_add(HASH_ENTRY_OVERHEAD)
        .saturating_add(256);
    loop {
        let mut batches = Vec::with_capacity(8);
        let mut batch_reserved = 0_usize;
        let mut selected_rows = 0_usize;
        while batches.len() < 8 {
            let batch = if let Some(batch) = first_batch.take() {
                Some(batch)
            } else {
                input.next_batch(memory)?
            };
            let Some(batch) = batch else {
                break;
            };
            let bytes = batch.estimated_bytes();
            reserve_or_spill_groups(
                bytes,
                &mut groups,
                &mut groups_reserved,
                &mut spill_runs,
                memory,
            )?;
            batch_reserved = batch_reserved.saturating_add(bytes);
            selected_rows = selected_rows.saturating_add(batch.visible_row_count());
            batches.push(batch);
            // Tight ceilings cap the round instead of reserving a
            // conservative upper bound larger than the whole budget; the
            // default ceiling keeps full 8-batch rounds.
            if selected_rows.saturating_mul(per_row_upper) > memory.limit() / 4 {
                break;
            }
        }
        if batches.is_empty() {
            break;
        }
        let local_upper = selected_rows.saturating_mul(per_row_upper);
        reserve_or_spill_groups(
            local_upper,
            &mut groups,
            &mut groups_reserved,
            &mut spill_runs,
            memory,
        )?;
        let mut used_before_merge = memory.used();
        let partials = batches
            .par_iter()
            .map(|batch| {
                direct_columns.map_or_else(
                    || build_local_expression_groups(batch, group_by, aggregates),
                    |columns| build_local_direct_groups(batch, columns, aggregates),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        for partial in partials {
            for entry in partial {
                let mut pending = Some(entry);
                while let Some((key, partial_group)) = pending.take() {
                    match merge_partial_group(
                        &mut groups,
                        key,
                        partial_group,
                        aggregates,
                        batch_reserved,
                        memory,
                    ) {
                        Ok(()) => {}
                        // The entry was handed back untouched, so spilling
                        // the map here and retrying it is safe.
                        Err((ExecError::MemoryLimitExceeded { .. }, Some(returned)))
                            if !groups.is_empty() =>
                        {
                            groups_reserved = groups_reserved
                                .saturating_add(memory.used().saturating_sub(used_before_merge));
                            spill_runs.push(write_aggregate_spill_run(&mut groups, memory)?);
                            memory.release(groups_reserved);
                            groups_reserved = 0;
                            used_before_merge = memory.used();
                            pending = Some(returned);
                        }
                        Err((error, _)) => return Err(error),
                    }
                }
            }
        }
        groups_reserved =
            groups_reserved.saturating_add(memory.used().saturating_sub(used_before_merge));
        memory.release(local_upper.saturating_add(batch_reserved));
        // Proactive spill at half the ceiling, mirroring the sort spill:
        // upstream scans size their working sets from the remaining
        // headroom, so a group map that hoards the budget until hard
        // failure starves them.
        if groups_reserved > memory.limit() / 2 && !groups.is_empty() {
            spill_runs.push(write_aggregate_spill_run(&mut groups, memory)?);
            memory.release(groups_reserved);
            groups_reserved = 0;
        }
    }
    if spill_runs.is_empty() {
        return finish_aggregate_groups(groups.into_values(), memory);
    }
    memory.release(groups_reserved);
    merge_spilled_aggregate_groups(spill_runs, groups, aggregates, memory)
}

/// The error and, when the failure struck *before* the entry touched the
/// map, the entry itself so the caller can spill and retry it; `None`
/// means a mid-merge failure that cannot be replayed.
type MergeGroupFailure = (ExecError, Option<(Vec<Value>, AggregateGroup)>);

/// Merges one partial group into the live map. A memory failure *before*
/// the entry touches the map hands the entry back (`Some`) so the caller
/// can spill and retry it; a failure while merging states cannot be
/// replayed and returns `None`.
fn merge_partial_group(
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    key: Vec<Value>,
    partial_group: AggregateGroup,
    aggregates: &[CompiledAggregate],
    batch_reserved: usize,
    memory: &MemoryTracker,
) -> Result<(), MergeGroupFailure> {
    if groups.len() == groups.capacity() {
        let growth = groups.capacity().max(64);
        if let Err(error) = reserve_hash_map_entries(
            groups,
            growth,
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            batch_reserved,
            memory,
        ) {
            return Err((error, Some((key, partial_group))));
        }
    }
    let group = match groups.entry(key) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let bytes = estimated_row_payload_bytes(&partial_group.values)
                .saturating_add(estimated_row_payload_bytes(entry.key()))
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()));
            if let Err(error) = memory.reserve(bytes) {
                return Err((error, Some((entry.into_key(), partial_group))));
            }
            entry.insert(AggregateGroup {
                values: partial_group.values,
                states: aggregates.iter().map(AggregateState::new).collect(),
            })
        }
    };
    for ((state, partial_state), aggregate) in group
        .states
        .iter_mut()
        .zip(partial_group.states)
        .zip(aggregates)
    {
        state
            .merge(aggregate, partial_state, memory)
            .map_err(|error| (error, None))?;
    }
    Ok(())
}

/// Reserves `bytes`, spilling the live group map as an on-disk run and
/// retrying once when the first attempt exceeds the memory ceiling. Only
/// reserves made *between* merge sections are safe to retry this way; a
/// failure mid-merge propagates because the interrupted group state cannot
/// be replayed.
fn reserve_or_spill_groups(
    bytes: usize,
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    groups_reserved: &mut usize,
    spill_runs: &mut Vec<AggregateSpillRun>,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    match memory.reserve(bytes) {
        Ok(()) => Ok(()),
        Err(ExecError::MemoryLimitExceeded { .. }) if !groups.is_empty() => {
            spill_runs.push(write_aggregate_spill_run(groups, memory)?);
            memory.release(*groups_reserved);
            *groups_reserved = 0;
            memory.reserve(bytes)
        }
        Err(error) => Err(error),
    }
}

/// One spilled aggregation run: entries sorted by their encoded group key,
/// one length-framed record each, streamed back in write order.
struct AggregateSpillRun {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
    _path: tempfile::TempPath,
    _reservation: spill::SpillReservation,
}

struct SpilledGroupEntry {
    /// The group key, encoded once at spill time; the k-way merge orders
    /// and matches entries on these exact bytes.
    key: Vec<u8>,
    values: Vec<Value>,
    states: Vec<SpilledAggregateState>,
}

struct SpilledAggregateState {
    value: SpilledAggregateValue,
    /// Drained DISTINCT keys; revival replays them through the regular
    /// update path, which rebuilds the int-set/value-set split.
    seen: Option<Vec<Value>>,
}

/// Spillable mirror of [`AggregateValue`]. `i128` units travel as decimal
/// strings so the encoding stays independent of integer width.
enum SpilledAggregateValue {
    Count(u64),
    Sum(Option<Value>),
    DecimalSum {
        units: String,
        scale: u8,
        float_output: bool,
    },
    Average {
        sum: f64,
        count: u64,
    },
    DecimalAverage {
        units: String,
        scale: u8,
        count: u64,
    },
    Minimum(Option<Value>),
    Maximum(Option<Value>),
    AnyValue(Option<Value>),
    Moments {
        count: u64,
        mean: f64,
        m2: f64,
        sample: bool,
        stddev: bool,
    },
    BitFold {
        accumulator: u64,
        seen: bool,
    },
}

fn spill_aggregate_state(state: AggregateState) -> Result<SpilledAggregateState, ExecError> {
    let AggregateState { value, seen, .. } = state;
    let value = match value {
        AggregateValue::Count(count) => SpilledAggregateValue::Count(count),
        AggregateValue::Sum(sum) => SpilledAggregateValue::Sum(sum),
        AggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => SpilledAggregateValue::DecimalSum {
            units: units.to_string(),
            scale,
            float_output,
        },
        AggregateValue::Average { sum, count } => SpilledAggregateValue::Average { sum, count },
        AggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => SpilledAggregateValue::DecimalAverage {
            units: units.to_string(),
            scale,
            count,
        },
        AggregateValue::Minimum(value) => SpilledAggregateValue::Minimum(value),
        AggregateValue::Maximum(value) => SpilledAggregateValue::Maximum(value),
        AggregateValue::AnyValue(value) => SpilledAggregateValue::AnyValue(value),
        AggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        },
        AggregateValue::BitFold { accumulator, seen } => {
            SpilledAggregateValue::BitFold { accumulator, seen }
        }
        AggregateValue::GroupConcat { .. }
        | AggregateValue::JsonArrayAgg { .. }
        | AggregateValue::JsonObjectAgg { .. } => {
            return Err(ExecError::InvalidPhysicalPlan(
                "aggregation spill reached a non-spillable aggregate state",
            ));
        }
    };
    Ok(SpilledAggregateState {
        value,
        seen: seen.map(DistinctSeen::drain_values),
    })
}

fn spilled_units(units: &str) -> Result<i128, ExecError> {
    units
        .parse::<i128>()
        .map_err(|_| ExecError::Source("aggregate spill decode: bad decimal units".to_owned()))
}

/// Rebuilds a live aggregate state from its spilled form. DISTINCT states
/// replay their seen keys through the regular update path; everything else
/// restores the accumulator directly (the extreme caches stay invalidated,
/// exactly as after a merge).
fn revive_aggregate_state(
    spilled: SpilledAggregateState,
    aggregate: &CompiledAggregate,
    memory: &MemoryTracker,
) -> Result<AggregateState, ExecError> {
    let mut state = AggregateState::new(aggregate);
    if let Some(values) = spilled.seen {
        for value in values {
            state.update(aggregate, &value, memory)?;
        }
        return Ok(state);
    }
    state.value = match spilled.value {
        SpilledAggregateValue::Count(count) => AggregateValue::Count(count),
        SpilledAggregateValue::AnyValue(value) => AggregateValue::AnyValue(value),
        SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => AggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        },
        SpilledAggregateValue::BitFold { accumulator, seen } => {
            AggregateValue::BitFold { accumulator, seen }
        }
        SpilledAggregateValue::Sum(sum) => AggregateValue::Sum(sum),
        SpilledAggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => AggregateValue::DecimalSum {
            units: spilled_units(&units)?,
            scale,
            float_output,
        },
        SpilledAggregateValue::Average { sum, count } => AggregateValue::Average { sum, count },
        SpilledAggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => AggregateValue::DecimalAverage {
            units: spilled_units(&units)?,
            scale,
            count,
        },
        SpilledAggregateValue::Minimum(value) => AggregateValue::Minimum(value),
        SpilledAggregateValue::Maximum(value) => AggregateValue::Maximum(value),
    };
    Ok(state)
}

/// Drains the live group map into one sorted on-disk run.
fn write_aggregate_spill_run(
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    memory: &MemoryTracker,
) -> Result<AggregateSpillRun, ExecError> {
    let mut entries = groups
        .drain()
        .map(|(key, group)| {
            let mut encoder = spill::Encoder::with_capacity(32);
            encoder.values(&key);
            Ok(SpilledGroupEntry {
                key: encoder.finish(),
                values: group.values,
                states: group
                    .states
                    .into_iter()
                    .map(spill_aggregate_state)
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, ExecError>>()?;
    entries.sort_unstable_by(|left, right| left.key.cmp(&right.key));
    let file = spill::spill_file("pintail-aggregate-spill-", memory.spill())
        .map_err(|error| ExecError::Source(format!("aggregate spill create: {error}")))?;
    let (file, path, mut reservation) = file.into_parts();
    let mut writer = std::io::BufWriter::new(file);
    for entry in entries {
        let mut encoder = spill::Encoder::with_capacity(entry.key.len() + 64);
        encoder.bytes(&entry.key);
        encoder.values(&entry.values);
        encoder.count(entry.states.len());
        for state in &entry.states {
            encode_aggregate_state(&mut encoder, state);
        }
        spill::write_record_quota(&mut writer, &encoder.finish(), &mut reservation)
            .map_err(|error| ExecError::Source(format!("aggregate spill write: {error}")))?;
    }
    let mut file = writer
        .into_inner()
        .map_err(|error| ExecError::Source(format!("aggregate spill flush: {error}")))?;
    std::io::Seek::rewind(&mut file)
        .map_err(|error| ExecError::Source(format!("aggregate spill rewind: {error}")))?;
    Ok(AggregateSpillRun {
        reader: std::io::BufReader::new(file),
        payload: Vec::new(),
        _path: path,
        _reservation: reservation,
    })
}

impl AggregateSpillRun {
    fn next_entry(&mut self) -> Result<Option<SpilledGroupEntry>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("aggregate spill read: {error}")))?
        {
            return Ok(None);
        }
        let mut decoder = spill::Decoder::new(&self.payload);
        let entry = (|| {
            let key = decoder.bytes()?.to_vec();
            let values = decoder.values()?;
            let count = decoder.count()?;
            let mut states = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                states.push(decode_aggregate_state(&mut decoder)?);
            }
            Ok::<_, String>(SpilledGroupEntry {
                key,
                values,
                states,
            })
        })()
        .map_err(|error| ExecError::Source(format!("aggregate spill decode: {error}")))?;
        Ok(Some(entry))
    }
}

const AGGREGATE_COUNT: u8 = 0;
const AGGREGATE_SUM: u8 = 1;
const AGGREGATE_DECIMAL_SUM: u8 = 2;
const AGGREGATE_AVERAGE: u8 = 3;
const AGGREGATE_DECIMAL_AVERAGE: u8 = 4;
const AGGREGATE_MINIMUM: u8 = 5;
const AGGREGATE_MAXIMUM: u8 = 6;
const AGGREGATE_ANY_VALUE: u8 = 7;
const AGGREGATE_MOMENTS: u8 = 8;
const AGGREGATE_BIT_FOLD: u8 = 9;

fn encode_aggregate_state(encoder: &mut spill::Encoder, state: &SpilledAggregateState) {
    match &state.value {
        SpilledAggregateValue::AnyValue(value) => {
            encoder.u8(AGGREGATE_ANY_VALUE);
            encoder.optional_value(value.as_ref());
        }
        SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => {
            encoder.u8(AGGREGATE_MOMENTS);
            encoder.u64(*count);
            encoder.f64(*mean);
            encoder.f64(*m2);
            encoder.bool(*sample);
            encoder.bool(*stddev);
        }
        SpilledAggregateValue::BitFold { accumulator, seen } => {
            encoder.u8(AGGREGATE_BIT_FOLD);
            encoder.u64(*accumulator);
            encoder.bool(*seen);
        }
        SpilledAggregateValue::Count(count) => {
            encoder.u8(AGGREGATE_COUNT);
            encoder.u64(*count);
        }
        SpilledAggregateValue::Sum(sum) => {
            encoder.u8(AGGREGATE_SUM);
            encoder.optional_value(sum.as_ref());
        }
        SpilledAggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => {
            encoder.u8(AGGREGATE_DECIMAL_SUM);
            encoder.str(units);
            encoder.u8(*scale);
            encoder.bool(*float_output);
        }
        SpilledAggregateValue::Average { sum, count } => {
            encoder.u8(AGGREGATE_AVERAGE);
            encoder.f64(*sum);
            encoder.u64(*count);
        }
        SpilledAggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => {
            encoder.u8(AGGREGATE_DECIMAL_AVERAGE);
            encoder.str(units);
            encoder.u8(*scale);
            encoder.u64(*count);
        }
        SpilledAggregateValue::Minimum(value) => {
            encoder.u8(AGGREGATE_MINIMUM);
            encoder.optional_value(value.as_ref());
        }
        SpilledAggregateValue::Maximum(value) => {
            encoder.u8(AGGREGATE_MAXIMUM);
            encoder.optional_value(value.as_ref());
        }
    }
    match &state.seen {
        None => encoder.bool(false),
        Some(seen) => {
            encoder.bool(true);
            encoder.values(seen);
        }
    }
}

fn decode_aggregate_state(
    decoder: &mut spill::Decoder<'_>,
) -> Result<SpilledAggregateState, String> {
    let value = match decoder.u8()? {
        AGGREGATE_COUNT => SpilledAggregateValue::Count(decoder.u64()?),
        AGGREGATE_ANY_VALUE => SpilledAggregateValue::AnyValue(decoder.optional_value()?),
        AGGREGATE_MOMENTS => SpilledAggregateValue::Moments {
            count: decoder.u64()?,
            mean: decoder.f64()?,
            m2: decoder.f64()?,
            sample: decoder.bool()?,
            stddev: decoder.bool()?,
        },
        AGGREGATE_BIT_FOLD => SpilledAggregateValue::BitFold {
            accumulator: decoder.u64()?,
            seen: decoder.bool()?,
        },
        AGGREGATE_SUM => SpilledAggregateValue::Sum(decoder.optional_value()?),
        AGGREGATE_DECIMAL_SUM => SpilledAggregateValue::DecimalSum {
            units: decoder.string()?,
            scale: decoder.u8()?,
            float_output: decoder.bool()?,
        },
        AGGREGATE_AVERAGE => SpilledAggregateValue::Average {
            sum: decoder.f64()?,
            count: decoder.u64()?,
        },
        AGGREGATE_DECIMAL_AVERAGE => SpilledAggregateValue::DecimalAverage {
            units: decoder.string()?,
            scale: decoder.u8()?,
            count: decoder.u64()?,
        },
        AGGREGATE_MINIMUM => SpilledAggregateValue::Minimum(decoder.optional_value()?),
        AGGREGATE_MAXIMUM => SpilledAggregateValue::Maximum(decoder.optional_value()?),
        other => return Err(format!("spilled aggregate holds unknown tag {other}")),
    };
    let seen = if decoder.bool()? {
        Some(decoder.values()?)
    } else {
        None
    };
    Ok(SpilledAggregateState { value, seen })
}

/// K-way merges the spilled runs (plus the resident remainder written as a
/// final run) into finished output rows. Runs are keyed and sorted by the
/// serialized group key, so equal groups are adjacent across run heads and
/// partial states combine through the existing merge path.
fn merge_spilled_aggregate_groups(
    mut runs: Vec<AggregateSpillRun>,
    mut groups: HashMap<Vec<Value>, AggregateGroup>,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    if !groups.is_empty() {
        runs.push(write_aggregate_spill_run(&mut groups, memory)?);
    }
    let mut heads = Vec::with_capacity(runs.len());
    for run in &mut runs {
        heads.push(run.next_entry()?);
    }
    let mut rows: Vec<Vec<Value>> = Vec::new();
    loop {
        let mut winner: Option<usize> = None;
        for (index, head) in heads.iter().enumerate() {
            let Some(candidate) = head else { continue };
            let better = match winner {
                None => true,
                Some(current) => {
                    let current_head = heads[current]
                        .as_ref()
                        .expect("winner head is always occupied");
                    candidate.key < current_head.key
                }
            };
            if better {
                winner = Some(index);
            }
        }
        let Some(winner) = winner else { break };
        let replacement = runs[winner].next_entry()?;
        let entry =
            std::mem::replace(&mut heads[winner], replacement).expect("winner head is occupied");
        if entry.states.len() != aggregates.len() {
            return Err(ExecError::Source(
                "aggregate spill decode: state arity mismatch".to_owned(),
            ));
        }
        // Revival and finishing reserve transient state (distinct sets,
        // merge growth) that dies with this group; measure and release it,
        // then account the finished row alone.
        let used_before_group = memory.used();
        let mut states = entry
            .states
            .into_iter()
            .zip(aggregates)
            .map(|(state, aggregate)| revive_aggregate_state(state, aggregate, memory))
            .collect::<Result<Vec<_>, _>>()?;
        // Fold every other run's entry for the same key into this group.
        for index in 0..heads.len() {
            while heads[index]
                .as_ref()
                .is_some_and(|head| head.key == entry.key)
            {
                let replacement = runs[index].next_entry()?;
                let duplicate = std::mem::replace(&mut heads[index], replacement)
                    .expect("matching head is occupied");
                if duplicate.states.len() != aggregates.len() {
                    return Err(ExecError::Source(
                        "aggregate spill decode: state arity mismatch".to_owned(),
                    ));
                }
                for ((state, spilled), aggregate) in
                    states.iter_mut().zip(duplicate.states).zip(aggregates)
                {
                    let other = revive_aggregate_state(spilled, aggregate, memory)?;
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
        let mut row = entry.values;
        row.reserve(states.len());
        for state in states {
            row.push(state.finish(memory)?);
        }
        memory.release(memory.used().saturating_sub(used_before_group));
        memory
            .reserve(size_of::<Vec<Value>>().saturating_add(estimated_row_payload_bytes(&row)))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
fn build_fused_inner_join_aggregate(
    input: &mut PullOperator,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<Option<MaterializedRows>, ExecError> {
    let PullOperator::HashJoin {
        left,
        right,
        kind,
        left_key,
        right_key,
        extra_keys,
        key_mode,
        column_types,
        right_width,
        state,
    } = input
    else {
        return Ok(None);
    };
    let left_width = column_types.len().saturating_sub(*right_width);
    // The fused spine probes on the primary key alone; composite-key joins
    // stay on the general operator.
    if !extra_keys.is_empty()
        || *kind != BoundJoinKind::Inner
        || state.is_some()
        || *right_width > column_types.len()
        || group_columns
            .iter()
            .any(|column| *column < left_width || *column >= column_types.len())
        || aggregates.iter().any(|aggregate| {
            aggregate.distinct
                || matches!(
                    aggregate.function,
                    AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
                )
        })
        || aggregates.iter().any(|aggregate| {
            aggregate
                .expr
                .as_ref()
                .is_some_and(|expression| expression.column_index().is_none())
        })
    {
        return Ok(None);
    }

    let right_group_columns = group_columns
        .iter()
        .map(|column| column - left_width)
        .collect::<Vec<_>>();
    let build_start = memory.used();
    let join = build_hash_join_state(right, right_key, *key_mode, extra_keys, memory)?;
    let build_reserved = memory.used().saturating_sub(build_start);
    // Dense direct-address probe (experiments/RESULTS.md e04, 2.4-4.2x):
    // Integer-mode build keys occupying a small dense range trade the
    // per-probe evaluate+hash for one bounds-checked index lookup. MySQL
    // auto-increment keys make this the common case, not the exception.
    let dense: Option<DenseJoinTable<'_>> =
        if matches!(key_mode, JoinKeyMode::Integer) && !join.build.is_empty() {
            let mut min = i128::MAX;
            let mut max = i128::MIN;
            let mut integers = true;
            for key in join.build.keys() {
                match key {
                    JoinHashKey::NegativeInteger(value) => {
                        min = min.min(i128::from(*value));
                        max = max.max(i128::from(*value));
                    }
                    JoinHashKey::NonNegativeInteger(value) => {
                        min = min.min(i128::from(*value));
                        max = max.max(i128::from(*value));
                    }
                    _ => {
                        integers = false;
                        break;
                    }
                }
            }
            if integers && max - min < MAX_DENSE_SPAN {
                let span = usize::try_from(max - min).expect("bounded span") + 1;
                let mut table: Vec<Option<&Vec<Vec<Value>>>> = vec![None; span];
                for (key, bucket) in &join.build {
                    let value = match key {
                        JoinHashKey::NegativeInteger(value) => i128::from(*value),
                        JoinHashKey::NonNegativeInteger(value) => i128::from(*value),
                        _ => unreachable!("verified integer keys"),
                    };
                    table[usize::try_from(value - min).expect("within span")] = Some(bucket);
                }
                Some((min, table))
            } else {
                None
            }
        } else {
            None
        };
    let plan = resolve_join_group_plan(&join.build, &right_group_columns)?;
    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    loop {
        let mut batches = Vec::with_capacity(8);
        let mut batch_reserved = 0_usize;
        while batches.len() < 8 {
            let Some(batch) = left.next_batch(memory)? else {
                break;
            };
            let bytes = batch.estimated_bytes();
            memory.reserve(bytes)?;
            batch_reserved = batch_reserved.saturating_add(bytes);
            batches.push(batch);
        }
        if batches.is_empty() {
            break;
        }
        let selected_rows = batches
            .iter()
            .map(RecordBatch::visible_row_count)
            .sum::<usize>();
        let local_upper = selected_rows.saturating_mul(
            right_group_columns
                .len()
                .saturating_mul(size_of::<Value>())
                .saturating_mul(2)
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD)
                .saturating_add(256),
        );
        memory.reserve(local_upper)?;
        let partials = batches
            .par_iter()
            .map(|batch| {
                build_local_fused_join_groups(
                    batch,
                    left_key,
                    *key_mode,
                    left_width,
                    aggregates,
                    &join.build,
                    dense.as_ref(),
                    &plan,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        for partial in partials {
            for (key, partial_group) in partial {
                if groups.len() == groups.capacity() {
                    let growth = groups.capacity().max(64);
                    reserve_hash_map_entries(
                        &mut groups,
                        growth,
                        size_of::<Vec<Value>>()
                            .saturating_add(size_of::<AggregateGroup>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        batch_reserved,
                        memory,
                    )?;
                }
                let group = match groups.entry(key) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let bytes = estimated_row_payload_bytes(&partial_group.values)
                            .saturating_add(estimated_row_payload_bytes(entry.key()))
                            .saturating_add(
                                aggregates.len().saturating_mul(size_of::<AggregateState>()),
                            );
                        memory.reserve(bytes)?;
                        entry.insert(AggregateGroup {
                            values: partial_group.values,
                            states: aggregates.iter().map(AggregateState::new).collect(),
                        })
                    }
                };
                for ((state, partial_state), aggregate) in group
                    .states
                    .iter_mut()
                    .zip(partial_group.states)
                    .zip(aggregates)
                {
                    state.merge(aggregate, partial_state, memory)?;
                }
            }
        }
        memory.release(local_upper.saturating_add(batch_reserved));
    }

    drop(dense);
    drop(join);
    memory.release(build_reserved);
    Ok(Some(finish_aggregate_groups(groups.into_values(), memory)?))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_local_fused_join_groups(
    batch: &RecordBatch,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    left_width: usize,
    aggregates: &[CompiledAggregate],
    build: &HashMap<JoinHashKey, Vec<Vec<Value>>>,
    dense: Option<&DenseJoinTable<'_>>,
    plan: &JoinGroupPlan,
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    // Groups are fixed by the build side: start with the resolved set and
    // index into it, so the probe loop never hashes or compares group
    // values (the Q8 profile's dominant cost).
    let mut groups = plan
        .values
        .iter()
        .map(|values| AggregateGroup {
            values: values.clone(),
            states: aggregates.iter().map(AggregateState::new).collect(),
        })
        .collect::<Vec<_>>();
    let mut touched = vec![false; groups.len()];
    let memory = MemoryTracker::new(usize::MAX);
    // Probe through the dense table when the left key is a packed integer
    // column; Integer key mode guarantees those physical variants, and NULL
    // rows skip exactly as normalized_join_key's None does.
    let left_typed = dense.and_then(|_| {
        left_key
            .column_index()
            .and_then(|column| batch.column(column))
            .and_then(ColumnVector::typed)
            .filter(|(typed, _)| {
                matches!(
                    typed,
                    crate::batch::TypedValues::Int64(_) | crate::batch::TypedValues::UInt64(_)
                )
            })
    });
    for row in batch.selection().selected_rows() {
        let matches = if let (Some((min, table)), Some((typed, validity))) = (dense, left_typed) {
            if !validity.is_valid(row) {
                continue;
            }
            let candidate = match typed {
                crate::batch::TypedValues::Int64(values) => i128::from(values[row]),
                crate::batch::TypedValues::UInt64(values) => i128::from(values[row]),
                _ => unreachable!("filtered to integer projections"),
            };
            let Some(offset) = candidate
                .checked_sub(*min)
                .and_then(|delta| usize::try_from(delta).ok())
            else {
                continue;
            };
            match table.get(offset) {
                Some(Some(bucket)) => *bucket,
                _ => continue,
            }
        } else {
            let Some(key) = normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? else {
                continue;
            };
            let Some(matches) = build.get(&key) else {
                continue;
            };
            matches
        };
        let indexes = plan
            .buckets
            .get(&(std::ptr::from_ref(matches) as usize))
            .ok_or(ExecError::InvalidPhysicalPlan(
                "probe matched a bucket outside the resolved group plan",
            ))?;
        for (right_values, group_index) in matches.iter().zip(indexes) {
            let group_index = *group_index;
            touched[group_index] = true;
            for (aggregate, state) in aggregates.iter().zip(&mut groups[group_index].states) {
                let value = match aggregate.expr.as_ref() {
                    None => &Value::Boolean(true),
                    Some(expression) => {
                        let column =
                            expression
                                .column_index()
                                .ok_or(ExecError::InvalidPhysicalPlan(
                                    "fused join aggregate expression is not a column",
                                ))?;
                        if column < left_width {
                            // Probe-side columns update typed-first: the
                            // Q8 profile showed per-row decimal text
                            // parse/format dominating this loop.
                            if update_state_from_typed_column(
                                state, aggregate, batch, column, row, &memory,
                            )? {
                                continue;
                            }
                            direct_group_value(batch, row, column)?
                        } else {
                            right_values.get(column - left_width).ok_or(
                                ExecError::InvalidPhysicalPlan(
                                    "join aggregate column is outside the joined layout",
                                ),
                            )?
                        }
                    }
                };
                state.update(aggregate, value, &memory)?;
            }
        }
    }
    Ok(groups
        .into_iter()
        .map(|group| {
            let key = group
                .values
                .iter()
                .cloned()
                .map(normalized_collation_value)
                .collect();
            (key, group)
        })
        .collect())
}

/// Dictionary-code aggregation for low-cardinality string group keys
/// (experiments/RESULTS.md e02: array-indexed accumulation, no hash table).
/// Handles one or two Utf8 group columns via base-256 composite codes mapped
/// to dense slots. Local dedup is byte-exact, mirroring the general path —
/// collation unification still happens at the normalized-key merge. Falls
/// back (`None`) whenever the shape doesn't qualify.
#[allow(clippy::too_many_lines)]
fn build_local_dictionary_groups(
    batch: &RecordBatch,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
) -> Result<Option<HashMap<Vec<Value>, AggregateGroup>>, ExecError> {
    struct DictAggregate {
        function: AggregateFunction,
        column: Option<usize>,
    }
    const MAX_CODES: usize = 256;
    const MAX_SLOTS: usize = 4096;
    if group_columns.is_empty() || group_columns.len() > 2 {
        return Ok(None);
    }
    let mut key_columns = Vec::with_capacity(group_columns.len());
    for column in group_columns {
        let Some(vector) = batch.column(*column) else {
            return Ok(None);
        };
        let Some((crate::batch::TypedValues::Utf8(keys), validity)) = vector.typed() else {
            return Ok(None);
        };
        key_columns.push((vector, keys, validity));
    }
    // Aggregate inputs: plain typed columns (numbers via the packed path) or
    // COUNT(*); anything else falls back to the general builder.
    let mut dict_aggregates = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        if aggregate.distinct {
            return Ok(None);
        }
        match aggregate.function {
            AggregateFunction::Count => {}
            // Float-carried sums and averages only: exact decimal AVG needs
            // the generic i128-unit state, not this lane's f64 slots.
            AggregateFunction::Sum | AggregateFunction::Average
                if aggregate_uses_float(aggregate) => {}
            _ => return Ok(None),
        }
        let column = match &aggregate.expr {
            None => None,
            Some(expression) => match expression.column_index() {
                Some(column) => Some(column),
                None => return Ok(None),
            },
        };
        if let Some(column) = column {
            let typed = batch.column(column).and_then(ColumnVector::typed);
            if aggregate.function != AggregateFunction::Count && typed.is_none() {
                return Ok(None);
            }
            if aggregate.function != AggregateFunction::Count
                && typed.is_some_and(|(values, _)| values.number_at(0).is_none())
                && batch.row_count() > 0
            {
                return Ok(None);
            }
        }
        dict_aggregates.push(DictAggregate {
            function: aggregate.function,
            column,
        });
    }

    // Pass 1: per-column dictionary codes (0 = NULL), composed base-256 and
    // mapped to dense slots through a sentinel table. The representative row
    // of a slot exhibits every key column's original value.
    let mut column_dicts: Vec<Vec<Option<usize>>> =
        key_columns.iter().map(|_| vec![None]).collect();
    let selected = batch.visible_row_count();
    let mut rows_buffer = Vec::with_capacity(selected);
    let mut codes_buffer = Vec::with_capacity(selected);
    let composite_capacity = MAX_CODES.pow(u32::try_from(key_columns.len()).expect("<= 2 columns"));
    let mut slot_table = vec![u16::MAX; composite_capacity];
    let mut slot_rows: Vec<usize> = Vec::new();
    for row in batch.selection().selected_rows() {
        let mut composite = 0_usize;
        for ((_, keys, validity), dict) in key_columns.iter().zip(column_dicts.iter_mut()) {
            let views = keys.views();
            let heap = keys.heap();
            let code = if validity.is_valid(row) {
                let view = &views[row];
                let found = dict[1..].iter().position(|representative| {
                    representative.is_some_and(|existing| view.same_bytes(&views[existing], heap))
                });
                if let Some(index) = found {
                    index + 1
                } else {
                    if dict.len() > MAX_CODES - 1 {
                        return Ok(None);
                    }
                    dict.push(Some(row));
                    dict.len() - 1
                }
            } else {
                if dict[0].is_none() {
                    dict[0] = Some(row);
                }
                0
            };
            composite = composite * MAX_CODES + code;
        }
        let slot = if slot_table[composite] == u16::MAX {
            if slot_rows.len() >= MAX_SLOTS {
                return Ok(None);
            }
            let slot = u16::try_from(slot_rows.len()).expect("bounded slots");
            slot_table[composite] = slot;
            slot_rows.push(row);
            slot
        } else {
            slot_table[composite]
        };
        rows_buffer.push(row);
        codes_buffer.push(slot);
    }

    // Pass 2: per aggregate, one tight loop over (row, slot).
    let code_count = slot_rows.len();
    let mut states: Vec<Vec<AggregateState>> = (0..code_count)
        .map(|_| aggregates.iter().map(AggregateState::new).collect())
        .collect();
    for (aggregate_index, dict_aggregate) in dict_aggregates.iter().enumerate() {
        match dict_aggregate.function {
            AggregateFunction::Count => {
                let mut counts = vec![0_u64; code_count];
                match dict_aggregate.column {
                    None => {
                        for &code in &codes_buffer {
                            counts[usize::from(code)] += 1;
                        }
                    }
                    Some(column) => {
                        let validity = batch
                            .column(column)
                            .and_then(ColumnVector::typed)
                            .map(|(_, validity)| validity);
                        for (&row, &code) in rows_buffer.iter().zip(&codes_buffer) {
                            let non_null = match validity {
                                Some(validity) => validity.is_valid(row),
                                None => !matches!(
                                    batch.column(column).and_then(|c| c.value(row)),
                                    Some(Value::Null) | None
                                ),
                            };
                            if non_null {
                                counts[usize::from(code)] += 1;
                            }
                        }
                    }
                }
                for (code, count) in counts.iter().enumerate() {
                    states[code][aggregate_index].value = AggregateValue::Count(*count);
                }
            }
            AggregateFunction::Sum | AggregateFunction::Average => {
                let column = dict_aggregate.column.expect("validated column input");
                let (typed, validity) = batch
                    .column(column)
                    .and_then(ColumnVector::typed)
                    .expect("validated typed input");
                let mut sums = vec![0.0_f64; code_count];
                let mut counts = vec![0_u64; code_count];
                for (&row, &code) in rows_buffer.iter().zip(&codes_buffer) {
                    if validity.is_valid(row)
                        && let Some(number) = typed.number_at(row)
                    {
                        let slot = usize::from(code);
                        sums[slot] += number;
                        counts[slot] += 1;
                    }
                }
                for code in 0..code_count {
                    if !sums[code].is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                    states[code][aggregate_index].value = if dict_aggregate.function
                        == AggregateFunction::Sum
                    {
                        AggregateValue::Sum((counts[code] > 0).then(|| Value::float64(sums[code])))
                    } else {
                        AggregateValue::Average {
                            sum: sums[code],
                            count: counts[code],
                        }
                    };
                }
            }
            _ => unreachable!("filtered above"),
        }
    }

    // Finalize: original values from each slot's representative row,
    // normalized map keys — the general local builder's exact contract.
    let mut groups = HashMap::with_capacity(code_count * 2);
    for (slot, &row) in slot_rows.iter().enumerate() {
        let mut values = Vec::with_capacity(key_columns.len());
        for (vector, _, _) in &key_columns {
            values.push(
                vector
                    .value(row)
                    .cloned()
                    .ok_or(ExecError::InvalidBatch("dictionary key row out of range"))?,
            );
        }
        let key = values
            .iter()
            .cloned()
            .map(normalized_collation_value)
            .collect();
        groups.insert(
            key,
            AggregateGroup {
                values,
                states: std::mem::take(&mut states[slot]),
            },
        );
    }
    Ok(Some(groups))
}

fn build_local_direct_groups(
    batch: &RecordBatch,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    if let Some(groups) = build_local_dictionary_groups(batch, group_columns, aggregates)? {
        return Ok(groups);
    }
    let mut groups = Vec::<AggregateGroup>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let memory = MemoryTracker::new(usize::MAX);
    let batch_bytes = batch.estimated_bytes();
    for row in batch.selection().selected_rows() {
        let raw_hash = direct_group_hash(batch, row, group_columns)?;
        let existing = raw_index
            .get(&raw_hash)
            .copied()
            .filter(|index| {
                direct_group_matches_exact(&groups[*index].values, batch, row, group_columns)
            })
            .or_else(|| {
                groups.iter().position(|group| {
                    direct_group_matches(&group.values, batch, row, group_columns)
                })
            });
        let group_index = existing.unwrap_or_else(|| {
            let values = group_columns
                .iter()
                .map(|column| {
                    direct_group_value(batch, row, *column)
                        .expect("validated direct grouping column")
                        .clone()
                })
                .collect();
            let index = groups.len();
            groups.push(AggregateGroup {
                values,
                states: aggregates.iter().map(AggregateState::new).collect(),
            });
            index
        });
        raw_index.entry(raw_hash).or_insert(group_index);
        update_aggregate_states(
            batch,
            row,
            batch_bytes,
            aggregates,
            &mut groups[group_index].states,
            &memory,
        )?;
    }
    Ok(groups
        .into_iter()
        .map(|group| {
            let key = group
                .values
                .iter()
                .cloned()
                .map(normalized_collation_value)
                .collect();
            (key, group)
        })
        .collect())
}

fn build_local_expression_groups(
    batch: &RecordBatch,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    let memory = MemoryTracker::new(usize::MAX);
    let batch_bytes = batch.estimated_bytes();
    for row in batch.selection().selected_rows() {
        let values = group_by
            .iter()
            .map(|expression| expression.evaluate(batch, row))
            .collect::<Result<Vec<_>, _>>()?;
        let key = values
            .iter()
            .cloned()
            .map(normalized_collation_value)
            .collect::<Vec<_>>();
        let group = groups.entry(key).or_insert_with(|| AggregateGroup {
            values,
            states: aggregates.iter().map(AggregateState::new).collect(),
        });
        update_aggregate_states(
            batch,
            row,
            batch_bytes,
            aggregates,
            &mut group.states,
            &memory,
        )?;
    }
    Ok(groups)
}

fn finish_aggregate_groups(
    groups: impl ExactSizeIterator<Item = AggregateGroup>,
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for group in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
fn build_direct_column_aggregate(
    input: &mut PullOperator,
    mut first_batch: Option<RecordBatch>,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    // Single-int-column inputs with eligible lanes take the streaming
    // two-pass partitioned path (e13: 4.2-8.9x); ineligible aggregate
    // shapes fall through to the sequential scalar-index loop below.
    let mut pending = std::collections::VecDeque::new();
    if let Some(batch) = first_batch.take() {
        pending.push_back(batch);
    }
    if matches!(*group_columns, [_] | [_, _]) {
        let head = match pending.pop_front() {
            Some(batch) => Some(batch),
            None => input.next_batch(memory)?,
        };
        let Some(head) = head else {
            return Ok(MaterializedRows {
                rows: Vec::new(),
                position: 0,
            });
        };
        let typed_text = |column: usize| {
            head.column(column).map(ColumnVector::data_type) == Some(DataType::Utf8)
                && head
                    .column(column)
                    .and_then(ColumnVector::typed)
                    .is_some_and(|(typed, _)| matches!(typed, crate::batch::TypedValues::Utf8(_)))
        };
        let keys = match *group_columns {
            [column] => {
                let group_type = head.column(column).map(ColumnVector::data_type);
                let int_typed = group_type.is_some_and(|data_type| {
                    matches!(
                        data_type.storage_type(),
                        DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
                    )
                });
                if int_typed {
                    group_type.map(|group_type| TwoPassKeySource::Int { column, group_type })
                } else if typed_text(column) {
                    Some(TwoPassKeySource::Text { column })
                } else {
                    None
                }
            }
            [first, second] if typed_text(first) && typed_text(second) => {
                Some(TwoPassKeySource::TextPair { first, second })
            }
            _ => None,
        };
        let lanes = keys.and_then(|_| two_pass_lanes(aggregates, &head));
        if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
            let kinds = lanes.as_ref().map(|lanes| {
                lanes
                    .iter()
                    .map(|lane| match lane {
                        TwoPassLane::CountStar => "count*",
                        TwoPassLane::Float { .. } => "float",
                        TwoPassLane::Int { .. } => "int",
                        TwoPassLane::Exact { .. } => "exact",
                        TwoPassLane::DecimalUnits { .. } => "decimal-units",
                        TwoPassLane::Distinct { .. } => "distinct",
                        TwoPassLane::ExtremeDecimal { .. } => "extreme-decimal",
                    })
                    .collect::<Vec<_>>()
            });
            eprintln!("[agg] direct path: keys={} lanes={kinds:?}", keys.is_some());
        }
        if let (Some(keys), Some(lanes)) = (keys, lanes) {
            // Streaming scatter (phase-0 profile, 2026-08-02): retaining
            // RecordBatches cost ~118 bytes/row and forced the sequential
            // Value-hashmap fallback on real 20M-row inputs. Scattering
            // (key bits, lane bits) as batches arrive costs the exact
            // 8*(1+lanes)+1 bytes/row and never falls back.
            return build_streaming_two_pass_aggregate(
                input, head, keys, &lanes, aggregates, memory,
            );
        }
        pending.push_front(head);
    }
    let mut groups = Vec::<AggregateGroup>::new();
    let mut scalar_index = HashMap::<Value, usize>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let mut index_reserved = 0_usize;

    loop {
        let batch = if let Some(batch) = pending.pop_front() {
            batch
        } else if let Some(batch) = input.next_batch(memory)? {
            batch
        } else {
            break;
        };
        let batch_bytes = batch.estimated_bytes();
        let indexed = group_columns.len() == 1
            && batch.column(group_columns[0]).is_some_and(|column| {
                matches!(
                    column.data_type().storage_type(),
                    DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
                )
            });
        for row in batch.selection().selected_rows() {
            let raw_hash = (!indexed)
                .then(|| direct_group_hash(&batch, row, group_columns))
                .transpose()?;
            let existing = if indexed {
                let value = direct_group_value(&batch, row, group_columns[0])?;
                scalar_index.get(value).copied()
            } else {
                raw_index
                    .get(&raw_hash.expect("non-indexed groups have a raw hash"))
                    .copied()
                    .filter(|index| {
                        direct_group_matches_exact(
                            &groups[*index].values,
                            &batch,
                            row,
                            group_columns,
                        )
                    })
                    .or_else(|| {
                        groups.iter().position(|group| {
                            direct_group_matches(&group.values, &batch, row, group_columns)
                        })
                    })
            };
            let group_index = if let Some(index) = existing {
                index
            } else {
                let values = group_columns
                    .iter()
                    .map(|column| direct_group_value(&batch, row, *column).cloned())
                    .collect::<Result<Vec<_>, _>>()?;
                let bytes = estimated_row_payload_bytes(&values)
                    .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()));
                memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                reserve_vec_elements(&mut groups, 1, 64, memory)?;
                memory.reserve(bytes)?;
                let index = groups.len();
                groups.push(AggregateGroup {
                    values,
                    states: aggregates.iter().map(AggregateState::new).collect(),
                });
                if indexed {
                    index_reserved = index_reserved.saturating_add(reserve_hash_map_entries(
                        &mut scalar_index,
                        1,
                        size_of::<Value>()
                            .saturating_add(size_of::<usize>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        batch_bytes,
                        memory,
                    )?);
                    let key = direct_group_value(&batch, row, group_columns[0])?.clone();
                    memory.reserve(key.heap_bytes())?;
                    index_reserved = index_reserved.saturating_add(key.heap_bytes());
                    scalar_index.insert(key, index);
                }
                index
            };
            if let Some(raw_hash) = raw_hash
                && !raw_index.contains_key(&raw_hash)
            {
                index_reserved = index_reserved.saturating_add(reserve_hash_map_entries(
                    &mut raw_index,
                    1,
                    size_of::<u64>()
                        .saturating_add(size_of::<usize>())
                        .saturating_add(HASH_ENTRY_OVERHEAD),
                    batch_bytes,
                    memory,
                )?);
                raw_index.insert(raw_hash, group_index);
            }
            update_aggregate_states(
                &batch,
                row,
                batch_bytes,
                aggregates,
                &mut groups[group_index].states,
                memory,
            )?;
        }
    }

    drop(scalar_index);
    drop(raw_index);
    memory.release(index_reserved);
    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for group in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

/// Per-aggregate scatter payload for the two-pass partitioned aggregate.
#[derive(Clone, Copy)]
enum TwoPassLane {
    /// COUNT(*): every row counts; the lane carries nothing.
    CountStar,
    /// COUNT/SUM/AVG over a float or decimal column: an f64 rides the lane
    /// (matching the sequential path's f64 accumulation for these types).
    Float { column: usize },
    /// COUNT/SUM/AVG over an integer column: exact bits ride the lane and
    /// pass 2 takes the sequential path's exact integer branch.
    Int { column: usize, data_type: DataType },
    /// MIN/MAX over a plain int/uint/float/bool column: exact bits ride the
    /// lane so the retained Value stays exact.
    Exact { column: usize, data_type: DataType },
    /// SUM over a decimal column: i64 scaled units ride the lane and pass 2
    /// accumulates i128 exactly. f64 lanes drift past the 4-decimal
    /// canonical on 500k-row group sums (the Q4 mismatch, 2026-08-02).
    DecimalUnits {
        column: usize,
        scale: u8,
        float_output: bool,
    },
    /// COUNT(DISTINCT `int_col)`: raw key bits ride the lane and pass 2
    /// dedups through the typed i128 set (e16 — this was the shape that
    /// kept Q7 off every typed path).
    Distinct { column: usize, data_type: DataType },
    /// MIN/MAX over a decimal column: i64 scaled units ride the lane;
    /// pass 2 compares units and formats only on replacement.
    ExtremeDecimal { column: usize, scale: u8 },
}

/// Whether every aggregate fits a scatter lane, and which kind. `None`
/// keeps the query on the sequential direct path.
fn two_pass_lanes(
    aggregates: &[CompiledAggregate],
    batch: &RecordBatch,
) -> Option<Vec<TwoPassLane>> {
    if aggregates.len() > 7 {
        // One mask bit per lane plus the key bit.
        return None;
    }
    aggregates
        .iter()
        .map(|aggregate| {
            if aggregate.distinct {
                // COUNT(DISTINCT int_col) rides its own lane; any other
                // distinct shape keeps the query off the two-pass path.
                if aggregate.function != AggregateFunction::Count {
                    return None;
                }
                let column = aggregate.expr.as_ref()?.column_index()?;
                let storage = batch.column(column)?.data_type().storage_type();
                return matches!(storage, DataType::Int64 | DataType::UInt64).then_some(
                    TwoPassLane::Distinct {
                        column,
                        data_type: storage,
                    },
                );
            }
            let Some(expr) = &aggregate.expr else {
                return matches!(aggregate.function, AggregateFunction::Count)
                    .then_some(TwoPassLane::CountStar);
            };
            let column = expr.column_index()?;
            let storage = batch.column(column)?.data_type().storage_type();
            match aggregate.function {
                AggregateFunction::Count | AggregateFunction::Sum | AggregateFunction::Average => {
                    match storage {
                        // Integer inputs stay on the exact integer branch;
                        // the generic state accumulates exact decimal
                        // averages from integer values too.
                        DataType::Int64 | DataType::UInt64 => Some(TwoPassLane::Int {
                            column,
                            data_type: storage,
                        }),
                        DataType::Float64 => Some(TwoPassLane::Float { column }),
                        _ => match batch.column(column)?.data_type() {
                            // SUM and exact AVG both ride the packed-units
                            // lane; the per-row apply branches on the
                            // aggregate function.
                            DataType::Decimal { scale, .. }
                                if aggregate.function == AggregateFunction::Sum
                                    || decimal_average_scale(aggregate).is_some() =>
                            {
                                Some(TwoPassLane::DecimalUnits {
                                    column,
                                    scale,
                                    float_output: aggregate_uses_float(aggregate),
                                })
                            }
                            DataType::Decimal { .. } => Some(TwoPassLane::Float { column }),
                            _ => None,
                        },
                    }
                }
                AggregateFunction::Minimum | AggregateFunction::Maximum => {
                    if let DataType::Decimal { scale, .. } = batch.column(column)?.data_type() {
                        return Some(TwoPassLane::ExtremeDecimal { column, scale });
                    }
                    matches!(
                        storage,
                        DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                    )
                    .then_some(TwoPassLane::Exact {
                        column,
                        data_type: storage,
                    })
                }
                // ANY_VALUE retains one exact value, exactly like MIN/MAX.
                AggregateFunction::AnyValue => matches!(
                    storage,
                    DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                )
                .then_some(TwoPassLane::Exact {
                    column,
                    data_type: storage,
                }),
                // Welford consumes one f64 per row, so the float lane
                // carries everything the moments need. Riding a lane is the
                // point: an aggregate with no lane drops its whole query
                // onto the per-row Value path, which is what costs Q7 its
                // margin against ClickHouse (issue #6).
                AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. } => matches!(
                    storage,
                    DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                )
                .then_some(TwoPassLane::Float { column }),
                // The bit folds need exact integer bits, which the int lane
                // already carries.
                AggregateFunction::BitAnd
                | AggregateFunction::BitOr
                | AggregateFunction::BitXor => {
                    matches!(storage, DataType::Int64 | DataType::UInt64).then_some(
                        TwoPassLane::Int {
                            column,
                            data_type: storage,
                        },
                    )
                }
                AggregateFunction::GroupConcat
                | AggregateFunction::JsonArrayAgg
                | AggregateFunction::JsonObjectAgg => None,
            }
        })
        .collect()
}

/// One worker's scatter output for one partition: struct-of-arrays rows.
#[derive(Default)]
struct TwoPassBucket {
    /// Group key bits per row.
    keys: Vec<u64>,
    /// Bit 7: key is NULL; bits 0..lanes: lane value is NULL.
    masks: Vec<u8>,
    /// `lanes.len() == keys.len() * lane_count`, row-major.
    lanes: Vec<u64>,
}

fn two_pass_key_bits(value: &Value) -> Option<(u64, bool)> {
    match value {
        Value::Null => Some((0, true)),
        Value::Int64(value) => Some((u64::from_ne_bytes(value.to_ne_bytes()), false)),
        Value::UInt64(value) => Some((*value, false)),
        Value::Float64(value) => Some((value.get().to_bits(), false)),
        Value::Boolean(value) => Some((u64::from(*value), false)),
        Value::Utf8(_) | Value::Binary(_) => None,
    }
}

fn two_pass_key_value(bits: u64, null: bool, data_type: DataType) -> Value {
    if null {
        return Value::Null;
    }
    match data_type.storage_type() {
        DataType::Int64 => Value::Int64(i64::from_ne_bytes(bits.to_ne_bytes())),
        DataType::UInt64 => Value::UInt64(bits),
        DataType::Float64 => Value::float64(f64::from_bits(bits)),
        DataType::Boolean => Value::Boolean(bits != 0),
        _ => Value::Null,
    }
}

/// How the streaming two-pass extracts group-key bits per row.
#[derive(Clone, Copy)]
enum TwoPassKeySource {
    /// One int-typed column: key bits are the value's bit pattern.
    Int { column: usize, group_type: DataType },
    /// One string column: key bits are interned string ids (bit 7 of the
    /// mask carries NULL, matching the int scheme).
    Text { column: usize },
    /// Two string columns: `(id_a + 1) << 32 | (id_b + 1)`, with 0 as the
    /// per-column NULL sentinel so `(NULL, x)`, `(x, NULL)` and
    /// `(NULL, NULL)` stay distinct groups.
    TextPair { first: usize, second: usize },
    /// Up to two DATE-PART expressions over temporal columns (the Q5
    /// shape, GROUP BY YEAR(d), MONTH(d)): each part value is bounded
    /// (year < 10^4, others < 60), so `(v + 1)` packs into 20 bits per
    /// part with 0 as the per-part NULL sentinel.
    DateParts {
        parts: [Option<(DatePart, usize)>; 2],
    },
}

/// Streaming two-pass partitioned aggregation for one int-typed group
/// column (experiments/RESULTS.md e13/e15 and the 2026-08-02 phase-0
/// profile). Pass 1 scatters (key bits, lane bits) into partition buckets
/// as batches arrive — no `RecordBatch` is retained. Pass 2 folds buckets
/// into per-partition typed hashmaps in parallel whenever the scatter
/// window fills, so memory is bounded by the group states plus one flush
/// window regardless of input size.
#[allow(clippy::too_many_lines)]
fn build_streaming_two_pass_aggregate(
    input: &mut PullOperator,
    first: RecordBatch,
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let partitions = std::thread::available_parallelism().map_or(8, usize::from);
    let lane_count = lanes.len();
    let scatter_row_bytes = size_of::<u64>() * (1 + lane_count) + 1;
    // Flush the scatter window at a quarter of the budget (bounded to
    // 1-64 MB) so the scan always keeps its transient headroom.
    let flush_bytes = (memory.limit() / 4).clamp(1 << 20, 64 << 20);
    let scan_floor = input.scan_transient_floor().saturating_mul(2);
    let mut buckets: Vec<TwoPassBucket> =
        (0..partitions).map(|_| TwoPassBucket::default()).collect();
    let mut maps: Vec<GroupKeyMap> = (0..partitions).map(|_| GroupKeyMap::default()).collect();
    let mut bucket_reserved = 0_usize;
    let mut group_reserved = 0_usize;
    let mut flushes = 0_u32;
    let mut intern = matches!(
        keys,
        TwoPassKeySource::Text { .. } | TwoPassKeySource::TextPair { .. }
    )
    .then(StringIntern::default);
    // Distinct lanes stay on the classic path: dense per-worker partials
    // would replicate each group's distinct set per thread and pay a
    // drain-and-reinsert merge that costs more than the scatter it saves
    // (n4 585ms -> 768ms when measured on 2026-08-02).
    let mut dense = lanes
        .iter()
        .all(|lane| !matches!(lane, TwoPassLane::Distinct { .. }))
        .then(|| dense_slot_count(keys))
        .flatten()
        .map(|slots| {
            let mut table: DenseGroupSlots = Vec::new();
            table.resize_with(slots, || None);
            table
        });
    if let Some(slots) = &dense {
        let slab = slots
            .len()
            .saturating_mul(size_of::<Option<Vec<AggregateState>>>());
        memory.reserve(slab)?;
        group_reserved = group_reserved.saturating_add(slab);
    }

    let mut window: Vec<(RecordBatch, Vec<Vec<u64>>)> = Vec::new();
    let mut window_reserved = 0_usize;
    let mut window_rows = 0_usize;
    let mut batch = Some(first);
    loop {
        let Some(current) = batch.take() else {
            break;
        };
        // String sources prepare their (tiny, per-distinct-value) dictionary
        // translations serially, then scatter rows in parallel from the
        // read-only tables; batches whose strings decoded without codes
        // fall back to the serial scatter below.
        let prepared = match (keys, &mut intern) {
            (TwoPassKeySource::Text { column }, Some(intern)) => {
                prepare_text_translations(&current, &[column], intern, memory)?
            }
            (TwoPassKeySource::TextPair { first, second }, Some(intern)) => {
                prepare_text_translations(&current, &[first, second], intern, memory)?
            }
            _ => Some(Vec::new()),
        };
        if let Some(translations) = prepared {
            let rows = current.visible_row_count();
            let need = rows
                .saturating_mul(scatter_row_bytes)
                .saturating_add(current.estimated_bytes());
            if let Err(error) = memory.reserve(need) {
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                )?;
                window_rows = 0;
                flushes += 1;
                if memory.reserve(need).is_err() {
                    return Err(error);
                }
            }
            window_reserved = window_reserved.saturating_add(need);
            window_rows += rows;
            window.push((current, translations));
            if window_rows.saturating_mul(scatter_row_bytes) >= flush_bytes
                || (scan_floor > 0 && memory.remaining() < scan_floor)
            {
                drain_two_pass_window(
                    &mut window,
                    keys,
                    lanes,
                    aggregates,
                    partitions,
                    &mut maps,
                    &mut dense,
                    intern.as_ref().map_or(0, |intern| intern.values.len()),
                    memory,
                    &mut group_reserved,
                    &mut window_reserved,
                )?;
                window_rows = 0;
                flushes += 1;
            }
            batch = input.next_batch(memory)?;
            continue;
        }
        let rows = current.visible_row_count();
        let bytes = rows.saturating_mul(scatter_row_bytes);
        if let Err(error) = memory.reserve(bytes) {
            // Free the scatter window and retry once; a second failure
            // means the group states themselves exceed the budget.
            two_pass_flush(
                &mut buckets,
                &mut maps,
                lanes,
                aggregates,
                memory,
                &mut group_reserved,
            )?;
            memory.release(bucket_reserved);
            bucket_reserved = 0;
            flushes += 1;
            match memory.reserve(bytes) {
                Ok(()) => {}
                Err(_) => return Err(error),
            }
        }
        bucket_reserved = bucket_reserved.saturating_add(bytes);
        match (keys, &mut intern) {
            (TwoPassKeySource::Text { column }, Some(intern)) => two_pass_scatter_strings(
                &current,
                column,
                lanes,
                partitions,
                &mut buckets,
                intern,
                memory,
            )?,
            (TwoPassKeySource::TextPair { first, second }, Some(intern)) => {
                two_pass_scatter_string_pair(
                    &current,
                    first,
                    second,
                    lanes,
                    partitions,
                    &mut buckets,
                    intern,
                    memory,
                )?;
            }
            (TwoPassKeySource::Int { column, .. }, _) => {
                two_pass_scatter_batch(&current, column, lanes, partitions, &mut buckets)?;
            }
            (TwoPassKeySource::DateParts { parts }, _) => {
                two_pass_scatter_date_parts(&current, parts, lanes, partitions, &mut buckets)?;
            }
            _ => unreachable!("intern presence follows the key source"),
        }
        drop(current);
        if bucket_reserved >= flush_bytes || (scan_floor > 0 && memory.remaining() < scan_floor) {
            two_pass_flush(
                &mut buckets,
                &mut maps,
                lanes,
                aggregates,
                memory,
                &mut group_reserved,
            )?;
            memory.release(bucket_reserved);
            bucket_reserved = 0;
            flushes += 1;
        }
        batch = input.next_batch(memory)?;
    }
    drain_two_pass_window(
        &mut window,
        keys,
        lanes,
        aggregates,
        partitions,
        &mut maps,
        &mut dense,
        intern.as_ref().map_or(0, |intern| intern.values.len()),
        memory,
        &mut group_reserved,
        &mut window_reserved,
    )?;
    two_pass_flush(
        &mut buckets,
        &mut maps,
        lanes,
        aggregates,
        memory,
        &mut group_reserved,
    )?;
    memory.release(bucket_reserved);
    if let Some(slots) = dense.take() {
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            &mut maps,
            memory,
            &mut group_reserved,
        )?;
    }
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        let groups: usize = maps.iter().map(HashMap::len).sum();
        eprintln!(
            "[agg] streaming two-pass: {groups} groups, {} flushes",
            flushes + 1
        );
    }

    // Finalize each partition in parallel; ORDER BY above owns ordering.
    let finalized = maps
        .into_par_iter()
        .map(|map| -> Result<(Vec<Vec<Value>>, usize), ExecError> {
            let interned = |id: u64| {
                Value::Utf8(
                    intern
                        .as_ref()
                        .expect("text keys carry an intern table")
                        .values[usize::try_from(id).expect("intern id fits usize")]
                    .clone(),
                )
            };
            let mut rows = Vec::with_capacity(map.len());
            let mut payload = 0_usize;
            for ((bits, null), states) in map {
                let mut row = Vec::with_capacity(2 + states.len());
                match keys {
                    TwoPassKeySource::Int { group_type, .. } => {
                        row.push(two_pass_key_value(bits, null, group_type));
                    }
                    TwoPassKeySource::Text { .. } => {
                        row.push(if null { Value::Null } else { interned(bits) });
                    }
                    TwoPassKeySource::TextPair { .. } => {
                        for id in [bits >> 32, bits & 0xFFFF_FFFF] {
                            row.push(if id == 0 {
                                Value::Null
                            } else {
                                interned(id - 1)
                            });
                        }
                    }
                    TwoPassKeySource::DateParts { parts } => {
                        let count = parts.iter().flatten().count();
                        for index in 0..count {
                            let shift = 20 * (count - 1 - index);
                            let id = (bits >> shift) & 0xF_FFFF;
                            row.push(if id == 0 {
                                Value::Null
                            } else {
                                Value::UInt64(id - 1)
                            });
                        }
                    }
                }
                for state in states {
                    row.push(state.finish(memory)?);
                }
                let bytes = estimated_row_payload_bytes(&row);
                memory.reserve(bytes)?;
                payload = payload.saturating_add(bytes);
                rows.push(row);
            }
            Ok((rows, payload))
        })
        .collect::<Result<Vec<_>, _>>();
    memory.release(group_reserved);
    let finalized = finalized?;
    let mut rows = Vec::new();
    for (partition_rows, _) in finalized {
        rows.extend(partition_rows);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

/// Pass 1 for one batch: extract (key bits, lane bits, null mask) per
/// selected row into the partition buckets. Reservation is the caller\'s.
fn two_pass_scatter_batch(
    batch: &RecordBatch,
    group_column: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let group_values = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    for row in batch.selection().selected_rows() {
        let value = group_values.value(row).ok_or(ExecError::InvalidBatch(
            "grouping row is outside the input batch",
        ))?;
        let (key_bits, key_null) = two_pass_key_bits(value)
            .ok_or(ExecError::InvalidBatch("two-pass key is not scalar"))?;
        scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
    }
    Ok(())
}

/// String-keyed scatter: group keys are interned string ids — dictionary
/// codes translate per batch (one intern per distinct entry), degraded
/// plain-text chunks intern per row. No Value cell is ever built.
fn two_pass_scatter_strings(
    batch: &RecordBatch,
    group_column: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let vector = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
        return Err(ExecError::InvalidBatch(
            "string two-pass key column lost its typed projection",
        ));
    };
    if let Some((codes, dict_values)) = strings.dictionary() {
        let translation = dict_values
            .iter()
            .map(|value| intern.intern(value.as_bytes(), memory))
            .collect::<Result<Vec<_>, _>>()?;
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                translation[usize::try_from(codes[row]).expect("dict code fits usize")]
            };
            scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
        }
    } else {
        let (views, heap) = (strings.views(), strings.heap());
        for row in batch.selection().selected_rows() {
            let key_null = !validity.is_valid(row);
            let key_bits = if key_null {
                0
            } else {
                views[row].with_bytes(heap, |bytes| intern.intern(bytes, memory))?
            };
            scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
        }
    }
    Ok(())
}

/// One string column's per-batch key extractor: dictionary translation
/// when codes survive, per-row view interning otherwise.
enum StringKeyReader<'a> {
    Dict {
        codes: &'a [u32],
        translation: Vec<u64>,
    },
    Plain {
        views: &'a [crate::array::StrView],
        heap: &'a [u8],
    },
}

impl StringKeyReader<'_> {
    fn read(
        &self,
        row: usize,
        intern: &mut StringIntern,
        memory: &MemoryTracker,
    ) -> Result<u64, ExecError> {
        match self {
            Self::Dict { codes, translation } => {
                Ok(translation[usize::try_from(codes[row]).expect("dict code fits usize")])
            }
            Self::Plain { views, heap } => {
                views[row].with_bytes(heap, |bytes| intern.intern(bytes, memory))
            }
        }
    }
}

fn string_key_reader<'a>(
    batch: &'a RecordBatch,
    column: usize,
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(StringKeyReader<'a>, &'a crate::array::ValidityMask), ExecError> {
    let vector = batch.column(column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
        return Err(ExecError::InvalidBatch(
            "string two-pass key column lost its typed projection",
        ));
    };
    let reader = if let Some((codes, dict_values)) = strings.dictionary() {
        StringKeyReader::Dict {
            codes,
            translation: dict_values
                .iter()
                .map(|value| intern.intern(value.as_bytes(), memory))
                .collect::<Result<Vec<_>, _>>()?,
        }
    } else {
        StringKeyReader::Plain {
            views: strings.views(),
            heap: strings.heap(),
        }
    };
    Ok((reader, validity))
}

/// Resolves this batch's dictionary translations against the global
/// intern table — the only step that needs `&mut intern`, and it costs one
/// intern per DISTINCT value. Returns `None` when any key column decoded
/// without codes (plain views need per-row interning, so those batches
/// stay on the serial path).
fn prepare_text_translations(
    batch: &RecordBatch,
    columns: &[usize],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<Option<Vec<Vec<u64>>>, ExecError> {
    let mut prepared = Vec::with_capacity(columns.len());
    for column in columns {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), _)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((_, dict_values)) = strings.dictionary() else {
            return Ok(None);
        };
        prepared.push(
            dict_values
                .iter()
                .map(|value| intern.intern(value.as_bytes(), memory))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(Some(prepared))
}

/// Scatters string keys from prepared (read-only) translations: no intern
/// access, so windows of batches scatter in parallel.
fn two_pass_scatter_text_prepared(
    batch: &RecordBatch,
    columns: &[usize],
    translations: &[Vec<u64>],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    let mut readers = Vec::with_capacity(columns.len());
    for (column, translation) in columns.iter().zip(translations) {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, _)) = strings.dictionary() else {
            return Err(ExecError::InvalidBatch(
                "prepared text scatter requires dictionary codes",
            ));
        };
        readers.push((codes, validity, translation));
    }
    let pair = readers.len() == 2;
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            let id = if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                let interned = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                if pair { interned + 1 } else { interned }
            } else {
                if !pair {
                    key_null = true;
                }
                0
            };
            key_bits = if pair { (key_bits << 32) | id } else { id };
        }
        scatter_two_pass_row(batch, row, key_bits, key_null, lanes, partitions, buckets);
    }
    Ok(())
}

/// Two string group columns: ids pack as `(a+1) << 32 | (b+1)` with 0 as
/// the per-column NULL sentinel (mask bit 7 stays clear).
#[allow(clippy::too_many_arguments)]
fn two_pass_scatter_string_pair(
    batch: &RecordBatch,
    first: usize,
    second: usize,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
    intern: &mut StringIntern,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let (first_reader, first_validity) = string_key_reader(batch, first, intern, memory)?;
    let (second_reader, second_validity) = string_key_reader(batch, second, intern, memory)?;
    for row in batch.selection().selected_rows() {
        let first_id = if first_validity.is_valid(row) {
            first_reader.read(row, intern, memory)? + 1
        } else {
            0
        };
        let second_id = if second_validity.is_valid(row) {
            second_reader.read(row, intern, memory)? + 1
        } else {
            0
        };
        let key_bits = (first_id << 32) | second_id;
        scatter_two_pass_row(batch, row, key_bits, false, lanes, partitions, buckets);
    }
    Ok(())
}

/// Up to two bounded date-part expressions as the group key: values come
/// straight from packed temporal units (no Value cells, no text).
fn two_pass_scatter_date_parts(
    batch: &RecordBatch,
    parts: [Option<(DatePart, usize)>; 2],
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) -> Result<(), ExecError> {
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        for (part, column) in parts.iter().flatten() {
            let id = match crate::expression::evaluate_units_date_part(batch, *column, row, *part) {
                Some(Ok(Value::UInt64(value))) => value + 1,
                Some(Ok(Value::Null)) => 0,
                Some(Err(error)) => return Err(error),
                _ => {
                    return Err(ExecError::InvalidBatch(
                        "date-part group key column lost its packed units",
                    ));
                }
            };
            debug_assert!(id < 1 << 20, "date part value fits 20 bits");
            key_bits = (key_bits << 20) | id;
        }
        scatter_two_pass_row(batch, row, key_bits, false, lanes, partitions, buckets);
    }
    Ok(())
}

#[inline]
/// Extracts one lane's scatter bits for one row; `None` is the NULL mark.
/// Shared by the scatter path (which buffers the bits) and the dense direct
/// path (which applies them immediately).
fn two_pass_lane_bits(batch: &RecordBatch, row: usize, lane: &TwoPassLane) -> Option<u64> {
    match lane {
        TwoPassLane::CountStar => Some(0),
        TwoPassLane::Float { column } => batch
            .column(*column)
            .and_then(|column| {
                let (typed, validity) = column.typed()?;
                validity
                    .is_valid(row)
                    .then(|| typed.number_at(row))
                    .flatten()
            })
            .or_else(|| {
                batch
                    .column(*column)
                    .and_then(|column| match column.value(row) {
                        Some(Value::Null) | None => None,
                        Some(value) => mysql_f64(value).ok(),
                    })
            })
            .map(f64::to_bits),
        TwoPassLane::DecimalUnits { column, .. } | TwoPassLane::ExtremeDecimal { column, .. } => {
            batch
                .column(*column)
                .and_then(|column| {
                    let (typed, validity) = column.typed()?;
                    validity
                        .is_valid(row)
                        .then(|| typed.units_at(row))
                        .flatten()
                })
                .and_then(|units| i64::try_from(units).ok())
                .map(|units| u64::from_ne_bytes(units.to_ne_bytes()))
        }
        TwoPassLane::Int { column, .. }
        | TwoPassLane::Exact { column, .. }
        | TwoPassLane::Distinct { column, .. } => {
            match batch.column(*column).and_then(|column| column.value(row)) {
                Some(Value::Int64(value)) => Some(u64::from_ne_bytes(value.to_ne_bytes())),
                Some(Value::UInt64(value)) => Some(*value),
                Some(Value::Float64(value)) => Some(value.get().to_bits()),
                Some(Value::Boolean(value)) => Some(u64::from(*value)),
                _ => None,
            }
        }
    }
}

fn scatter_two_pass_row(
    batch: &RecordBatch,
    row: usize,
    key_bits: u64,
    key_null: bool,
    lanes: &[TwoPassLane],
    partitions: usize,
    buckets: &mut [TwoPassBucket],
) {
    let lane_count = lanes.len();
    {
        let mut mask = u8::from(key_null) << 7;
        let bucket = &mut buckets[usize::try_from(
            crate::batch::mix64(key_bits ^ u64::from(key_null)) % partitions as u64,
        )
        .expect("partition index fits usize")];
        let lane_base = bucket.lanes.len();
        bucket.lanes.resize(lane_base + lane_count, 0);
        for (lane_index, lane) in lanes.iter().enumerate() {
            match two_pass_lane_bits(batch, row, lane) {
                Some(bits) => bucket.lanes[lane_base + lane_index] = bits,
                None => mask |= 1 << lane_index,
            }
        }
        bucket.keys.push(key_bits);
        bucket.masks.push(mask);
    }
}

/// Global string-key intern table for string-keyed two-pass grouping:
/// dictionary code spaces are per chunk, so keys unify through this table.
#[derive(Default)]
struct StringIntern {
    index: HashMap<Vec<u8>, u64>,
    values: Vec<String>,
}

impl StringIntern {
    fn intern(&mut self, bytes: &[u8], memory: &MemoryTracker) -> Result<u64, ExecError> {
        // Group keys unify under the same case-insensitive rule the
        // sequential path applies (compare_utf8_mysql): fold to lowercase
        // for identity, keep the first-seen spelling for output — exactly
        // what MySQL's ci collations return for GROUP BY.
        let value = std::str::from_utf8(bytes)
            .map_err(|_| ExecError::InvalidBatch("string group key is not UTF-8"))?;
        let folded: Vec<u8> = if value.is_ascii() {
            bytes.to_ascii_lowercase()
        } else {
            value
                .chars()
                .flat_map(char::to_lowercase)
                .collect::<String>()
                .into_bytes()
        };
        if let Some(id) = self.index.get(&folded) {
            return Ok(*id);
        }
        let id = u64::try_from(self.values.len()).expect("intern ids fit u64");
        memory.reserve(
            bytes
                .len()
                .saturating_mul(3)
                .saturating_add(HASH_ENTRY_OVERHEAD)
                .saturating_add(size_of::<String>() + size_of::<u64>()),
        )?;
        self.index.insert(folded, id);
        self.values.push(value.to_owned());
        Ok(id)
    }
}

/// Pass 2: fold every partition\'s scattered rows into its typed group
/// map, in parallel, then clear the buckets (keeping capacity).
/// Scatters a bounded window of batches in parallel (one bucket set per
/// batch — no cross-worker sharing) and folds every set in one pass-2
/// flush. Only int-keyed sources scatter in parallel: string sources
/// share the intern table and stay on the serial path.
#[allow(clippy::too_many_arguments)]
fn drain_two_pass_window(
    window: &mut Vec<(RecordBatch, Vec<Vec<u64>>)>,
    keys: TwoPassKeySource,
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    dense: &mut Option<DenseGroupSlots>,
    intern_len: usize,
    memory: &MemoryTracker,
    group_reserved: &mut usize,
    window_reserved: &mut usize,
) -> Result<(), ExecError> {
    if window.is_empty() {
        return Ok(());
    }
    if let Some(slots) = dense.as_mut() {
        if dense_in_bounds(keys, intern_len) {
            let columns: &[usize] = match keys {
                TwoPassKeySource::Text { column } => &[column],
                TwoPassKeySource::TextPair { first, second } => &[first, second],
                _ => unreachable!("dense slots are text-keyed"),
            };
            // One partial per rayon worker (fold), merged pairwise
            // (reduce): batches of the window aggregate in parallel with
            // no per-row buffering and no hashing. Transient partials are
            // bounded by worker count x slot table, under the scatter
            // window's own reservation.
            let slot_count = slots.len();
            let folded = window
                .par_iter()
                .try_fold(
                    || vec![None; slot_count],
                    |mut acc, (batch, translations)| {
                        two_pass_dense_batch(
                            batch,
                            keys,
                            columns,
                            translations,
                            lanes,
                            aggregates,
                            &mut acc,
                            memory,
                        )?;
                        Ok(acc)
                    },
                )
                .try_reduce(
                    || vec![None; slot_count],
                    |left, right| merge_dense_slots(left, right, aggregates, memory),
                )?;
            let merged = merge_dense_slots(std::mem::take(slots), folded, aggregates, memory)?;
            *slots = merged;
            window.clear();
            memory.release(*window_reserved);
            *window_reserved = 0;
            return Ok(());
        }
        // The intern table outgrew the dense domain: unify what the dense
        // slots hold into the partition maps and continue on the classic
        // scatter path for the rest of the stream.
        let slots = dense.take().expect("checked above");
        fold_dense_into_maps(
            slots,
            keys,
            aggregates,
            partitions,
            maps,
            memory,
            group_reserved,
        )?;
    }
    let mut sets = window
        .par_iter()
        .map(
            |(batch, translations)| -> Result<Vec<TwoPassBucket>, ExecError> {
                let mut buckets: Vec<TwoPassBucket> =
                    (0..partitions).map(|_| TwoPassBucket::default()).collect();
                match keys {
                    TwoPassKeySource::Int { column, .. } => {
                        two_pass_scatter_batch(batch, column, lanes, partitions, &mut buckets)?;
                    }
                    TwoPassKeySource::DateParts { parts } => {
                        two_pass_scatter_date_parts(batch, parts, lanes, partitions, &mut buckets)?;
                    }
                    TwoPassKeySource::Text { column } => {
                        two_pass_scatter_text_prepared(
                            batch,
                            &[column],
                            translations,
                            lanes,
                            partitions,
                            &mut buckets,
                        )?;
                    }
                    TwoPassKeySource::TextPair { first, second } => {
                        two_pass_scatter_text_prepared(
                            batch,
                            &[first, second],
                            translations,
                            lanes,
                            partitions,
                            &mut buckets,
                        )?;
                    }
                }
                Ok(buckets)
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    window.clear();
    let outcome = two_pass_flush_sets(&mut sets, maps, lanes, aggregates, memory, group_reserved);
    memory.release(*window_reserved);
    *window_reserved = 0;
    outcome
}

fn two_pass_flush(
    buckets: &mut [TwoPassBucket],
    maps: &mut [GroupKeyMap],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let set: Vec<TwoPassBucket> = buckets.iter_mut().map(std::mem::take).collect();
    let mut sets = [set];
    let outcome = two_pass_flush_sets(&mut sets, maps, lanes, aggregates, memory, group_reserved);
    let [set] = sets;
    for (destination, bucket) in buckets.iter_mut().zip(set) {
        *destination = bucket;
    }
    outcome
}

/// Applies one lane's scattered bits to one aggregate state. Shared by
/// pass-2 flush (bits re-read from buckets) and the dense direct path
/// (bits applied straight from the batch).
fn apply_two_pass_lane(
    state: &mut AggregateState,
    lane: &TwoPassLane,
    aggregate: &CompiledAggregate,
    bits: u64,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    match lane {
        TwoPassLane::CountStar => state.update(aggregate, &Value::UInt64(1), memory),
        TwoPassLane::DecimalUnits {
            scale,
            float_output,
            ..
        } => {
            let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
            if let Some(result_scale) = decimal_average_scale(aggregate) {
                let rescaled = (*scale <= result_scale)
                    .then(|| decimal_units_from_int(units, result_scale - *scale))
                    .flatten()
                    .ok_or(ExecError::NumericOverflow)?;
                return state.update_decimal_average_units(rescaled, result_scale);
            }
            state.update_decimal_sum_units(units, *scale, *float_output)
        }
        TwoPassLane::ExtremeDecimal { scale, .. } => {
            let units = i128::from(i64::from_ne_bytes(bits.to_ne_bytes()));
            state.update_extreme_units(
                aggregate,
                units,
                || Some(pintail_types::format_decimal_scaled(units, *scale)),
                memory,
            )
        }
        TwoPassLane::Distinct { data_type, .. } => {
            let key = if *data_type == DataType::Int64 {
                i128::from(i64::from_ne_bytes(bits.to_ne_bytes()))
            } else {
                i128::from(bits)
            };
            state.update_distinct_count_int(key, memory)
        }
        TwoPassLane::Float { .. } => {
            let number = f64::from_bits(bits);
            state.update_with_number(aggregate, &Value::float64(number), Some(number), memory)
        }
        TwoPassLane::Int { data_type, .. } => {
            let value = two_pass_key_value(bits, false, *data_type);
            // number=None keeps integer sums on the exact integer branch,
            // as sequential does.
            state.update_with_number(aggregate, &value, None, memory)
        }
        TwoPassLane::Exact { data_type, .. } => {
            let value = two_pass_key_value(bits, false, *data_type);
            let number = match &value {
                Value::Int64(v) =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(*v as f64)
                }
                Value::UInt64(v) =>
                {
                    #[allow(clippy::cast_precision_loss)]
                    Some(*v as f64)
                }
                Value::Float64(v) => Some(v.get()),
                _ => None,
            };
            state.update_with_number(aggregate, &value, number, memory)
        }
    }
}

/// Dense slot table for small text-keyed group domains: intern ids are
/// dense small integers, so the whole scatter/flush round trip (buffer 17
/// bytes per row, re-read, hash-probe) collapses into direct indexing.
/// Slot 0 is the NULL group for single-column keys; pairs pack their
/// NULL-encoded side ids directly.
type DenseGroupSlots = Vec<Option<Vec<AggregateState>>>;

/// Single text column: intern ids 0..=1023 map to slots 1..=1024.
const DENSE_TEXT_CAP: usize = 1024;
/// Text pair: side ids are (intern id + 1) with 0 as NULL, kept < 65.
const DENSE_PAIR_SIDE: usize = 65;

fn dense_slot_count(keys: TwoPassKeySource) -> Option<usize> {
    match keys {
        TwoPassKeySource::Text { .. } => Some(DENSE_TEXT_CAP + 1),
        TwoPassKeySource::TextPair { .. } => Some(DENSE_PAIR_SIDE * DENSE_PAIR_SIDE),
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => None,
    }
}

/// Whether every sentinel the current intern table can produce still fits
/// the dense slots.
fn dense_in_bounds(keys: TwoPassKeySource, intern_len: usize) -> bool {
    match keys {
        TwoPassKeySource::Text { .. } => intern_len <= DENSE_TEXT_CAP,
        TwoPassKeySource::TextPair { .. } => intern_len + 1 < DENSE_PAIR_SIDE,
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => false,
    }
}

fn dense_slot_index(keys: TwoPassKeySource, key_bits: u64, key_null: bool) -> usize {
    match keys {
        TwoPassKeySource::Text { .. } => {
            if key_null {
                0
            } else {
                usize::try_from(key_bits).expect("intern id fits usize") + 1
            }
        }
        TwoPassKeySource::TextPair { .. } => {
            let first = usize::try_from(key_bits >> 32).expect("side id fits usize");
            let second = usize::try_from(key_bits & 0xFFFF_FFFF).expect("side id fits usize");
            first * DENSE_PAIR_SIDE + second
        }
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => {
            unreachable!("dense slots are text-keyed")
        }
    }
}

/// Inverse of [`dense_slot_index`]: the map key the classic path would use.
fn dense_slot_sentinel(keys: TwoPassKeySource, index: usize) -> (u64, bool) {
    match keys {
        TwoPassKeySource::Text { .. } => {
            if index == 0 {
                (0, true)
            } else {
                (
                    u64::try_from(index - 1).expect("slot index fits u64"),
                    false,
                )
            }
        }
        TwoPassKeySource::TextPair { .. } => {
            let first = u64::try_from(index / DENSE_PAIR_SIDE).expect("slot index fits u64");
            let second = u64::try_from(index % DENSE_PAIR_SIDE).expect("slot index fits u64");
            ((first << 32) | second, false)
        }
        TwoPassKeySource::Int { .. } | TwoPassKeySource::DateParts { .. } => {
            unreachable!("dense slots are text-keyed")
        }
    }
}

/// Dense pass over one batch: same key readers as
/// [`two_pass_scatter_text_prepared`], same lane extraction and state
/// updates as scatter + flush — minus the buffering between them.
#[allow(clippy::too_many_arguments)]
fn two_pass_dense_batch(
    batch: &RecordBatch,
    keys: TwoPassKeySource,
    columns: &[usize],
    translations: &[Vec<u64>],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    slots: &mut DenseGroupSlots,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let mut readers = Vec::with_capacity(columns.len());
    for (column, translation) in columns.iter().zip(translations) {
        let vector = batch.column(*column).ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))?;
        let Some((crate::batch::TypedValues::Utf8(strings), validity)) = vector.typed() else {
            return Err(ExecError::InvalidBatch(
                "string two-pass key column lost its typed projection",
            ));
        };
        let Some((codes, _)) = strings.dictionary() else {
            return Err(ExecError::InvalidBatch(
                "prepared text scatter requires dictionary codes",
            ));
        };
        readers.push((codes, validity, translation));
    }
    let pair = readers.len() == 2;
    for row in batch.selection().selected_rows() {
        let mut key_bits = 0_u64;
        let mut key_null = false;
        for (codes, validity, translation) in &readers {
            let id = if validity.is_valid(row) {
                let code = usize::try_from(codes[row]).expect("dict code fits usize");
                let interned = *translation
                    .get(code)
                    .ok_or(ExecError::InvalidBatch("dictionary code is out of bounds"))?;
                if pair { interned + 1 } else { interned }
            } else {
                if !pair {
                    key_null = true;
                }
                0
            };
            key_bits = if pair { (key_bits << 32) | id } else { id };
        }
        let states = slots[dense_slot_index(keys, key_bits, key_null)]
            .get_or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
        for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
            if let Some(bits) = two_pass_lane_bits(batch, row, lane) {
                apply_two_pass_lane(&mut states[lane_index], lane, aggregate, bits, memory)?;
            }
        }
    }
    Ok(())
}

/// Merges one dense partial into another (per-batch fold outputs).
fn merge_dense_slots(
    mut into: DenseGroupSlots,
    from: DenseGroupSlots,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<DenseGroupSlots, ExecError> {
    for (target, source) in into.iter_mut().zip(from) {
        let Some(source) = source else { continue };
        match target {
            None => *target = Some(source),
            Some(states) => {
                for ((state, other), aggregate) in states.iter_mut().zip(source).zip(aggregates) {
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
    }
    Ok(into)
}

/// Folds dense slots into the partition maps (dense overflow, mixed
/// serial-scatter flows, and the final pass share this): map collisions
/// merge state-by-state, so dense and classic results always unify.
#[allow(clippy::too_many_arguments)]
fn fold_dense_into_maps(
    slots: DenseGroupSlots,
    keys: TwoPassKeySource,
    aggregates: &[CompiledAggregate],
    partitions: usize,
    maps: &mut [GroupKeyMap],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    for (index, slot) in slots.into_iter().enumerate() {
        let Some(states) = slot else { continue };
        let (bits, null) = dense_slot_sentinel(keys, index);
        let partition =
            usize::try_from(crate::batch::mix64(bits ^ u64::from(null)) % partitions as u64)
                .expect("partition index fits usize");
        match maps[partition].entry((bits, null)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                memory.reserve(per_group_bytes)?;
                *group_reserved = group_reserved.saturating_add(per_group_bytes);
                entry.insert(states);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                for ((state, other), aggregate) in
                    entry.get_mut().iter_mut().zip(states).zip(aggregates)
                {
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
    }
    Ok(())
}

/// Pass 2 over several scatter outputs at once (one per parallel scatter
/// worker): each partition folds its bucket from EVERY set, so parallel
/// pass 1 needs no cross-worker merging (e13's shape, bounded windows).
#[allow(clippy::too_many_lines)]
fn two_pass_flush_sets(
    sets: &mut [Vec<TwoPassBucket>],
    maps: &mut [GroupKeyMap],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let lane_count = lanes.len();
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    let sets_ref: &[Vec<TwoPassBucket>] = sets;
    let added = maps
        .par_iter_mut()
        .enumerate()
        .map(|(partition, map)| -> Result<usize, ExecError> {
            let before = map.len();
            for set in sets_ref {
                let bucket = &set[partition];
                for (row, (key, mask)) in bucket.keys.iter().zip(&bucket.masks).enumerate() {
                    let key_null = mask & (1 << 7) != 0;
                    let states = map
                        .entry((*key, key_null))
                        .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
                    for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate()
                    {
                        if mask & (1 << lane_index) != 0 {
                            continue;
                        }
                        let bits = bucket.lanes[row * lane_count + lane_index];
                        apply_two_pass_lane(
                            &mut states[lane_index],
                            lane,
                            aggregate,
                            bits,
                            memory,
                        )?;
                    }
                }
            }
            let new_groups = map.len().saturating_sub(before);
            let bytes = new_groups.saturating_mul(per_group_bytes);
            memory.reserve(bytes)?;
            Ok(bytes)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for set in sets.iter_mut() {
        for bucket in set.iter_mut() {
            bucket.keys.clear();
            bucket.masks.clear();
            bucket.lanes.clear();
        }
    }
    *group_reserved = group_reserved.saturating_add(added.into_iter().sum());
    Ok(())
}

fn direct_group_value(batch: &RecordBatch, row: usize, column: usize) -> Result<&Value, ExecError> {
    batch
        .column(column)
        .and_then(|values| values.value(row))
        .ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))
}

fn direct_group_matches(
    values: &[Value],
    batch: &RecordBatch,
    row: usize,
    columns: &[usize],
) -> bool {
    values.iter().zip(columns).all(|(grouped, column)| {
        direct_group_value(batch, row, *column).is_ok_and(|candidate| match (grouped, candidate) {
            (Value::Utf8(left), Value::Utf8(right)) => {
                if left.is_ascii() && right.is_ascii() {
                    left.eq_ignore_ascii_case(right)
                } else {
                    compare_utf8_mysql(left, right) == Ordering::Equal
                }
            }
            _ => grouped == candidate,
        })
    })
}

fn direct_group_matches_exact(
    values: &[Value],
    batch: &RecordBatch,
    row: usize,
    columns: &[usize],
) -> bool {
    values.iter().zip(columns).all(|(grouped, column)| {
        direct_group_value(batch, row, *column).is_ok_and(|candidate| grouped == candidate)
    })
}

fn direct_group_hash(batch: &RecordBatch, row: usize, columns: &[usize]) -> Result<u64, ExecError> {
    // The result only routes rows into LOCAL per-batch groups; cross-batch
    // merging keys on normalized values, so per-column path choice (typed vs
    // Value hashing) just needs to be consistent within one batch — and it
    // is, because a column's typed projection is a per-batch constant.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for column in columns {
        let typed_hash = batch
            .column(*column)
            .and_then(ColumnVector::typed)
            .and_then(|(typed, validity)| typed.group_hash_at(row, validity));
        let column_hash = if let Some(column_hash) = typed_hash {
            column_hash
        } else {
            let mut hasher = DefaultHasher::new();
            direct_group_value(batch, row, *column)?.hash(&mut hasher);
            hasher.finish()
        };
        hash = crate::batch::mix64(hash ^ column_hash);
    }
    Ok(hash)
}

/// Typed-first aggregate update from a batch column: packed units and raw
/// integers route straight into the state with no Value cell and no lazy
/// text. Returns whether the row was handled (NULL rows count as handled —
/// they join no aggregate).
fn update_state_from_typed_column(
    state: &mut AggregateState,
    aggregate: &CompiledAggregate,
    batch: &RecordBatch,
    column: usize,
    row: usize,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let Some((typed, validity)) = batch.column(column).and_then(super::ColumnVector::typed) else {
        return Ok(false);
    };
    if !validity.is_valid(row) {
        // JSON_ARRAYAGG is the one aggregate that collects NULL inputs;
        // its generic update encodes them as JSON nulls.
        return Ok(aggregate.function != AggregateFunction::JsonArrayAgg);
    }
    if aggregate.distinct {
        // COUNT(DISTINCT int_col): dedup on the raw integer, no Value (e16).
        if matches!(aggregate.function, AggregateFunction::Count)
            && let Some(key) = typed.int_key_at(row)
        {
            state.update_distinct_count_int(key, memory)?;
            return Ok(true);
        }
        return Ok(false);
    }
    match aggregate.function {
        // No typed fast path yet; `false` sends the row through the generic
        // Value update, which handles them correctly.
        AggregateFunction::AnyValue
        | AggregateFunction::StdDev { .. }
        | AggregateFunction::Variance { .. }
        | AggregateFunction::BitAnd
        | AggregateFunction::BitOr
        | AggregateFunction::BitXor
        | AggregateFunction::JsonObjectAgg => return Ok(false),
        AggregateFunction::Count => {
            state.update_with_number(aggregate, &Value::Boolean(true), None, memory)?;
            return Ok(true);
        }
        AggregateFunction::Sum => {
            if let (Some(units), Some(scale)) = (typed.units_at(row), typed.decimal_scale()) {
                state.update_decimal_sum_units(units, scale, aggregate_uses_float(aggregate))?;
                return Ok(true);
            }
            if aggregate_uses_float(aggregate)
                && let Some(number) = typed.number_at(row)
            {
                state.update_with_number(aggregate, &Value::Boolean(true), Some(number), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::Average => {
            if let Some(result_scale) = decimal_average_scale(aggregate) {
                // Exact decimal AVG: take packed units when the column has
                // them; integer numbers stay exact through the f64 hint;
                // anything else falls back to the real-value update.
                if let (Some(units), Some(scale)) = (typed.units_at(row), typed.decimal_scale())
                    && scale <= result_scale
                    && let Some(rescaled) = decimal_units_from_int(units, result_scale - scale)
                {
                    state.update_decimal_average_units(rescaled, result_scale)?;
                    return Ok(true);
                }
                if let Some(number) = typed.number_at(row)
                    && number.fract() == 0.0
                {
                    state.update_with_number(
                        aggregate,
                        &Value::Boolean(true),
                        Some(number),
                        memory,
                    )?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if let Some(number) = typed.number_at(row) {
                state.update_with_number(aggregate, &Value::Boolean(true), Some(number), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum => {
            if let Some(units) = typed.units_at(row) {
                state.update_extreme_units(aggregate, units, || typed.format_unit(row), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg => {}
    }
    Ok(false)
}

#[allow(clippy::too_many_lines)]
fn update_aggregate_states(
    batch: &RecordBatch,
    row: usize,
    batch_bytes: usize,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let mut numeric_cache = [None::<(usize, f64)>; 8];
    let mut numeric_cache_len = 0_usize;
    for (aggregate, state) in aggregates.iter().zip(states) {
        let direct_scalar = aggregate
            .expr
            .as_ref()
            .is_none_or(|expression| expression.column_index().is_some())
            && !matches!(
                aggregate.function,
                AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
            );
        if !direct_scalar {
            let update_memory = aggregate
                .expr
                .as_ref()
                .map_or(0, |expression| {
                    expression
                        .allocation_upper_bound(batch, row)
                        .saturating_mul(13)
                })
                .saturating_add(
                    64_usize
                        .saturating_mul(size_of::<String>())
                        .saturating_add(256),
                );
            memory.ensure_transient(batch_bytes.saturating_add(update_memory))?;
        }
        // GROUP_CONCAT with an aggregate-local ORDER BY evaluates its key
        // expressions alongside the argument so finish can sort.
        if aggregate.function == AggregateFunction::GroupConcat
            && !aggregate.order_within.is_empty()
        {
            let Some(expression) = &aggregate.expr else {
                return Err(ExecError::InvalidPhysicalPlan(
                    "group-concat requires an argument expression",
                ));
            };
            let value = expression.evaluate(batch, row)?;
            let keys = aggregate
                .order_within
                .iter()
                .map(|(key, _, _)| key.evaluate(batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            state.update_group_concat(&value, keys, memory)?;
            continue;
        }
        match &aggregate.expr {
            None => state.update(aggregate, &Value::Boolean(true), memory)?,
            Some(expression) => {
                if let Some(column) = expression.column_index() {
                    // Typed-first: numeric aggregation over packed units
                    // never touches the column's Value cells or lazy text
                    // (2026-08-02 phase-0 profile: whole-column text
                    // forcing dominated the string-keyed paths).
                    if update_state_from_typed_column(state, aggregate, batch, column, row, memory)?
                    {
                        continue;
                    }
                    let value = direct_group_value(batch, row, column)?;
                    let is_extreme = matches!(
                        aggregate.function,
                        AggregateFunction::Minimum | AggregateFunction::Maximum
                    );
                    // Min/Max take a typed number when available to guide
                    // comparisons but never pay a text parse for it — the
                    // fallback parse is reserved for float-accumulating
                    // aggregates that need the number unconditionally.
                    if is_extreme && !matches!(value, Value::Null) {
                        let number = batch
                            .column(column)
                            .and_then(super::ColumnVector::typed)
                            .and_then(|(typed, _)| typed.number_at(row));
                        state.update_with_number(aggregate, value, number, memory)?;
                        continue;
                    }
                    let number = if aggregate_uses_float(aggregate) && !matches!(value, Value::Null)
                    {
                        if let Some((_, number)) = numeric_cache[..numeric_cache_len]
                            .iter()
                            .filter_map(Option::as_ref)
                            .find(|(cached_column, _)| *cached_column == column)
                        {
                            Some(*number)
                        } else {
                            // Packed projections resolve without per-row text
                            // parsing; mysql_f64 remains the fallback carrier
                            // path (docs/decisions.md, native decimal ADR).
                            let number = batch
                                .column(column)
                                .and_then(super::ColumnVector::typed)
                                .and_then(|(typed, _)| typed.number_at(row))
                                .map_or_else(|| mysql_f64(value), Ok)?;
                            if numeric_cache_len < numeric_cache.len() {
                                numeric_cache[numeric_cache_len] = Some((column, number));
                                numeric_cache_len += 1;
                            }
                            Some(number)
                        }
                    } else {
                        None
                    };
                    state.update_with_number(aggregate, value, number, memory)?;
                } else {
                    let value = expression.evaluate(batch, row)?;
                    state.update(aggregate, &value, memory)?;
                }
            }
        }
    }
    Ok(())
}

fn aggregate_uses_float(aggregate: &CompiledAggregate) -> bool {
    (aggregate.function == AggregateFunction::Average && decimal_average_scale(aggregate).is_none())
        || (aggregate.function == AggregateFunction::Sum
            && aggregate.data_type == Some(DataType::Float64))
}

/// The result scale of an exact decimal AVG, when the binder typed this
/// aggregate as one. `None` keeps the f64 average path.
fn decimal_average_scale(aggregate: &CompiledAggregate) -> Option<u8> {
    if aggregate.function != AggregateFunction::Average {
        return None;
    }
    match aggregate.data_type {
        Some(DataType::Decimal { scale, .. }) => Some(scale),
        _ => None,
    }
}

/// Exact scaled units from a typed-lane f64 (integers below 2^53 convert
/// losslessly); `None` refuses anything that cannot be exact.
fn exact_decimal_units_from_f64(number: f64, scale: u8) -> Option<i128> {
    if number.fract() != 0.0 || number.abs() >= 9_007_199_254_740_992.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    decimal_units_from_int(number as i128, scale)
}

fn decimal_units_from_int(value: i128, scale: u8) -> Option<i128> {
    value.checked_mul(10_i128.checked_pow(u32::from(scale))?)
}

fn add_aggregate_value(
    current: Option<Value>,
    value: &Value,
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    match data_type {
        Some(DataType::UInt64) => {
            let left = current.map_or(Ok(0), |value| mysql_u64(&value))?;
            left.checked_add(mysql_u64(value)?)
                .map(Value::UInt64)
                .ok_or(ExecError::NumericOverflow)
        }
        Some(DataType::Int64) => {
            let left = current.map_or(Ok(0), |value| mysql_i64(&value))?;
            left.checked_add(mysql_i64(value)?)
                .map(Value::Int64)
                .ok_or(ExecError::NumericOverflow)
        }
        Some(DataType::Float64) => {
            let result = current.map_or(Ok(0.0), |value| mysql_f64(&value))? + mysql_f64(value)?;
            if result.is_finite() {
                Ok(Value::float64(result))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        _ => Err(ExecError::InvalidExpressionType),
    }
}

fn compare_aggregate_values(
    left: &Value,
    right: &Value,
    data_type: Option<DataType>,
) -> Result<Ordering, ExecError> {
    if matches!(data_type, Some(DataType::Decimal { .. })) {
        let (Value::Utf8(left), Value::Utf8(right)) = (left, right) else {
            return Err(ExecError::InvalidExpressionType);
        };
        return compare_decimal_text(left, right);
    }
    compare_mysql(left, right)
}

pub(crate) fn compare_decimal_text(left: &str, right: &str) -> Result<Ordering, ExecError> {
    let (left_negative, left_integer, left_fraction) = decimal_parts(left)?;
    let (right_negative, right_integer, right_fraction) = decimal_parts(right)?;
    if left_negative != right_negative {
        return Ok(if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = left_integer
        .len()
        .cmp(&right_integer.len())
        .then_with(|| left_integer.cmp(right_integer))
        .then_with(|| {
            let digits = left_fraction.len().max(right_fraction.len());
            (0..digits)
                .map(|index| {
                    left_fraction
                        .as_bytes()
                        .get(index)
                        .copied()
                        .unwrap_or(b'0')
                        .cmp(
                            &right_fraction
                                .as_bytes()
                                .get(index)
                                .copied()
                                .unwrap_or(b'0'),
                        )
                })
                .find(|ordering| *ordering != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });
    Ok(if left_negative {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn decimal_parts(value: &str) -> Result<(bool, &str, &str), ExecError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if integer.is_empty()
        || !integer.bytes().all(|digit| digit.is_ascii_digit())
        || !fraction.bytes().all(|digit| digit.is_ascii_digit())
    {
        return Err(ExecError::InvalidExpressionType);
    }
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let zero = integer == "0" && fraction.bytes().all(|digit| digit == b'0');
    Ok((negative && !zero, integer, fraction))
}

fn aggregate_string(value: &Value) -> Result<String, ExecError> {
    match value {
        Value::Null => Err(ExecError::InvalidExpressionType),
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

fn scalar_string_memory_upper_bound(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int64(_) | Value::UInt64(_) | Value::Float64(_) => 24,
        Value::Utf8(value) => value.len(),
        Value::Binary(value) => value.len(),
    }
}

fn batch_row(batch: &RecordBatch, row: usize) -> Result<Vec<Value>, ExecError> {
    batch
        .columns()
        .iter()
        .map(|column| {
            column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                "join row is outside an input column",
            ))
        })
        .collect()
}

fn normalized_hash_key(value: Value) -> Option<Value> {
    (!matches!(value, Value::Null)).then(|| normalized_collation_value(value))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
enum JoinHashKey {
    NegativeInteger(i64),
    NonNegativeInteger(u64),
    MysqlNumber(pintail_types::Float64),
    Scalar(Value),
    /// Multi-key equality: primary key first, extras in declaration order.
    Composite(Vec<JoinHashKey>),
}

impl JoinHashKey {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Scalar(value) => value.heap_bytes(),
            Self::NegativeInteger(_) | Self::NonNegativeInteger(_) | Self::MysqlNumber(_) => 0,
            Self::Composite(parts) => parts
                .iter()
                .map(|part| size_of::<Self>().saturating_add(part.heap_bytes()))
                .fold(0, usize::saturating_add),
        }
    }
}

#[derive(Clone, Copy)]
enum JoinSide {
    Build,
    Probe,
}

/// Extends a primary join key with the extra equality keys, or returns
/// `None` when any component is NULL (the row can never match). Single-key
/// joins pass through untouched, keeping their hash shape identical.
fn composite_join_key(
    primary: JoinHashKey,
    batch: &RecordBatch,
    row: usize,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    side: JoinSide,
) -> Result<Option<JoinHashKey>, ExecError> {
    if extra_keys.is_empty() {
        return Ok(Some(primary));
    }
    let mut parts = Vec::with_capacity(1 + extra_keys.len());
    parts.push(primary);
    for (probe_key, build_key, mode) in extra_keys {
        let expr = match side {
            JoinSide::Probe => probe_key,
            JoinSide::Build => build_key,
        };
        let Some(part) = normalized_join_key(expr.evaluate(batch, row)?, *mode)? else {
            return Ok(None);
        };
        parts.push(part);
    }
    Ok(Some(JoinHashKey::Composite(parts)))
}

fn normalized_join_key(value: Value, mode: JoinKeyMode) -> Result<Option<JoinHashKey>, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let key = match mode {
        JoinKeyMode::CollatedText => JoinHashKey::Scalar(normalized_collation_value(value)),
        JoinKeyMode::Binary | JoinKeyMode::Boolean => JoinHashKey::Scalar(value),
        JoinKeyMode::Integer => match value {
            Value::Int64(value) if value < 0 => JoinHashKey::NegativeInteger(value),
            Value::Int64(value) => JoinHashKey::NonNegativeInteger(
                u64::try_from(value).expect("nonnegative i64 fits u64"),
            ),
            Value::UInt64(value) => JoinHashKey::NonNegativeInteger(value),
            _ => return Err(ExecError::InvalidExpressionType),
        },
        JoinKeyMode::MysqlNumber => {
            let value = mysql_f64(&value)?;
            let value = if value == 0.0 { 0.0 } else { value };
            if !value.is_finite() {
                return Err(ExecError::InvalidExpressionType);
            }
            JoinHashKey::MysqlNumber(pintail_types::Float64::new(value))
        }
    };
    Ok(Some(key))
}

fn normalized_collation_value(value: Value) -> Value {
    match value {
        Value::Utf8(value) => Value::Utf8(normalized_collation_text(&value)),
        value => value,
    }
}

/// Text normalization for grouping, hashing, DISTINCT, and set membership.
/// The returned hexadecimal ICU primary sort key compares bytewise in the
/// same order as [`compare_collated_text`], so every hash-based and ordered
/// operator shares one case- and accent-insensitive equivalence relation.
pub(crate) fn normalized_collation_text(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = Vec::new();
    MYSQL_DEFAULT_COLLATOR.with(|collator| {
        collator
            .write_sort_key_to(text, &mut key)
            .expect("Vec-backed collation keys cannot fail");
    });
    let mut encoded = String::with_capacity(key.len().saturating_mul(2));
    for byte in key {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Compares text with the initial `utf8mb4_0900_ai_ci` compatibility profile.
pub(crate) fn compare_collated_text(left: &str, right: &str) -> std::cmp::Ordering {
    MYSQL_DEFAULT_COLLATOR.with(|collator| collator.compare(left, right))
}

thread_local! {
    static MYSQL_DEFAULT_COLLATOR: icu_collator::CollatorBorrowed<'static> = {
        let mut options = icu_collator::options::CollatorOptions::default();
        options.strength = Some(icu_collator::options::Strength::Primary);
        icu_collator::Collator::try_new(icu_collator::CollatorPreferences::default(), options)
            .expect("compiled ICU root collation data is available")
    };
}

struct MaterializedRows {
    rows: Vec<Vec<Value>>,
    position: usize,
}

fn next_materialized_batch(
    state: &mut MaterializedRows,
    column_types: &[DataType],
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    if state.position >= state.rows.len() {
        return Ok(None);
    }
    let end = state
        .position
        .saturating_add(DEFAULT_BATCH_ROWS)
        .min(state.rows.len());
    let rows = &state.rows[state.position..end];
    memory.ensure_transient(estimated_record_batch_bytes(rows, column_types.len()))?;
    let columns = rows_to_columns(rows, column_types)?;
    let batch = RecordBatch::new(rows.len(), columns)?;
    state.position = end;
    Ok(Some(batch))
}

fn materialize(
    input: &mut PullOperator,
    memory: &MemoryTracker,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut rows = Vec::new();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        reserve_vec_elements(&mut rows, additional_rows, 0, memory)?;
        for row in batch.selection().selected_rows() {
            let row_bytes =
                estimated_batch_row_bytes(&batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            memory.reserve(row_bytes)?;
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "cross-join row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn execute_nested_loop_join(
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
    left_columns: &[BoundColumn],
    right_columns: &[BoundColumn],
    kind: BoundJoinKind,
    condition: &BoundExpr,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut columns = left_columns.to_vec();
    columns.extend_from_slice(right_columns);
    let column_types = columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    for left in left_rows {
        memory.check_deadline()?;
        let mut matches = 0_usize;
        for right in right_rows {
            memory.ensure_transient(
                estimated_row_payload_bytes(left)
                    .saturating_add(estimated_row_payload_bytes(right)),
            )?;
            let mut candidate = left.clone();
            candidate.extend(right.iter().cloned());
            let vectors = rows_to_columns(std::slice::from_ref(&candidate), &column_types)?;
            let batch = RecordBatch::new(1, vectors)?;
            let mut predicate = condition.clone();
            resolve_dependent_expr_subqueries(
                &mut predicate,
                &batch,
                0,
                &columns,
                provider,
                memory,
            )?;
            let predicate = CompiledExpr::compile(&predicate, &columns)?;
            if !predicate_truth(&predicate.evaluate(&batch, 0)?)? {
                continue;
            }
            matches = matches.saturating_add(1);
            match kind {
                BoundJoinKind::Inner | BoundJoinKind::Left => {
                    push_nested_join_row(&mut output, candidate, memory)?;
                }
                BoundJoinKind::Scalar => {
                    if matches > 1 {
                        return Err(ExecError::ScalarSubqueryRows { rows: matches });
                    }
                    push_nested_join_row(&mut output, candidate, memory)?;
                }
                BoundJoinKind::Semi => break,
                BoundJoinKind::Anti => {}
                BoundJoinKind::Cross => {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "nested-loop ON evaluation cannot represent a cross join",
                    ));
                }
            }
        }
        match kind {
            BoundJoinKind::Left | BoundJoinKind::Scalar if matches == 0 => {
                let mut row = left.clone();
                row.extend(std::iter::repeat_n(Value::Null, right_columns.len()));
                push_nested_join_row(&mut output, row, memory)?;
            }
            BoundJoinKind::Semi if matches > 0 => {
                push_nested_join_row(&mut output, left.clone(), memory)?;
            }
            BoundJoinKind::Anti if matches == 0 => {
                push_nested_join_row(&mut output, left.clone(), memory)?;
            }
            BoundJoinKind::Inner
            | BoundJoinKind::Left
            | BoundJoinKind::Scalar
            | BoundJoinKind::Semi
            | BoundJoinKind::Anti => {}
            BoundJoinKind::Cross => unreachable!("cross joins return above"),
        }
    }
    Ok(output)
}

fn push_nested_join_row(
    output: &mut Vec<Vec<Value>>,
    row: Vec<Value>,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    reserve_vec_elements(output, 1, 0, memory)?;
    memory.reserve(estimated_row_payload_bytes(&row))?;
    output.push(row);
    Ok(())
}

/// One window computation compiled against its input's column layout.
struct CompiledWindow {
    function: CompiledWindowFunction,
    partition: Vec<CompiledExpr>,
    /// Order keys with `(ascending, nulls_first, decimal)`.
    order: Vec<(CompiledExpr, bool, bool, bool)>,
    /// Explicit `ROWS`/`RANGE` frame; `None` keeps `MySQL`'s default frame.
    frame: Option<pintail_sql::BoundWindowFrame>,
}

enum CompiledWindowFunction {
    RowNumber,
    Rank,
    DenseRank,
    /// The aggregate plus its compiled argument; `COUNT(*)` compiles a
    /// constant 1 so every row counts.
    Aggregate(CompiledAggregate, CompiledExpr),
    /// `LAG`/`LEAD` with its compiled value expression; the default is
    /// compiled alongside so the edge substitution is a plain lookup.
    Offset {
        lead: bool,
        offset: u64,
        argument: CompiledExpr,
        default: Option<CompiledExpr>,
    },
    NTile(u64),
    Extreme {
        last: bool,
        argument: CompiledExpr,
    },
}

impl CompiledWindow {
    fn compile(window: &BoundWindow, columns: &[BoundColumn]) -> Result<Self, ExecError> {
        let function = match &window.function {
            WindowFunction::Offset {
                lead,
                expr,
                offset,
                default,
            } => CompiledWindowFunction::Offset {
                lead: *lead,
                offset: *offset,
                argument: CompiledExpr::compile(expr, columns)?,
                default: default
                    .as_ref()
                    .map(|value| CompiledExpr::compile(value, columns))
                    .transpose()?,
            },
            WindowFunction::NTile(buckets) => CompiledWindowFunction::NTile(*buckets),
            WindowFunction::Extreme { last, expr } => CompiledWindowFunction::Extreme {
                last: *last,
                argument: CompiledExpr::compile(expr, columns)?,
            },
            WindowFunction::RowNumber => CompiledWindowFunction::RowNumber,
            WindowFunction::Rank => CompiledWindowFunction::Rank,
            WindowFunction::DenseRank => CompiledWindowFunction::DenseRank,
            WindowFunction::Aggregate(aggregate) => {
                let argument = match &aggregate.expr {
                    Some(expr) => CompiledExpr::compile(expr, columns)?,
                    None => CompiledExpr::compile(
                        &BoundExpr {
                            kind: BoundExprKind::Literal(Value::Int64(1)),
                            data_type: Some(DataType::Int64),
                            nullable: false,
                        },
                        columns,
                    )?,
                };
                CompiledWindowFunction::Aggregate(
                    CompiledAggregate::compile(aggregate, columns)?,
                    argument,
                )
            }
        };
        Ok(Self {
            function,
            partition: window
                .partition_by
                .iter()
                .map(|expr| CompiledExpr::compile(expr, columns))
                .collect::<Result<Vec<_>, _>>()?,
            order: window
                .order_by
                .iter()
                .map(|key| {
                    Ok::<_, ExecError>((
                        CompiledExpr::compile(&key.expr, columns)?,
                        key.ascending,
                        key.nulls_first,
                        matches!(key.expr.data_type, Some(DataType::Decimal { .. })),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
            frame: window.frame,
        })
    }
}

/// Materializes the input, computes every window over its partitions, and
/// returns rows with the window results appended as trailing columns.
fn build_window(
    input: &mut PullOperator,
    windows: &[CompiledWindow],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut keys: Vec<Vec<Vec<Value>>> = windows.iter().map(|_| Vec::new()).collect();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            memory.ensure_transient(batch_bytes)?;
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "window row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            memory.reserve(estimated_row_payload_bytes(&values))?;
            for (index, window) in windows.iter().enumerate() {
                let mut row_keys =
                    Vec::with_capacity(window.partition.len() + window.order.len() + 1);
                for expr in &window.partition {
                    row_keys.push(expr.evaluate(&batch, row)?);
                }
                for (expr, _, _, _) in &window.order {
                    row_keys.push(expr.evaluate(&batch, row)?);
                }
                match &window.function {
                    CompiledWindowFunction::Aggregate(_, argument)
                    | CompiledWindowFunction::Extreme { argument, .. } => {
                        row_keys.push(argument.evaluate(&batch, row)?);
                    }
                    // LAG/LEAD carry the value and, when present, the edge
                    // default, so both are read positionally later.
                    CompiledWindowFunction::Offset {
                        argument, default, ..
                    } => {
                        row_keys.push(argument.evaluate(&batch, row)?);
                        if let Some(default) = default {
                            row_keys.push(default.evaluate(&batch, row)?);
                        }
                    }
                    CompiledWindowFunction::RowNumber
                    | CompiledWindowFunction::Rank
                    | CompiledWindowFunction::DenseRank
                    | CompiledWindowFunction::NTile(_) => {}
                }
                memory.reserve(estimated_row_payload_bytes(&row_keys))?;
                keys[index].push(row_keys);
            }
            rows.push(values);
        }
    }
    let row_count = rows.len();
    for (index, window) in windows.iter().enumerate() {
        let result = compute_window_column(window, &keys[index], row_count, memory)?;
        for (row, value) in rows.iter_mut().zip(&result) {
            memory.reserve(value.heap_bytes().saturating_add(size_of::<Value>()))?;
            row.push(value.clone());
        }
    }
    Ok(MaterializedRows { rows, position: 0 })
}

enum NumericRangeTarget {
    NegativeInfinity,
    ExactDecimal { units: i128, scale: u8 },
    Value(Value),
    PositiveInfinity,
}

#[allow(clippy::cast_precision_loss)] // Float ordering keys are approximate by definition.
fn numeric_range_target(
    current: &Value,
    offset_units: i128,
    offset_scale: u8,
    add: bool,
    decimal: bool,
) -> Result<NumericRangeTarget, ExecError> {
    let overflow = || {
        if add {
            NumericRangeTarget::PositiveInfinity
        } else {
            NumericRangeTarget::NegativeInfinity
        }
    };
    let target = match current {
        Value::Int64(value) => 10_i128
            .checked_pow(u32::from(offset_scale))
            .and_then(|factor| i128::from(*value).checked_mul(factor))
            .and_then(|value| {
                if add {
                    value.checked_add(offset_units)
                } else {
                    value.checked_sub(offset_units)
                }
            })
            .map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale: offset_scale,
            }),
        Value::UInt64(value) => 10_i128
            .checked_pow(u32::from(offset_scale))
            .and_then(|factor| i128::from(*value).checked_mul(factor))
            .and_then(|value| {
                if add {
                    value.checked_add(offset_units)
                } else {
                    value.checked_sub(offset_units)
                }
            })
            .map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale: offset_scale,
            }),
        Value::Float64(value) => {
            let offset = offset_units as f64 / 10_f64.powi(i32::from(offset_scale));
            let value = if add {
                value.get() + offset
            } else {
                value.get() - offset
            };
            if value.is_finite() {
                NumericRangeTarget::Value(Value::float64(value))
            } else {
                overflow()
            }
        }
        Value::Utf8(text) if decimal => {
            let current_scale = text
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len());
            let current_scale =
                u8::try_from(current_scale).map_err(|_| ExecError::NumericOverflow)?;
            let scale = current_scale.max(offset_scale);
            let units = pintail_types::parse_decimal_scaled(text, current_scale)
                .ok_or(ExecError::InvalidExpressionType)?;
            let rescale = |units: i128, from: u8| {
                10_i128
                    .checked_pow(u32::from(scale - from))
                    .and_then(|factor| units.checked_mul(factor))
            };
            let offset = rescale(units, current_scale)
                .zip(rescale(offset_units, offset_scale))
                .and_then(|(units, offset)| {
                    if add {
                        units.checked_add(offset)
                    } else {
                        units.checked_sub(offset)
                    }
                });
            offset.map_or_else(overflow, |units| NumericRangeTarget::ExactDecimal {
                units,
                scale,
            })
        }
        _ => return Err(ExecError::InvalidExpressionType),
    };
    Ok(target)
}

fn numeric_range_bound(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    current: usize,
    offset: (i128, u8),
    preceding: bool,
    upper: bool,
) -> Result<usize, ExecError> {
    let Some((_, ascending, _, decimal)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let current_value = &keys[partition[current]][key_position];
    let target = numeric_range_target(
        current_value,
        offset.0,
        offset.1,
        if *ascending { !preceding } else { preceding },
        *decimal,
    )?;
    range_bound_for_target(window, keys, partition, &target, upper)
}

fn temporal_range_bound(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    current: usize,
    interval: (u64, pintail_sql::IntervalUnit),
    preceding: bool,
    upper: bool,
) -> Result<usize, ExecError> {
    let Some((_, ascending, _, _)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let current_value = &keys[partition[current]][key_position];
    let target = crate::expression::shift_temporal_value(
        current_value,
        interval.0,
        interval.1,
        if *ascending { !preceding } else { preceding },
    )?;
    range_bound_for_target(
        window,
        keys,
        partition,
        &NumericRangeTarget::Value(target),
        upper,
    )
}

fn range_bound_for_target(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    partition: &[usize],
    target: &NumericRangeTarget,
    upper: bool,
) -> Result<usize, ExecError> {
    let Some((_, ascending, nulls_first, decimal)) = window.order.first() else {
        return Err(ExecError::InvalidExpressionType);
    };
    let key_position = window.partition.len();
    let compare = |candidate: &Value| {
        if matches!(candidate, Value::Null) {
            return if *nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let natural = match &target {
            NumericRangeTarget::NegativeInfinity => Ordering::Greater,
            NumericRangeTarget::PositiveInfinity => Ordering::Less,
            NumericRangeTarget::ExactDecimal { units, scale } => {
                let candidate = match candidate {
                    Value::Boolean(value) => i8::from(*value).to_string(),
                    Value::Int64(value) => value.to_string(),
                    Value::UInt64(value) => value.to_string(),
                    Value::Utf8(value) if *decimal => value.clone(),
                    _ => return Ordering::Equal,
                };
                let ordering = compare_decimal_text(
                    &candidate,
                    &pintail_types::format_decimal_scaled(*units, *scale),
                )
                .unwrap_or(Ordering::Equal);
                if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
            NumericRangeTarget::Value(target) => compare_sort_values(
                candidate,
                target,
                BoundOrderKey {
                    index: 0,
                    ascending: *ascending,
                    nulls_first: *nulls_first,
                    decimal: *decimal,
                },
            ),
        };
        match target {
            NumericRangeTarget::Value(_) | NumericRangeTarget::ExactDecimal { .. } => natural,
            _ if *ascending => natural,
            _ => natural.reverse(),
        }
    };
    let mut low = 0;
    let mut high = partition.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let ordering = compare(&keys[partition[middle]][key_position]);
        let before_boundary = if upper {
            ordering != Ordering::Greater
        } else {
            ordering == Ordering::Less
        };
        if before_boundary {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// Computes one window's value per row: sorts a permutation by
/// (partition, order) keys, then walks each partition assigning ranks or
/// aggregate frames (whole partition without ORDER BY; running frame
/// including the current row's peers with it — `MySQL`'s default frames).
#[allow(clippy::too_many_lines)]
fn compute_window_column(
    window: &CompiledWindow,
    keys: &[Vec<Value>],
    row_count: usize,
    memory: &MemoryTracker,
) -> Result<Vec<Value>, ExecError> {
    let partition_len = window.partition.len();
    let order_key = |ascending: bool, nulls_first: bool, decimal: bool| BoundOrderKey {
        index: 0,
        ascending,
        nulls_first,
        decimal,
    };
    let compare_rows = |left: usize, right: usize| {
        let left_keys = &keys[left];
        let right_keys = &keys[right];
        for position in 0..partition_len {
            let ordering = compare_sort_values(
                &left_keys[position],
                &right_keys[position],
                order_key(true, true, false),
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        for (position, (_, ascending, nulls_first, decimal)) in window.order.iter().enumerate() {
            let ordering = compare_sort_values(
                &left_keys[partition_len + position],
                &right_keys[partition_len + position],
                order_key(*ascending, *nulls_first, *decimal),
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    };
    let same_partition = |left: usize, right: usize| {
        (0..partition_len).all(|position| {
            compare_sort_values(
                &keys[left][position],
                &keys[right][position],
                order_key(true, true, false),
            ) == Ordering::Equal
        })
    };
    let same_peers = |left: usize, right: usize| {
        window.order.iter().enumerate().all(|(position, key)| {
            compare_sort_values(
                &keys[left][partition_len + position],
                &keys[right][partition_len + position],
                order_key(key.1, key.2, key.3),
            ) == Ordering::Equal
        })
    };

    let mut order = (0..row_count).collect::<Vec<_>>();
    memory.reserve(row_count.saturating_mul(size_of::<usize>()))?;
    order.sort_by(|left, right| compare_rows(*left, *right));

    let mut results = vec![Value::Null; row_count];
    let mut start = 0;
    while start < row_count {
        let mut end = start + 1;
        while end < row_count && same_partition(order[start], order[end]) {
            end += 1;
        }
        let partition = &order[start..end];
        let peer_start = |from: usize| {
            let mut first = from;
            while first > 0 && same_peers(partition[first - 1], partition[from]) {
                first -= 1;
            }
            first
        };
        let peer_end = |from: usize| {
            let mut last = from + 1;
            while last < partition.len() && same_peers(partition[from], partition[last]) {
                last += 1;
            }
            last
        };
        let frame_extent = |frame: pintail_sql::BoundWindowFrame,
                            index: usize|
         -> Result<(usize, usize), ExecError> {
            use pintail_sql::{BoundFrameBound as Edge, BoundFrameOffset as Offset};
            let len = partition.len();
            let row_offset = |offset: Offset| match offset {
                Offset::Rows(value) => Ok(usize::try_from(value).unwrap_or(usize::MAX)),
                _ => Err(ExecError::InvalidPhysicalPlan(
                    "non-row offset reached a ROWS frame",
                )),
            };
            let range_bound = |offset: Offset, preceding: bool, upper: bool| match offset {
                Offset::Numeric { units, scale } => numeric_range_bound(
                    window,
                    keys,
                    partition,
                    index,
                    (units, scale),
                    preceding,
                    upper,
                ),
                Offset::Interval { value, unit } => temporal_range_bound(
                    window,
                    keys,
                    partition,
                    index,
                    (value, unit),
                    preceding,
                    upper,
                ),
                Offset::Rows(_) => Err(ExecError::InvalidPhysicalPlan(
                    "row offset reached a RANGE frame",
                )),
            };
            let current_is_null = window
                .order
                .first()
                .is_some_and(|_| matches!(keys[partition[index]][partition_len], Value::Null));
            let start = match frame.start {
                Edge::UnboundedPreceding => 0,
                Edge::Preceding(_) | Edge::Following(_) if frame.range && current_is_null => {
                    peer_start(index)
                }
                Edge::Preceding(offset) if frame.range => range_bound(offset, true, false)?,
                Edge::Following(offset) if frame.range => range_bound(offset, false, false)?,
                Edge::Preceding(offset) => index.saturating_sub(row_offset(offset)?),
                Edge::CurrentRow if frame.range => peer_start(index),
                Edge::CurrentRow => index,
                Edge::Following(offset) => index.saturating_add(row_offset(offset)?).min(len),
                Edge::UnboundedFollowing => len,
            };
            let end = match frame.end {
                Edge::UnboundedPreceding => 0,
                Edge::Preceding(_) | Edge::Following(_) if frame.range && current_is_null => {
                    peer_end(index)
                }
                Edge::Preceding(offset) if frame.range => range_bound(offset, true, true)?,
                Edge::Following(offset) if frame.range => range_bound(offset, false, true)?,
                Edge::Preceding(offset) => index
                    .checked_sub(row_offset(offset)?)
                    .map_or(0, |row| row + 1),
                Edge::CurrentRow if frame.range => peer_end(index),
                Edge::CurrentRow => index + 1,
                Edge::Following(offset) => index
                    .saturating_add(row_offset(offset)?)
                    .saturating_add(1)
                    .min(len),
                Edge::UnboundedFollowing => len,
            };
            Ok((start, end))
        };
        match &window.function {
            CompiledWindowFunction::RowNumber
            | CompiledWindowFunction::Rank
            | CompiledWindowFunction::DenseRank => {
                let mut rank = 0_u64;
                let mut dense = 0_u64;
                for (position, row) in partition.iter().enumerate() {
                    let number = u64::try_from(position + 1).unwrap_or(u64::MAX);
                    if position == 0 || !same_peers(partition[position - 1], *row) {
                        rank = number;
                        dense += 1;
                    }
                    results[*row] = Value::UInt64(match window.function {
                        CompiledWindowFunction::RowNumber => number,
                        CompiledWindowFunction::Rank => rank,
                        _ => dense,
                    });
                }
            }
            CompiledWindowFunction::Offset {
                lead,
                offset,
                default,
                ..
            } => {
                let value_position = partition_len + window.order.len();
                let offset = usize::try_from(*offset).unwrap_or(usize::MAX);
                for (index, row) in partition.iter().enumerate() {
                    let source = if *lead {
                        index.checked_add(offset)
                    } else {
                        index.checked_sub(offset)
                    };
                    let value = match source.filter(|source| *source < partition.len()) {
                        Some(source) => keys[partition[source]][value_position].clone(),
                        // Past the partition edge MySQL substitutes the
                        // default, evaluated on the current row, and NULL
                        // when none was given.
                        None if default.is_some() => keys[*row][value_position + 1].clone(),
                        None => Value::Null,
                    };
                    memory.reserve(value.heap_bytes())?;
                    results[*row] = value;
                }
            }
            CompiledWindowFunction::NTile(buckets) => {
                // MySQL gives the larger buckets to the earlier positions:
                // the first (len % buckets) buckets take one extra row.
                // More buckets than rows means every row is its own bucket
                // and the rest are empty; capping at the row count keeps
                // NTILE(18446744073709551615) from walking 2^64 of them.
                let buckets = usize::try_from(*buckets)
                    .unwrap_or(usize::MAX)
                    .max(1)
                    .min(partition.len().max(1));
                let base = partition.len() / buckets;
                let wide = partition.len() % buckets;
                let mut assigned = 0;
                for bucket in 0..buckets {
                    let size = base + usize::from(bucket < wide);
                    for row in partition.iter().skip(assigned).take(size) {
                        results[*row] = Value::UInt64(bucket as u64 + 1);
                    }
                    assigned += size;
                }
            }
            CompiledWindowFunction::Extreme { last, .. } => {
                let value_position = partition_len + window.order.len();
                if let Some(frame) = window.frame {
                    // An explicit frame governs which row is read. Binding a
                    // frame and then ignoring it would answer the default
                    // frame's question under the caller's syntax.
                    for index in 0..partition.len() {
                        let (start, end) = frame_extent(frame, index)?;
                        // An empty frame has no value to read.
                        let value = if start >= end {
                            Value::Null
                        } else {
                            let source = if *last { end - 1 } else { start };
                            keys[partition[source]][value_position].clone()
                        };
                        memory.reserve(value.heap_bytes())?;
                        results[partition[index]] = value;
                    }
                } else if !*last || window.order.is_empty() {
                    // FIRST_VALUE reads the partition's first row; without
                    // ORDER BY the frame is the whole partition, so
                    // LAST_VALUE reads its last.
                    let source = if *last {
                        *partition.last().expect("partitions are non-empty")
                    } else {
                        partition[0]
                    };
                    let value = keys[source][value_position].clone();
                    for row in partition {
                        memory.reserve(value.heap_bytes())?;
                        results[*row] = value.clone();
                    }
                } else {
                    // Under MySQL's default frame LAST_VALUE is the last row
                    // of the CURRENT PEER GROUP, not of the partition. This
                    // surprises people, and matching it is the whole point of
                    // pinning it against the oracle.
                    let mut group_start = 0;
                    while group_start < partition.len() {
                        let mut group_end = group_start + 1;
                        while group_end < partition.len()
                            && same_peers(partition[group_start], partition[group_end])
                        {
                            group_end += 1;
                        }
                        let value = keys[partition[group_end - 1]][value_position].clone();
                        for row in &partition[group_start..group_end] {
                            memory.reserve(value.heap_bytes())?;
                            results[*row] = value.clone();
                        }
                        group_start = group_end;
                    }
                }
            }
            CompiledWindowFunction::Aggregate(aggregate, _) => {
                let argument_position = partition_len + window.order.len();
                if let Some(frame) = window.frame {
                    use pintail_sql::BoundFrameBound as Edge;
                    // A frame anchored at UNBOUNDED PRECEDING accumulates
                    // once across the partition; anything else is a sliding
                    // window recomputed over its own width. MIN/MAX cannot be
                    // un-accumulated, so a bounded start has no cheaper form
                    // without a monotonic deque per aggregate kind.
                    // The incremental path needs the frame end to advance
                    // monotonically, which holds for both ROWS and RANGE when
                    // the start is anchored — peer-group ends are also
                    // non-decreasing across a sorted partition.
                    let running = matches!(frame.start, Edge::UnboundedPreceding);
                    let mut state = AggregateState::new(aggregate);
                    let mut accumulated = 0_usize;
                    for index in 0..partition.len() {
                        // Under RANGE, CURRENT ROW covers the whole peer
                        // group rather than the single row: the frame is
                        // defined over the ordering key's values, and peers
                        // share one value.
                        let (start, end) = frame_extent(frame, index)?;
                        let value = if running {
                            while accumulated < end {
                                state.update(
                                    aggregate,
                                    &keys[partition[accumulated]][argument_position],
                                    memory,
                                )?;
                                accumulated += 1;
                            }
                            state.clone().finish(memory)?
                        } else {
                            let mut framed = AggregateState::new(aggregate);
                            for row in partition.iter().take(end).skip(start) {
                                framed.update(aggregate, &keys[*row][argument_position], memory)?;
                            }
                            framed.finish(memory)?
                        };
                        memory.reserve(value.heap_bytes())?;
                        results[partition[index]] = value;
                    }
                } else if window.order.is_empty() {
                    // Whole-partition frame.
                    let mut state = AggregateState::new(aggregate);
                    for row in partition {
                        state.update(aggregate, &keys[*row][argument_position], memory)?;
                    }
                    let value = state.finish(memory)?;
                    for row in partition {
                        memory.reserve(value.heap_bytes())?;
                        results[*row] = value.clone();
                    }
                } else {
                    // Running frame including the current row's peers.
                    let mut state = AggregateState::new(aggregate);
                    let mut group_start = 0;
                    while group_start < partition.len() {
                        let mut group_end = group_start + 1;
                        while group_end < partition.len()
                            && same_peers(partition[group_start], partition[group_end])
                        {
                            group_end += 1;
                        }
                        for row in &partition[group_start..group_end] {
                            state.update(aggregate, &keys[*row][argument_position], memory)?;
                        }
                        let value = state.clone().finish(memory)?;
                        for row in &partition[group_start..group_end] {
                            memory.reserve(value.heap_bytes())?;
                            results[*row] = value.clone();
                        }
                        group_start = group_end;
                    }
                }
            }
        }
        start = end;
    }
    Ok(results)
}

/// Sorted rows served either from memory (the fast path, byte-identical to
/// the pre-spill behavior) or by merging sorted on-disk runs when the input
/// exceeded the query memory ceiling.
enum SortedRows {
    Memory(MaterializedRows),
    Spilled(SpilledMerge),
}

impl SortedRows {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        match self {
            Self::Memory(rows) => {
                let row = rows.rows.get(rows.position).cloned();
                rows.position = rows.position.saturating_add(usize::from(row.is_some()));
                Ok(row)
            }
            Self::Spilled(merge) => merge.next_row(),
        }
    }

    fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        match self {
            Self::Memory(rows) => next_materialized_batch(rows, column_types, memory),
            Self::Spilled(merge) => merge.next_batch(column_types, memory),
        }
    }
}

/// Blocking standalone DISTINCT implemented as an external sort followed by
/// adjacent-row elimination. The all-column ordering uses the same collation
/// and exact DECIMAL comparator as ORDER BY, grouping, and set semantics.
struct DistinctRows {
    sorted: SortedRows,
    keys: Vec<BoundOrderKey>,
    last: Option<Vec<Value>>,
    last_reserved: usize,
}

impl DistinctRows {
    fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            let Some(mut batch) = self.sorted.next_batch(column_types, memory)? else {
                memory.release(self.last_reserved);
                self.last_reserved = 0;
                self.last = None;
                return Ok(None);
            };
            let batch_bytes = batch.estimated_bytes();
            for row in batch.selection().selected_rows().collect::<Vec<_>>() {
                let values = batch_row(&batch, row)?;
                if self
                    .last
                    .as_ref()
                    .is_some_and(|last| compare_sort_rows(last, &values, &self.keys).is_eq())
                {
                    batch.selection_mut().set(row, false)?;
                    continue;
                }
                let bytes = estimated_row_payload_bytes(&values);
                memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                if bytes > self.last_reserved {
                    memory.reserve(bytes - self.last_reserved)?;
                } else {
                    memory.release(self.last_reserved - bytes);
                }
                self.last_reserved = bytes;
                self.last = Some(values);
            }
            if batch.visible_row_count() > 0 {
                return Ok(Some(batch));
            }
        }
    }
}

/// External sort-merge state for INTERSECT and EXCEPT. Both inputs use the
/// shared full-row comparator, so spilling preserves exact DECIMAL and `MySQL`
/// collation equivalence instead of introducing a second key definition.
struct SetOpRows {
    left: SortedRowCursor,
    right: SortedRowCursor,
    keys: Vec<BoundOrderKey>,
    keep_matching: bool,
    all: bool,
    pending: Option<(Vec<Value>, u64)>,
    pending_reserved: usize,
}

struct SortedRowCursor {
    rows: SortedRows,
    head: Option<Vec<Value>>,
    exhausted: bool,
}

impl SortedRowCursor {
    fn new(rows: SortedRows) -> Self {
        Self {
            rows,
            head: None,
            exhausted: false,
        }
    }

    fn ensure_head(&mut self) -> Result<(), ExecError> {
        if self.head.is_none() && !self.exhausted {
            self.head = self.rows.next_row()?;
            self.exhausted = self.head.is_none();
        }
        Ok(())
    }

    fn take_group(&mut self, keys: &[BoundOrderKey]) -> Result<(Vec<Value>, u64), ExecError> {
        self.ensure_head()?;
        let first = self
            .head
            .take()
            .ok_or(ExecError::InvalidPhysicalPlan("set input group is empty"))?;
        let mut count = 1_u64;
        loop {
            let Some(next) = self.rows.next_row()? else {
                self.exhausted = true;
                break;
            };
            if compare_sort_rows(&first, &next, keys).is_eq() {
                count = count.saturating_add(1);
            } else {
                self.head = Some(next);
                break;
            }
        }
        Ok((first, count))
    }
}

impl SetOpRows {
    fn next_group(
        &mut self,
        memory: &MemoryTracker,
    ) -> Result<Option<(Vec<Value>, u64)>, ExecError> {
        loop {
            memory.check_deadline()?;
            self.left.ensure_head()?;
            self.right.ensure_head()?;
            let ordering = match (self.left.head.as_ref(), self.right.head.as_ref()) {
                (None, _) => return Ok(None),
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => compare_sort_rows(left, right, &self.keys),
            };
            match ordering {
                Ordering::Less => {
                    let (row, left_count) = self.left.take_group(&self.keys)?;
                    if !self.keep_matching {
                        return Ok(Some((row, if self.all { left_count } else { 1 })));
                    }
                }
                Ordering::Greater => {
                    let _ = self.right.take_group(&self.keys)?;
                }
                Ordering::Equal => {
                    let (row, left_count) = self.left.take_group(&self.keys)?;
                    let (_, right_count) = self.right.take_group(&self.keys)?;
                    let output_count = if self.keep_matching {
                        if self.all {
                            left_count.min(right_count)
                        } else {
                            1
                        }
                    } else if self.all {
                        left_count.saturating_sub(right_count)
                    } else {
                        0
                    };
                    if output_count > 0 {
                        return Ok(Some((row, output_count)));
                    }
                }
            }
        }
    }

    fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        let mut rows = Vec::with_capacity(DEFAULT_BATCH_ROWS);
        while rows.len() < DEFAULT_BATCH_ROWS {
            if self.pending.is_none() {
                let Some((row, count)) = self.next_group(memory)? else {
                    break;
                };
                let bytes = estimated_row_payload_bytes(&row);
                memory.reserve(bytes)?;
                self.pending_reserved = bytes;
                self.pending = Some((row, count));
            }
            let (row, count) = self.pending.as_mut().expect("initialized above");
            rows.push(row.clone());
            *count -= 1;
            if *count == 0 {
                self.pending = None;
                memory.release(self.pending_reserved);
                self.pending_reserved = 0;
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        memory.ensure_transient(estimated_record_batch_bytes(&rows, column_types.len()))?;
        let columns = rows_to_columns(&rows, column_types)?;
        Ok(Some(RecordBatch::new(rows.len(), columns)?))
    }
}

fn build_set_operation(
    left: &mut PullOperator,
    right: &mut PullOperator,
    column_types: &[DataType],
    keep_matching: bool,
    all: bool,
    memory: &MemoryTracker,
) -> Result<SetOpRows, ExecError> {
    let keys = column_types
        .iter()
        .enumerate()
        .map(|(index, data_type)| BoundOrderKey {
            index,
            ascending: true,
            nulls_first: true,
            decimal: matches!(data_type, DataType::Decimal { .. }),
        })
        .collect::<Vec<_>>();
    let left = build_sort(left, &keys, None, None, memory)?;
    let right = build_sort(right, &keys, None, None, memory)?;
    Ok(SetOpRows {
        left: SortedRowCursor::new(left),
        right: SortedRowCursor::new(right),
        keys,
        keep_matching,
        all,
        pending: None,
        pending_reserved: 0,
    })
}

fn build_distinct(
    input: &mut PullOperator,
    column_types: &[DataType],
    memory: &MemoryTracker,
) -> Result<DistinctRows, ExecError> {
    let keys = column_types
        .iter()
        .enumerate()
        .map(|(index, data_type)| BoundOrderKey {
            index,
            ascending: true,
            nulls_first: true,
            decimal: matches!(data_type, DataType::Decimal { .. }),
        })
        .collect::<Vec<_>>();
    let sorted = build_sort(input, &keys, None, None, memory)?;
    Ok(DistinctRows {
        sorted,
        keys,
        last: None,
        last_reserved: 0,
    })
}

fn build_sort(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    top_k: Option<usize>,
    trim_to: Option<usize>,
    memory: &MemoryTracker,
) -> Result<SortedRows, ExecError> {
    let compare = |left: &Vec<Value>, right: &Vec<Value>| compare_sort_rows(left, right, keys);
    if let Some(top_k) = top_k {
        // Top-k retains at most k rows and cannot exceed the ceiling by
        // materializing its input; the in-memory path is unchanged.
        let mut rows = materialize_top_k(input, top_k, keys, compare, memory)?;
        rows.sort_by(compare);
        if let Some(width) = trim_to {
            for row in &mut rows {
                row.truncate(width);
            }
        }
        return Ok(SortedRows::Memory(MaterializedRows { rows, position: 0 }));
    }
    let SpillMaterialization {
        mut rows,
        runs,
        reserved: rows_reserved,
    } = materialize_with_spill(input, keys, memory)?;
    rows.sort_by(compare);
    if runs.is_empty() {
        if let Some(width) = trim_to {
            for row in &mut rows {
                row.truncate(width);
            }
        }
        return Ok(SortedRows::Memory(MaterializedRows { rows, position: 0 }));
    }
    let mut merge = SpilledMerge::new(runs, keys.to_vec(), trim_to)?;
    merge.push_final_run(&rows, memory)?;
    memory.release(rows_reserved);
    Ok(SortedRows::Spilled(merge))
}

/// Materializes the sort input, spilling the accumulated rows as a sorted
/// on-disk run whenever the memory ceiling would be exceeded. Queries that
/// fit in memory take exactly the old path and produce no runs.
struct SpillMaterialization {
    rows: Vec<Vec<Value>>,
    runs: Vec<SpilledRun>,
    reserved: usize,
}

fn materialize_with_spill(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    memory: &MemoryTracker,
) -> Result<SpillMaterialization, ExecError> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut retained = 0_usize;
    let mut vector_reserved = 0_usize;
    let mut runs: Vec<SpilledRun> = Vec::new();
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        match reserve_vec_elements(&mut rows, additional_rows, 0, memory) {
            Ok(reserved) => vector_reserved = vector_reserved.saturating_add(reserved),
            Err(ExecError::MemoryLimitExceeded { .. }) if !rows.is_empty() => {
                rows.sort_by(|left, right| compare_sort_rows(left, right, keys));
                runs.push(SpilledRun::write(&rows, memory)?);
                rows = Vec::new();
                memory.release(retained.saturating_add(vector_reserved));
                retained = 0;
                vector_reserved = 0;
                vector_reserved = vector_reserved.saturating_add(reserve_vec_elements(
                    &mut rows,
                    additional_rows,
                    0,
                    memory,
                )?);
            }
            Err(error) => return Err(error),
        }
        for row in batch.selection().selected_rows() {
            let row_bytes =
                estimated_batch_row_bytes(&batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            match memory.reserve(row_bytes) {
                Ok(()) => {}
                Err(ExecError::MemoryLimitExceeded { .. }) if !rows.is_empty() => {
                    // Spill the buffered rows as one sorted run and retry;
                    // releasing both the row payloads and the vector's
                    // capacity reservation frees the sort's whole footprint.
                    rows.sort_by(|left, right| compare_sort_rows(left, right, keys));
                    runs.push(SpilledRun::write(&rows, memory)?);
                    rows = Vec::new();
                    memory.release(retained.saturating_add(vector_reserved));
                    retained = 0;
                    vector_reserved = 0;
                    memory.reserve(row_bytes)?;
                }
                Err(error) => return Err(error),
            }
            retained = retained.saturating_add(row_bytes);
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "sort row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
            // Proactive spill at half the ceiling: upstream operators size
            // their own working sets from the remaining headroom, so a sort
            // that hoards the budget until hard failure starves the scan.
            if retained.saturating_add(vector_reserved) > memory.limit() / 2 && rows.len() > 1 {
                rows.sort_by(|left, right| compare_sort_rows(left, right, keys));
                runs.push(SpilledRun::write(&rows, memory)?);
                rows = Vec::new();
                memory.release(retained.saturating_add(vector_reserved));
                retained = 0;
                vector_reserved = 0;
            }
        }
    }
    Ok(SpillMaterialization {
        rows,
        runs,
        reserved: retained.saturating_add(vector_reserved),
    })
}

/// One sorted run on disk: length-framed binary rows in a self-deleting
/// temp file, streamed back in write order.
struct SpilledRun {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
    _path: tempfile::TempPath,
    _reservation: spill::SpillReservation,
}

impl SpilledRun {
    fn write(rows: &[Vec<Value>], memory: &MemoryTracker) -> Result<Self, ExecError> {
        let file = spill::spill_file("pintail-sort-spill-", memory.spill())
            .map_err(|error| ExecError::Source(format!("sort spill create: {error}")))?;
        let (file, path, mut reservation) = file.into_parts();
        let mut writer = std::io::BufWriter::new(file);
        let mut encoder = spill::Encoder::new();
        for row in rows {
            encoder.values(row);
            let payload = std::mem::replace(&mut encoder, spill::Encoder::new()).finish();
            spill::write_record_quota(&mut writer, &payload, &mut reservation)
                .map_err(|error| ExecError::Source(format!("sort spill write: {error}")))?;
        }
        let mut file = writer
            .into_inner()
            .map_err(|error| ExecError::Source(format!("sort spill flush: {error}")))?;
        std::io::Seek::rewind(&mut file)
            .map_err(|error| ExecError::Source(format!("sort spill rewind: {error}")))?;
        Ok(Self {
            reader: std::io::BufReader::new(file),
            payload: Vec::new(),
            _path: path,
            _reservation: reservation,
        })
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("sort spill read: {error}")))?
        {
            return Ok(None);
        }
        spill::Decoder::new(&self.payload)
            .values()
            .map(Some)
            .map_err(|error| ExecError::Source(format!("sort spill decode: {error}")))
    }
}

/// K-way merge over sorted spilled runs; run count is bounded by
/// input-bytes / memory-ceiling, so a linear minimum scan per row is fine.
struct SpilledMerge {
    runs: Vec<SpilledRun>,
    heads: Vec<Option<Vec<Value>>>,
    keys: Vec<BoundOrderKey>,
    trim_to: Option<usize>,
}

impl SpilledMerge {
    fn new(
        runs: Vec<SpilledRun>,
        keys: Vec<BoundOrderKey>,
        trim_to: Option<usize>,
    ) -> Result<Self, ExecError> {
        let mut merge = Self {
            heads: Vec::with_capacity(runs.len()),
            runs,
            keys,
            trim_to,
        };
        for index in 0..merge.runs.len() {
            let head = merge.runs[index].next_row()?;
            merge.heads.push(head);
        }
        Ok(merge)
    }

    fn push_final_run(
        &mut self,
        rows: &[Vec<Value>],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut run = SpilledRun::write(rows, memory)?;
        let head = run.next_row()?;
        self.runs.push(run);
        self.heads.push(head);
        Ok(())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        let mut best: Option<usize> = None;
        for (index, head) in self.heads.iter().enumerate() {
            let Some(candidate) = head else { continue };
            let better = match best {
                None => true,
                Some(current) => {
                    let current_head = self.heads[current]
                        .as_ref()
                        .expect("best head is always occupied");
                    compare_sort_rows(candidate, current_head, &self.keys) == Ordering::Less
                }
            };
            if better {
                best = Some(index);
            }
        }
        let Some(winner) = best else { return Ok(None) };
        let replacement = self.runs[winner].next_row()?;
        let mut row = std::mem::replace(&mut self.heads[winner], replacement)
            .expect("winner head is occupied");
        if let Some(width) = self.trim_to {
            row.truncate(width);
        }
        Ok(Some(row))
    }

    fn next_batch(
        &mut self,
        column_types: &[DataType],
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        let mut rows = Vec::with_capacity(DEFAULT_BATCH_ROWS);
        while rows.len() < DEFAULT_BATCH_ROWS {
            let Some(row) = self.next_row()? else { break };
            rows.push(row);
        }
        if rows.is_empty() {
            return Ok(None);
        }
        memory.ensure_transient(estimated_record_batch_bytes(&rows, column_types.len()))?;
        let columns = rows_to_columns(&rows, column_types)?;
        Ok(Some(RecordBatch::new(rows.len(), columns)?))
    }
}

fn materialize_top_k(
    input: &mut PullOperator,
    top_k: usize,
    keys: &[BoundOrderKey],
    compare: impl Copy + FnMut(&Vec<Value>, &Vec<Value>) -> Ordering,
    memory: &MemoryTracker,
) -> Result<Vec<Vec<Value>>, ExecError> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    // Threshold prefilter (experiments/RESULTS.md e03): once k rows are
    // retained, their current worst acts as a cutoff — rows comparing
    // STRICTLY worse on the sort keys can never enter the top k and are
    // skipped before any column values are cloned. Rows tying the threshold
    // are kept, so the candidate set stays a superset and selection
    // semantics are unchanged.
    let mut threshold: Option<Vec<Value>> = None;
    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        let additional_rows = batch.visible_row_count();
        memory.ensure_transient(
            batch_bytes.saturating_add(additional_rows.saturating_mul(size_of::<Vec<Value>>())),
        )?;
        reserve_vec_elements(&mut rows, additional_rows, 0, memory)?;
        for row in batch.selection().selected_rows() {
            if let Some(threshold_row) = &threshold {
                let mut ordering = Ordering::Equal;
                for key in keys {
                    let candidate = batch
                        .column(key.index)
                        .and_then(|column| column.value(row))
                        .unwrap_or(&Value::Null);
                    let retained = threshold_row.get(key.index).unwrap_or(&Value::Null);
                    let key_ordering = compare_sort_values(candidate, retained, *key);
                    if key_ordering != Ordering::Equal {
                        ordering = key_ordering;
                        break;
                    }
                }
                if ordering == Ordering::Greater {
                    continue;
                }
            }
            let row_bytes =
                estimated_batch_row_bytes(&batch, row)?.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
            memory.reserve(row_bytes)?;
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "top-K row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
        if rows.len() > top_k {
            rows.select_nth_unstable_by(top_k, compare);
            let released = rows[top_k..]
                .iter()
                .map(|row| estimated_row_payload_bytes(row))
                .sum::<usize>();
            rows.truncate(top_k);
            let old_capacity = rows.capacity();
            rows.shrink_to_fit();
            memory.release(
                released.saturating_add(
                    old_capacity
                        .saturating_sub(rows.capacity())
                        .saturating_mul(size_of::<Vec<Value>>()),
                ),
            );
            let mut compare = compare;
            threshold = rows
                .iter()
                .max_by(|left, right| compare(left, right))
                .cloned();
        }
    }
    Ok(rows)
}

fn compare_sort_rows(left: &[Value], right: &[Value], keys: &[BoundOrderKey]) -> Ordering {
    for key in keys {
        let ordering = compare_sort_values(
            left.get(key.index).unwrap_or(&Value::Null),
            right.get(key.index).unwrap_or(&Value::Null),
            *key,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_sort_values(left: &Value, right: &Value, key: BoundOrderKey) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if key.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if key.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Value::Utf8(left), Value::Utf8(right)) => {
            // Canonical decimal text orders numerically; lexical ordering
            // would put "9.00" after "10.00". Unparseable text (shouldn't
            // happen for decimal-typed keys) falls back to text order.
            let ordering = if key.decimal {
                compare_decimal_text(left, right)
                    .unwrap_or_else(|_| compare_utf8_mysql(left, right))
            } else {
                compare_utf8_mysql(left, right)
            };
            order_direction(ordering, key.ascending)
        }
        _ => order_direction(left.cmp(right), key.ascending),
    }
}

fn order_direction(ordering: Ordering, ascending: bool) -> Ordering {
    if ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn rows_to_columns(
    rows: &[Vec<Value>],
    column_types: &[DataType],
) -> Result<Vec<ColumnVector>, ExecError> {
    column_types
        .iter()
        .enumerate()
        .map(|(column, data_type)| {
            let values = rows
                .iter()
                .map(|row| {
                    row.get(column).cloned().ok_or(ExecError::InvalidBatch(
                        "cross-join result is shorter than its layout",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            ColumnVector::new(*data_type, values).map_err(ExecError::from)
        })
        .collect()
}

fn estimated_row_payload_bytes(row: &[Value]) -> usize {
    size_of_val(row) + row.iter().map(Value::heap_bytes).sum::<usize>() + 2 * size_of::<usize>()
}

fn reserve_vec_elements<T>(
    values: &mut Vec<T>,
    additional: usize,
    minimum_growth: usize,
    memory: &MemoryTracker,
) -> Result<usize, ExecError> {
    let required = values.len().saturating_add(additional);
    if required <= values.capacity() {
        return Ok(0);
    }
    let old_capacity = values.capacity();
    let growth = required.saturating_sub(old_capacity).max(minimum_growth);
    let capacity_bound = old_capacity
        .saturating_add(growth)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    let reserved = capacity_bound
        .saturating_sub(old_capacity)
        .saturating_mul(size_of::<T>());
    memory.reserve(reserved)?;
    values.reserve_exact(
        old_capacity
            .saturating_add(growth)
            .saturating_sub(values.len()),
    );
    let actual = values
        .capacity()
        .saturating_sub(old_capacity)
        .saturating_mul(size_of::<T>());
    if actual > reserved {
        return Err(ExecError::InvalidPhysicalPlan(
            "vector capacity exceeded its preflight bound",
        ));
    }
    memory.release(reserved - actual);
    Ok(actual)
}

fn reserve_hash_map_entries<K, V, S>(
    values: &mut HashMap<K, V, S>,
    additional: usize,
    entry_bytes: usize,
    transient_bytes: usize,
    memory: &MemoryTracker,
) -> Result<usize, ExecError>
where
    K: Eq + Hash,
    S: std::hash::BuildHasher,
{
    let required = values.len().saturating_add(additional);
    if required <= values.capacity() {
        return Ok(0);
    }
    let old_capacity = values.capacity();
    let capacity_bound = required
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let reserved = capacity_bound
        .saturating_sub(old_capacity)
        .saturating_mul(entry_bytes);
    memory.ensure_transient(transient_bytes.saturating_add(reserved))?;
    memory.reserve(reserved)?;
    values.reserve(additional);
    let actual = values
        .capacity()
        .saturating_sub(old_capacity)
        .saturating_mul(entry_bytes);
    if actual > reserved {
        return Err(ExecError::InvalidPhysicalPlan(
            "hash-map capacity exceeded its preflight bound",
        ));
    }
    memory.release(reserved - actual);
    Ok(actual)
}

fn reserve_hash_set_entries<T, S>(
    values: &mut HashSet<T, S>,
    additional: usize,
    entry_bytes: usize,
    transient_bytes: usize,
    memory: &MemoryTracker,
) -> Result<usize, ExecError>
where
    T: Eq + Hash,
    S: std::hash::BuildHasher,
{
    let required = values.len().saturating_add(additional);
    if required <= values.capacity() {
        return Ok(0);
    }
    let old_capacity = values.capacity();
    let capacity_bound = required
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let reserved = capacity_bound
        .saturating_sub(old_capacity)
        .saturating_mul(entry_bytes);
    memory.ensure_transient(transient_bytes.saturating_add(reserved))?;
    memory.reserve(reserved)?;
    values.reserve(additional);
    let actual = values
        .capacity()
        .saturating_sub(old_capacity)
        .saturating_mul(entry_bytes);
    if actual > reserved {
        return Err(ExecError::InvalidPhysicalPlan(
            "hash-set capacity exceeded its preflight bound",
        ));
    }
    memory.release(reserved - actual);
    Ok(actual)
}

fn estimated_batch_row_bytes(batch: &RecordBatch, row: usize) -> Result<usize, ExecError> {
    let heap_bytes = batch
        .columns()
        .iter()
        .try_fold(0_usize, |heap_bytes, column| {
            let value = column
                .value(row)
                .ok_or(ExecError::InvalidBatch("row is outside an input column"))?;
            Ok::<_, ExecError>(heap_bytes.saturating_add(value.heap_bytes()))
        })?;
    Ok(size_of::<Vec<Value>>()
        .saturating_add(batch.columns().len().saturating_mul(size_of::<Value>()))
        .saturating_add(heap_bytes)
        .saturating_add(2 * size_of::<usize>()))
}

/// One selected row as normalized values (the same key the Distinct
/// operator uses), for set-membership hashing.
/// `MySQL`'s default `cte_max_recursion_depth`.
pub const DEFAULT_CTE_MAX_RECURSION_DEPTH: u64 = 1000;

/// Drains an operator into the recursive accumulator, returning the fresh
/// delta. `UNION DISTINCT` recursion dedups on collation-normalized rows
/// while accumulating the original values.
fn drain_recursive_rows(
    operator: &mut PullOperator,
    distinct: bool,
    seen: &mut HashSet<Vec<Value>>,
    rows: &mut Vec<Vec<Value>>,
    memory: &MemoryTracker,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut delta = Vec::new();
    while let Some(batch) = operator.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        for row in batch.selection().selected_rows() {
            let values = batch_row(&batch, row)?;
            let row_bytes = estimated_row_payload_bytes(&values);
            memory.ensure_transient(batch_bytes.saturating_add(row_bytes.saturating_mul(3)))?;
            if distinct {
                let key: Vec<Value> = values
                    .iter()
                    .cloned()
                    .map(normalized_collation_value)
                    .collect();
                if seen.contains(&key) {
                    continue;
                }
                memory.reserve(row_bytes)?;
                seen.insert(key);
            }
            // Retained twice: once in the accumulator, once in the delta the
            // next iteration replays.
            memory.reserve(row_bytes.saturating_mul(2))?;
            rows.push(values.clone());
            delta.push(values);
        }
    }
    Ok(delta)
}

/// Serves the recursive working table from the current iteration's delta
/// and delegates every other scan to the base provider.
struct RecursiveWorkingProvider<'provider> {
    base: &'provider dyn ScanProvider,
    working: (DatabaseId, TableId),
    column_types: &'provider [DataType],
    delta: &'provider [Vec<Value>],
}

impl ScanProvider for RecursiveWorkingProvider<'_> {
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        if (scan.table.database_id, scan.table.table_id) != self.working {
            return self.base.open_scan(scan, memory_limit);
        }
        // Working scans never carry storage predicates (the optimizer keeps
        // their filters as Filter nodes); the projection maps 1-based
        // synthetic column IDs onto delta positions.
        let mut projected_types = Vec::with_capacity(scan.projected_column_ids.len());
        let mut positions = Vec::with_capacity(scan.projected_column_ids.len());
        for id in &scan.projected_column_ids {
            let position = usize::try_from(id.saturating_sub(1)).unwrap_or(usize::MAX);
            let data_type =
                self.column_types
                    .get(position)
                    .copied()
                    .ok_or(ExecError::InvalidPhysicalPlan(
                        "recursive working scan projects an unknown column",
                    ))?;
            positions.push(position);
            projected_types.push(data_type);
        }
        let rows: Vec<Vec<Value>> = self
            .delta
            .iter()
            .map(|row| {
                positions
                    .iter()
                    .map(|&position| row[position].clone())
                    .collect()
            })
            .collect();
        Ok(Box::new(RowsBatchStream {
            rows,
            cursor: 0,
            column_types: projected_types,
        }))
    }
}

/// In-memory batch stream over pre-materialized rows.
struct RowsBatchStream {
    rows: Vec<Vec<Value>>,
    cursor: usize,
    column_types: Vec<DataType>,
}

impl BatchStream for RowsBatchStream {
    fn next_batch(&mut self, _available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        if self.cursor >= self.rows.len() {
            return Ok(None);
        }
        let end = (self.cursor + DEFAULT_BATCH_ROWS).min(self.rows.len());
        let chunk = &self.rows[self.cursor..end];
        self.cursor = end;
        let columns = rows_to_columns(chunk, &self.column_types)?;
        Ok(Some(RecordBatch::new(chunk.len(), columns)?))
    }

    fn retained_bytes(&self) -> usize {
        self.rows[self.cursor..]
            .iter()
            .map(|row| estimated_row_payload_bytes(row))
            .fold(0, usize::saturating_add)
    }

    fn next_batch_memory_upper_bound(&self, _budget: usize) -> usize {
        let end = (self.cursor + DEFAULT_BATCH_ROWS).min(self.rows.len());
        self.rows[self.cursor..end]
            .iter()
            .map(|row| estimated_row_payload_bytes(row))
            .fold(0, usize::saturating_add)
            .saturating_mul(2)
    }
}

fn estimated_record_batch_bytes(rows: &[Vec<Value>], column_count: usize) -> usize {
    size_of::<RecordBatch>()
        .saturating_add(column_count.saturating_mul(size_of::<ColumnVector>().saturating_mul(2)))
        .saturating_add(
            rows.len()
                .saturating_mul(column_count)
                .saturating_mul(size_of::<Value>()),
        )
        .saturating_add(rows.len().div_ceil(64).saturating_mul(size_of::<u64>()))
        .saturating_add(
            rows.iter()
                .flat_map(|row| row.iter())
                .map(Value::heap_bytes)
                .fold(0_usize, usize::saturating_add),
        )
}

struct CrossJoinState {
    inputs: Vec<Vec<Vec<Value>>>,
    indexes: Vec<usize>,
    done: bool,
}

impl CrossJoinState {
    fn new(inputs: Vec<Vec<Vec<Value>>>) -> Self {
        let done = inputs.is_empty() || inputs.iter().any(Vec::is_empty);
        Self {
            indexes: vec![0; inputs.len()],
            inputs,
            done,
        }
    }

    fn next_rows(&mut self, maximum: usize) -> Vec<Vec<Value>> {
        let row_count = self.remaining_rows().min(maximum);
        let width = self
            .inputs
            .iter()
            .filter_map(|input| input.first())
            .map(Vec::len)
            .sum();
        let mut rows = Vec::with_capacity(row_count);
        while !self.done && rows.len() < maximum {
            let mut row = Vec::with_capacity(width);
            for (input, index) in self.inputs.iter().zip(&self.indexes) {
                row.extend(input[*index].iter().cloned());
            }
            rows.push(row);
            self.advance();
        }
        rows
    }

    fn remaining_rows(&self) -> usize {
        if self.done {
            return 0;
        }
        let total = self
            .inputs
            .iter()
            .map(Vec::len)
            .fold(1_usize, usize::saturating_mul);
        let rank = self
            .indexes
            .iter()
            .enumerate()
            .fold(0_usize, |rank, (position, index)| {
                let stride = self.inputs[position + 1..]
                    .iter()
                    .map(Vec::len)
                    .fold(1_usize, usize::saturating_mul);
                rank.saturating_add(index.saturating_mul(stride))
            });
        total.saturating_sub(rank)
    }

    fn next_batch_memory_upper_bound(&self, maximum: usize, column_count: usize) -> usize {
        let row_count = self.remaining_rows().min(maximum);
        if row_count == 0 {
            return 0;
        }
        let row_heap_upper = self
            .inputs
            .iter()
            .map(|input| {
                input
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(Value::heap_bytes)
                            .fold(0_usize, usize::saturating_add)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .fold(0_usize, usize::saturating_add);
        let temporary_rows = row_count
            .saturating_mul(size_of::<Vec<Value>>())
            .saturating_add(
                row_count
                    .saturating_mul(column_count)
                    .saturating_mul(size_of::<Value>()),
            )
            .saturating_add(row_count.saturating_mul(row_heap_upper));
        let projected_batch = size_of::<RecordBatch>()
            .saturating_add(
                column_count.saturating_mul(size_of::<ColumnVector>().saturating_mul(2)),
            )
            .saturating_add(
                row_count
                    .saturating_mul(column_count)
                    .saturating_mul(size_of::<Value>()),
            )
            .saturating_add(row_count.div_ceil(64).saturating_mul(size_of::<u64>()))
            .saturating_add(row_count.saturating_mul(row_heap_upper));
        temporary_rows.saturating_add(projected_batch)
    }

    fn advance(&mut self) {
        for position in (0..self.indexes.len()).rev() {
            self.indexes[position] += 1;
            if self.indexes[position] < self.inputs[position].len() {
                return;
            }
            self.indexes[position] = 0;
        }
        self.done = true;
    }
}

fn validate_scan_batch(batch: &RecordBatch, expected_types: &[DataType]) -> Result<(), ExecError> {
    if batch.row_count() > DEFAULT_BATCH_ROWS {
        return Err(ExecError::InvalidBatch(
            "scan batch exceeds the executor row target",
        ));
    }
    if batch.columns().len() != expected_types.len() {
        return Err(ExecError::InvalidBatch(
            "scan batch column count differs from its projection",
        ));
    }
    if batch
        .columns()
        .iter()
        .zip(expected_types)
        .any(|(column, expected)| column.data_type() != *expected)
    {
        return Err(ExecError::InvalidBatch(
            "scan batch column type differs from its projection",
        ));
    }
    Ok(())
}

/// Physical planning or execution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecError {
    /// A logical operator has no physical implementation yet.
    UnsupportedOperator(&'static str),
    /// A join predicate is not a single cross-input equality yet.
    UnsupportedJoinCondition,
    /// A cross join has no safe catalog cardinality estimate.
    CrossJoinCardinalityUnknown,
    /// A cross join exceeds the v1 Cartesian-product guard.
    CrossJoinGuardExceeded {
        /// Estimated result rows.
        estimated_rows: u64,
        /// Configured safety ceiling.
        limit: u64,
    },
    /// A scalar subquery produced more than one row.
    ScalarSubqueryRows {
        /// Actual result cardinality.
        rows: usize,
    },
    /// A physical plan violates an internal layout invariant.
    InvalidPhysicalPlan(&'static str),
    /// A source returned a malformed batch.
    InvalidBatch(&'static str),
    /// A compiled expression cannot find its stable input column.
    MissingColumn {
        /// Query-visible relation.
        relation: String,
        /// Source column.
        column: String,
    },
    /// An expression operation received an impossible bound type.
    InvalidExpressionType,
    /// Numeric evaluation exceeded the bound result type.
    NumericOverflow,
    /// Binary numeric coercion encountered invalid UTF-8.
    InvalidUtf8Number,
    /// A date/time value or operation is outside the supported `MySQL` range.
    InvalidDateTime,
    /// A recursive CTE did not converge within the iteration cap.
    RecursionDepthExceeded {
        /// Iteration cap (`MySQL`'s `cte_max_recursion_depth` default).
        limit: u64,
    },
    /// The configured statement execution deadline elapsed.
    QueryTimedOut,
    /// A source-specific failure.
    Source(String),
    /// The scan provider was configured twice for one stable table.
    DuplicateSnapshot {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
    },
    /// The scan provider has no pinned reader for a stable table.
    MissingSnapshot {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
    },
    /// The pinned reader and bound catalog used different schema versions.
    SnapshotSchemaChanged {
        /// Stable database identity.
        database_id: DatabaseId,
        /// Stable table identity.
        table_id: TableId,
        /// Version used while binding.
        expected: u32,
        /// Version pinned by storage.
        actual: u32,
    },
    /// The hard per-query memory cap would be exceeded.
    MemoryLimitExceeded {
        /// Bytes already reserved.
        used: usize,
        /// Additional transient or persistent bytes requested.
        requested: usize,
        /// Hard query limit.
        limit: usize,
    },
    /// A batch invariant was violated.
    Batch(BatchError),
}

impl fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOperator(operator) => {
                write!(formatter, "physical operator {operator} is not implemented")
            }
            Self::UnsupportedJoinCondition => formatter.write_str(
                "hash join requires one equality between left and right input expressions",
            ),
            Self::CrossJoinCardinalityUnknown => {
                formatter.write_str("cross join requires known catalog row counts for every input")
            }
            Self::CrossJoinGuardExceeded {
                estimated_rows,
                limit,
            } => write!(
                formatter,
                "cross join estimate {estimated_rows} exceeds safety limit {limit}"
            ),
            Self::ScalarSubqueryRows { rows } => {
                write!(formatter, "scalar subquery produced {rows} rows")
            }
            Self::RecursionDepthExceeded { limit } => write!(
                formatter,
                "recursive query aborted after {} iterations (cte_max_recursion_depth = {limit})",
                limit + 1
            ),
            Self::QueryTimedOut => formatter
                .write_str("query execution was interrupted after max_execution_time elapsed"),
            Self::InvalidPhysicalPlan(message) => {
                write!(formatter, "invalid physical plan: {message}")
            }
            Self::InvalidBatch(message) => write!(formatter, "invalid source batch: {message}"),
            Self::MissingColumn { relation, column } => {
                write!(formatter, "physical input is missing {relation}.{column}")
            }
            Self::InvalidExpressionType => {
                formatter.write_str("bound expression has an invalid physical type")
            }
            Self::NumericOverflow => formatter.write_str("numeric expression overflow"),
            Self::InvalidUtf8Number => {
                formatter.write_str("binary value is not valid UTF-8 for numeric coercion")
            }
            Self::InvalidDateTime => formatter.write_str("invalid MySQL date/time value"),
            Self::Source(message) => write!(formatter, "scan source failed: {message}"),
            Self::DuplicateSnapshot {
                database_id,
                table_id,
            } => write!(
                formatter,
                "snapshot provider repeats database {} table {}",
                database_id.get(),
                table_id.get()
            ),
            Self::MissingSnapshot {
                database_id,
                table_id,
            } => write!(
                formatter,
                "no pinned snapshot for database {} table {}",
                database_id.get(),
                table_id.get()
            ),
            Self::SnapshotSchemaChanged {
                database_id,
                table_id,
                expected,
                actual,
            } => write!(
                formatter,
                "snapshot schema changed for database {} table {}: bound version {expected}, pinned version {actual}",
                database_id.get(),
                table_id.get()
            ),
            Self::MemoryLimitExceeded {
                used,
                requested,
                limit,
            } => write!(
                formatter,
                "query memory limit exceeded: {used} bytes used, {requested} requested, {limit} limit"
            ),
            Self::Batch(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Batch(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BatchError> for ExecError {
    fn from(error: BatchError) -> Self {
        Self::Batch(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        mem::size_of,
        sync::Mutex,
    };

    fn drain_partitions(runs: &mut [super::GraceRun]) -> Vec<u64> {
        let mut ids = Vec::new();
        for run in runs {
            let mut reader = run.reader().expect("partition reader");
            while let Some((_, values)) = reader.next_entry().expect("partition entry") {
                match values.first() {
                    Some(pintail_types::Value::UInt64(id)) => ids.push(*id),
                    other => panic!("unexpected spilled row {other:?}"),
                }
            }
        }
        ids
    }

    #[test]
    fn scalar_join_null_extends_zero_matches_and_errors_on_two() {
        use pintail_sql::BoundJoinKind;
        use pintail_types::Value;

        let left = vec![Value::UInt64(7)];
        let mut match_index = 0;
        let (row, complete) =
            super::join_emit(BoundJoinKind::Scalar, &left, None, &mut match_index, 1)
                .expect("zero matches");
        assert_eq!(row, Some(vec![Value::UInt64(7), Value::Null]));
        assert!(complete);

        let matches = vec![
            vec![Value::Utf8("first".to_owned())],
            vec![Value::Utf8("second".to_owned())],
        ];
        assert!(matches!(
            super::join_emit(BoundJoinKind::Scalar, &left, Some(&matches), &mut 0, 1,),
            Err(super::ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    #[test]
    fn splitting_an_oversized_partition_keeps_every_row_and_spreads_the_keys() {
        use super::{
            GRACE_PARTITIONS, GraceJoin, JoinHashKey, MemoryTracker, split_grace_partition,
        };
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create(&memory).expect("grace state");
        let ids = (0..500_u64).collect::<Vec<_>>();
        for id in &ids {
            let key = JoinHashKey::NonNegativeInteger(*id);
            let row = vec![pintail_types::Value::UInt64(*id)];
            grace.build_files[0].append(&key, &row).expect("build");
            grace.probe_files[0].append(&key, &row).expect("probe");
        }

        split_grace_partition(&mut grace, 0, &memory).expect("split");
        assert_eq!(grace.build_files.len(), GRACE_PARTITIONS * 2);
        assert_eq!(grace.depths[GRACE_PARTITIONS], 1);

        let mut build = drain_partitions(&mut grace.build_files[GRACE_PARTITIONS..]);
        let mut probe = drain_partitions(&mut grace.probe_files[GRACE_PARTITIONS..]);
        build.sort_unstable();
        probe.sort_unstable();
        assert_eq!(build, ids, "no build row may be lost in a split");
        assert_eq!(probe, ids, "probe rows follow their keys");

        // The emptied original must never be served again.
        assert!(
            drain_partitions(&mut grace.build_files[0..1]).is_empty(),
            "the split partition is left empty"
        );
    }

    #[test]
    fn a_single_key_that_never_fits_reports_skew_at_the_depth_bound() {
        use super::{ExecError, GraceJoin, MAX_GRACE_DEPTH, MemoryTracker, split_grace_partition};
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create(&memory).expect("grace state");
        grace.depths[0] = MAX_GRACE_DEPTH;
        let error = split_grace_partition(&mut grace, 0, &memory).expect_err("depth bound");
        let ExecError::Source(message) = error else {
            panic!("expected a source error at the depth bound");
        };
        assert!(
            message.contains("one join key holds more rows than the ceiling"),
            "the message must name the cause, saw {message}"
        );
    }

    #[test]
    fn collation_keys_match_comparison_for_case_accents_and_expansions() {
        use std::cmp::Ordering;

        for (left, right) in [
            ("CaFé", "cafe"),
            ("é", "e\u{301}"),
            ("Straße", "STRASSE"),
            ("Ａ", "a"),
        ] {
            assert_eq!(super::compare_collated_text(left, right), Ordering::Equal);
            assert_eq!(
                super::normalized_collation_text(left),
                super::normalized_collation_text(right)
            );
        }
        assert_eq!(
            super::compare_collated_text("Émile", "Ernie"),
            Ordering::Less
        );
        assert!(
            super::normalized_collation_text("Émile") < super::normalized_collation_text("Ernie")
        );

        // utf8mb4_0900_ai_ci is a NO PAD collation: unlike older PAD SPACE
        // collations, a trailing space participates in comparison and keys.
        assert_ne!(super::compare_collated_text("a", "a "), Ordering::Equal);
        assert_ne!(
            super::normalized_collation_text("a"),
            super::normalized_collation_text("a ")
        );
    }

    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, parse_statement};
    use pintail_types::{Column, DataType, TableSchema, Value};

    use crate::{
        BatchStream, ColumnVector, ExecError, Execution, LogicalPlanner, MemoryTracker, Optimizer,
        PhysicalPlanner, RecordBatch, Scan, ScanProvider,
    };

    use super::{compare_decimal_text, dependent_subquery_memory_limit, reserve_vec_elements};

    struct StaticProvider {
        batches: Mutex<Vec<RecordBatch>>,
    }

    impl ScanProvider for StaticProvider {
        fn open_scan(
            &self,
            _scan: &Scan,
            _memory_limit: usize,
        ) -> Result<Box<dyn BatchStream>, ExecError> {
            let batches = self
                .batches
                .lock()
                .map_err(|_| ExecError::Source("test provider lock poisoned".to_owned()))?
                .clone();
            Ok(Box::new(StaticStream {
                batches: batches.into(),
            }))
        }
    }

    struct StaticStream {
        batches: VecDeque<RecordBatch>,
    }

    impl BatchStream for StaticStream {
        fn next_batch(
            &mut self,
            _available_memory: usize,
        ) -> Result<Option<RecordBatch>, ExecError> {
            Ok(self.batches.pop_front())
        }

        fn retained_bytes(&self) -> usize {
            0
        }

        fn next_batch_memory_upper_bound(&self, _budget: usize) -> usize {
            0
        }
    }

    fn physical(sql: &str) -> crate::PhysicalPlan {
        let table = catalog_table(1, "events", 3);
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(sql).expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound))).expect("physical")
    }

    fn catalog_table(id: u64, name: &str, rows: u64) -> TableEntry {
        TableEntry::new(
            TableId::new(id),
            name,
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(rows),
        )
        .expect("table")
    }

    fn source_batch() -> RecordBatch {
        RecordBatch::new(
            3,
            vec![
                ColumnVector::new(
                    DataType::UInt64,
                    vec![Value::UInt64(1), Value::UInt64(2), Value::UInt64(3)],
                )
                .expect("ids"),
                ColumnVector::new(
                    DataType::Utf8,
                    vec![
                        Value::Utf8("alpha".to_owned()),
                        Value::Utf8("Beta".to_owned()),
                        Value::Utf8("gamma".to_owned()),
                    ],
                )
                .expect("names"),
            ],
        )
        .expect("batch")
    }

    #[test]
    fn pulls_filter_project_and_limit_with_one_selection_mask() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![source_batch()]),
        };
        let plan = physical("SELECT name FROM events WHERE id > 1 LIMIT 1");
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");

        assert_eq!(execution.output_fields()[0].name, "name");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(batch.visible_row_count(), 1);
        assert_eq!(batch.selection().selected_rows().collect::<Vec<_>>(), [1]);
        assert_eq!(
            batch.column(0).and_then(|column| column.value(1)),
            Some(&Value::Utf8("Beta".to_owned()))
        );
        assert!(execution.next_batch().expect("end").is_none());
    }

    #[test]
    fn compares_canonical_decimals_by_numeric_value() {
        assert_eq!(
            compare_decimal_text("9.00", "10.00").expect("decimal comparison"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_decimal_text("-10.00", "-9.00").expect("negative decimal comparison"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_decimal_text("1.0", "1.00").expect("scale-insensitive comparison"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_decimal_text("-0.00", "0").expect("signed zero comparison"),
            std::cmp::Ordering::Equal
        );

        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT \
             CAST('9007199254740993' AS DECIMAL(16,0)) > \
             CAST('9007199254740992' AS DECIMAL(16,0)), \
             CAST('1.00' AS DECIMAL(3,2)) = CAST('1.0' AS DECIMAL(2,1)), \
             CAST('9007199254740993' AS DECIMAL(16,0)) > 9007199254740992",
        );
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024).expect("decimal comparison execution");
        let batch = execution.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Boolean(true))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Boolean(true))
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(0)),
            Some(&Value::Boolean(true))
        );
    }

    #[test]
    fn decimal_in_and_modulo_do_not_cross_the_f64_carrier() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT \
             CAST('9007199254740993' AS DECIMAL(16,0)) \
                 IN (9007199254740992), \
             CAST('9007199254740993' AS DECIMAL(16,0)) % 2",
        );
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024).expect("decimal set execution");
        let batch = execution.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Boolean(false))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8("1".to_owned()))
        );
    }

    #[test]
    fn decimal_arithmetic_keeps_unrounded_division_intermediates() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT (14620 / 9432456) / (24250 / 9432456), \
             (1 / 3) * 3, 1 / 3 / 3",
        );
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024).expect("decimal chain execution");
        let batch = execution.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8("0.60288653".to_owned()))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8("1.0000".to_owned()))
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(0)),
            Some(&Value::Utf8("0.11111111".to_owned()))
        );
    }

    #[test]
    fn decimal_grouping_distinct_and_extremes_stay_exact() {
        let table = TableEntry::new(
            TableId::new(1),
            "payments",
            TableSchema::new(
                1,
                vec![Column::new(
                    1,
                    "amount",
                    DataType::Decimal {
                        precision: 16,
                        scale: 0,
                    },
                    true,
                )],
            )
            .expect("schema"),
            TableStatistics::with_row_count(4),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let plan = |sql: &str| {
            let statement = parse_statement(sql).expect("parse");
            let bound = Binder::new(&catalog, Some("app"))
                .bind(&statement)
                .expect("bind");
            PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
                .expect("physical")
        };
        let amounts = || {
            ColumnVector::new(
                DataType::Decimal {
                    precision: 16,
                    scale: 0,
                },
                vec![
                    Value::Utf8("9007199254740992".to_owned()),
                    Value::Utf8("9007199254740993".to_owned()),
                    Value::Utf8("9007199254740993".to_owned()),
                    Value::Null,
                ],
            )
            .expect("amounts")
        };

        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(4, vec![amounts()]).expect("batch")]),
        };
        let mut aggregate = Execution::start(
            plan("SELECT COUNT(DISTINCT amount), MIN(amount), MAX(amount) FROM payments"),
            &provider,
            64 * 1024,
        )
        .expect("aggregate execution");
        let batch = aggregate.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::UInt64(2))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8("9007199254740992".to_owned()))
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(0)),
            Some(&Value::Utf8("9007199254740993".to_owned()))
        );

        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(4, vec![amounts()]).expect("batch")]),
        };
        let mut grouped = Execution::start(
            plan("SELECT amount, COUNT(*) FROM payments GROUP BY amount ORDER BY amount"),
            &provider,
            64 * 1024,
        )
        .expect("grouped execution");
        let batch = grouped.next_batch().expect("pull").expect("batch");
        assert_eq!(batch.visible_row_count(), 3);
        assert_eq!(
            batch.column(1).expect("counts").values(),
            [Value::UInt64(1), Value::UInt64(1), Value::UInt64(2)]
        );
    }

    #[test]
    fn temporal_range_offsets_apply_calendar_intervals() {
        let table = TableEntry::new(
            TableId::new(1),
            "events",
            TableSchema::new(
                1,
                vec![Column::new(1, "occurred_on", DataType::Date32, false)],
            )
            .expect("schema"),
            TableStatistics::with_row_count(3),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(
            "SELECT COUNT(*) OVER (ORDER BY occurred_on \
             RANGE BETWEEN INTERVAL 2 DAY PRECEDING AND CURRENT ROW) FROM events",
        )
        .expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind temporal range");
        let plan = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical");
        let dates = ColumnVector::new(
            DataType::Date32,
            ["2024-01-01", "2024-01-02", "2024-01-10"]
                .map(|date| Value::Utf8(date.to_owned()))
                .to_vec(),
        )
        .expect("dates");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(3, vec![dates]).expect("batch")]),
        };
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).expect("counts").values(),
            [Value::UInt64(1), Value::UInt64(2), Value::UInt64(1)]
        );
    }

    #[test]
    fn json_constructors_distinguish_json_columns_from_equal_text() {
        let table = TableEntry::new(
            TableId::new(1),
            "documents",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "document", DataType::Json, false),
                    Column::new(2, "text", DataType::Utf8, false),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(1),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(
            "SELECT JSON_OBJECT('json', document, 'text', text), \
                    JSON_ARRAY(document, text) \
             FROM documents",
        )
        .expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let plan = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical JSON constructors");
        let json_text = Value::Utf8(r#"{"x":1}"#.to_owned());
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    1,
                    vec![
                        ColumnVector::new(DataType::Json, vec![json_text.clone()])
                            .expect("JSON column"),
                        ColumnVector::new(DataType::Utf8, vec![json_text]).expect("text column"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("JSON execution");
        assert_eq!(execution.output_fields()[0].data_type, Some(DataType::Json));
        assert_eq!(execution.output_fields()[1].data_type, Some(DataType::Json));
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8(
                r#"{"json": {"x": 1}, "text": "{\"x\":1}"}"#.to_owned()
            ))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8(r#"[{"x": 1}, "{\"x\":1}"]"#.to_owned()))
        );
    }

    #[test]
    fn json_arrayagg_distinguishes_json_columns_from_equal_text() {
        let table = TableEntry::new(
            TableId::new(1),
            "documents",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "document", DataType::Json, true),
                    Column::new(2, "text", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(2),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement =
            parse_statement("SELECT JSON_ARRAYAGG(document), JSON_ARRAYAGG(text) FROM documents")
                .expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let plan = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical JSON aggregates");
        let json_text = Value::Utf8(r#"{"x":1}"#.to_owned());
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    2,
                    vec![
                        ColumnVector::new(DataType::Json, vec![json_text.clone(), Value::Null])
                            .expect("JSON column"),
                        ColumnVector::new(DataType::Utf8, vec![json_text, Value::Null])
                            .expect("text column"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("JSON execution");
        assert_eq!(execution.output_fields()[0].data_type, Some(DataType::Json));
        assert_eq!(execution.output_fields()[1].data_type, Some(DataType::Json));
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8(r#"[{"x": 1}, null]"#.to_owned()))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8(r#"["{\"x\":1}", null]"#.to_owned()))
        );
    }

    #[test]
    fn executes_constant_queries_without_a_scan() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical("SELECT 1 + 2 AS answer, NULL AS absent, '12x' + 1 AS coerced");
        let mut execution = Execution::start(plan, &provider, 4 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Int64(3))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Null)
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(0)),
            Some(&Value::float64(13.0))
        );
    }

    #[test]
    fn executes_mysql_string_conditional_pattern_and_cast_functions() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT CONCAT(LOWER('Hello'), '-', UPPER('world')), \
             SUBSTRING('abcdef', 2, 3), TRIM('  space  '), \
             CASE WHEN 0 THEN 'bad' ELSE 'ok' END, IFNULL(NULL, 'fallback'), \
             NULLIF(1, 1), 2 IN (1, 2, NULL), 3 NOT IN (1, 2, NULL), \
             'Alphabet' LIKE 'a%bet', 5 BETWEEN 2 AND 8, \
             CAST('12x' AS SIGNED), CONVERT('34x', SIGNED), \
             CONVERT('MiXeD' USING utf8mb4), ROUND(12.345, 2), ROUND(149, -2)",
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Utf8("hello-WORLD".to_owned()),
                Value::Utf8("bcd".to_owned()),
                Value::Utf8("space".to_owned()),
                Value::Utf8("ok".to_owned()),
                Value::Utf8("fallback".to_owned()),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
                Value::Boolean(true),
                Value::Boolean(true),
                Value::Int64(12),
                Value::Int64(34),
                Value::Utf8("MiXeD".to_owned()),
                Value::Utf8("12.35".into()),
                Value::float64(100.0),
            ]
        );
    }

    #[test]
    fn executes_mysql_conditional_type_coercion_exactly_and_lazily() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT IF(1, CAST('1.25' AS DECIMAL(3,2)), 0), \
             CASE WHEN 0 THEN 0 ELSE CAST('2.50' AS DECIMAL(3,2)) END, \
             IFNULL(NULL, CAST('3.75' AS DECIMAL(3,2))), \
             COALESCE(NULL, CAST('4.50' AS DECIMAL(3,2)), 0), \
             NULLIF(CAST('9007199254740993' AS DECIMAL(16,0)), 9007199254740992), \
             NULLIF(CAST('9007199254740993' AS DECIMAL(16,0)), 9007199254740993), \
             IF(1, 'selected', CAST('not-a-date' AS DATE))",
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Utf8("1.25".to_owned()),
                Value::Utf8("2.50".to_owned()),
                Value::Utf8("3.75".to_owned()),
                Value::Utf8("4.50".to_owned()),
                Value::Utf8("9007199254740993".to_owned()),
                Value::Null,
                Value::Utf8("selected".to_owned()),
            ]
        );
    }

    #[test]
    fn executes_lowered_constant_scalar_and_in_subqueries() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT (SELECT 1 + 2), \
             2 IN (SELECT 1 UNION ALL SELECT 2), \
             3 NOT IN (SELECT 1 UNION ALL SELECT NULL), \
             (SELECT 9 LIMIT 0)",
        );
        let mut execution = Execution::start(plan, &provider, 16 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Int64(3),
                Value::Boolean(true),
                Value::Null,
                Value::Null,
            ]
        );
    }

    #[test]
    fn executes_mysql_date_time_functions_and_interval_arithmetic() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT DATE('2024-02-29 12:34:56'), YEAR('2024-02-29'), \
             MONTH('2024-02-29'), DAY('2024-02-29'), HOUR('2024-02-29 12:34:56'), \
             MINUTE('2024-02-29 12:34:56'), SECOND('2024-02-29 12:34:56'), \
             DATE_FORMAT('2024-02-29 12:34:56', '%Y-%m-%d %H:%i:%s'), \
             DATE_ADD('2024-01-31', INTERVAL 1 MONTH), \
             DATE_SUB('2024-03-01', INTERVAL 1 DAY), \
             DATEDIFF('2024-03-05', '2024-03-01'), \
             FROM_UNIXTIME(UNIX_TIMESTAMP('2024-02-29 12:34:56'))",
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Utf8("2024-02-29".to_owned()),
                Value::UInt64(2024),
                Value::UInt64(2),
                Value::UInt64(29),
                Value::UInt64(12),
                Value::UInt64(34),
                Value::UInt64(56),
                Value::Utf8("2024-02-29 12:34:56".to_owned()),
                Value::Utf8("2024-02-29".to_owned()),
                Value::Utf8("2024-02-29".to_owned()),
                Value::Int64(4),
                Value::Utf8("2024-02-29 12:34:56".to_owned()),
            ]
        );
    }

    #[test]
    fn casts_mysql_time_intervals_with_fractional_precision() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT CAST('12:34:56.7896' AS TIME(3)), \
             CAST('-12:34:56.123456' AS TIME(6)), \
             CAST('1 02:03:04' AS TIME), CAST('1112' AS TIME), \
             CAST('2026-08-06 07:08:09.987654' AS TIME(3)), \
             CAST('850:00:00' AS TIME), CAST('not-a-time' AS TIME)",
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::Utf8("12:34:56.790".to_owned()),
                Value::Utf8("-12:34:56.123456".to_owned()),
                Value::Utf8("26:03:04".to_owned()),
                Value::Utf8("00:11:12".to_owned()),
                Value::Utf8("07:08:09.988".to_owned()),
                Value::Utf8("838:59:59".to_owned()),
                Value::Null,
            ]
        );
    }

    #[test]
    fn casts_only_valid_documents_to_canonical_json() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            r#"SELECT CAST('{"aa":1,"b":[true,null]}' AS JSON),
               JSON_TYPE(CAST('[1,2]' AS JSON))"#,
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8(r#"{"b": [true, null], "aa": 1}"#.to_owned()))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8("ARRAY".to_owned()))
        );

        let invalid = physical("SELECT CAST('not-json' AS JSON)");
        let mut execution = Execution::start(invalid, &provider, 32 * 1024).expect("execution");
        assert!(matches!(
            execution.next_batch(),
            Err(ExecError::InvalidExpressionType)
        ));
    }

    #[test]
    fn casts_mysql_year_with_numeric_string_and_temporal_rules() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT CAST(0 AS YEAR), CAST('0' AS YEAR), CAST(69 AS YEAR), \
             CAST(70 AS YEAR), CAST(1901 AS YEAR), CAST(2155 AS YEAR), \
             CAST(2156 AS YEAR), CAST(1944.5 AS YEAR), \
             CAST(CAST('2024-02-29' AS DATE) AS YEAR), \
             CAST('11:35:00' AS YEAR), CAST('1979aaa' AS YEAR), \
             CAST('not-a-year' AS YEAR)",
        );
        let mut execution = Execution::start(plan, &provider, 32 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let values = batch
            .columns()
            .iter()
            .map(|column| column.value(0).cloned().expect("value"))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                Value::UInt64(0),
                Value::UInt64(2000),
                Value::UInt64(2069),
                Value::UInt64(1970),
                Value::UInt64(1901),
                Value::UInt64(2155),
                Value::Null,
                Value::UInt64(1945),
                Value::UInt64(2024),
                Value::UInt64(2011),
                Value::UInt64(1979),
                Value::Null,
            ]
        );
    }

    #[test]
    fn enforces_the_hard_query_memory_cap() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![source_batch()]),
        };
        let plan = physical("SELECT id, name FROM events");
        let mut execution = Execution::start(plan, &provider, 1).expect("execution");
        assert!(matches!(
            execution.next_batch(),
            Err(ExecError::MemoryLimitExceeded { limit: 1, .. })
        ));
    }

    #[test]
    fn regexp_replace_output_obeys_the_query_memory_cap() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical("SELECT REGEXP_REPLACE(REPEAT('a', 1000), 'a', 'replacement')");
        let program_memory = crate::expression::REGEX_PROGRAM_MEMORY_UPPER_BOUND;
        assert!(matches!(
            Execution::start(plan.clone(), &provider, program_memory - 1),
            Err(ExecError::MemoryLimitExceeded { .. })
        ));
        let limit = program_memory + 4 * 1024;
        let mut execution = Execution::start(plan, &provider, limit).expect("execution");
        assert!(matches!(
            execution.next_batch(),
            Err(ExecError::MemoryLimitExceeded {
                limit: actual,
                ..
            }) if actual == limit
        ));
    }

    #[test]
    fn accounts_for_reserved_vector_capacity_before_pushes() {
        let memory = MemoryTracker::new(16 * 1024);
        let mut values = Vec::<String>::new();

        let reserved = reserve_vec_elements(&mut values, 1, 64, &memory).expect("reserve capacity");
        assert_eq!(reserved, values.capacity() * size_of::<String>(),);
        assert_eq!(memory.used(), reserved);

        values.push("x".to_owned());
        assert_eq!(
            reserve_vec_elements(&mut values, 1, 64, &memory).expect("reuse spare capacity"),
            0
        );
        assert_eq!(memory.used(), reserved);
    }

    #[test]
    fn elapsed_statement_deadline_interrupts_before_execution() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let deadline = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .expect("monotonic clock supports a one millisecond subtraction");
        assert!(matches!(
            Execution::start_with_deadline(
                physical("SELECT 1"),
                &provider,
                64 * 1024,
                Some(deadline),
            ),
            Err(ExecError::QueryTimedOut)
        ));
    }

    #[test]
    fn counts_materialized_subquery_results_against_the_parent_cap() {
        let names = ColumnVector::new(
            DataType::Utf8,
            vec![Value::Utf8("a".repeat(300)), Value::Utf8("b".repeat(300))],
        )
        .expect("names");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(2, vec![names]).expect("batch")]),
        };
        let plan = physical("SELECT 'needle' IN (SELECT name FROM events)");
        assert!(matches!(
            Execution::start(plan, &provider, 800),
            Err(ExecError::MemoryLimitExceeded { limit: 800, .. })
        ));
    }

    #[test]
    fn dependent_subquery_limit_keeps_the_outer_batch_live() {
        let batch = source_batch();
        let batch_bytes = batch.estimated_bytes();
        let memory = MemoryTracker::new(batch_bytes + 512);
        assert_eq!(
            dependent_subquery_memory_limit(&memory, &batch).expect("outer batch fits"),
            512
        );

        memory.reserve(513).expect("persistent state fits alone");
        assert!(matches!(
            dependent_subquery_memory_limit(&memory, &batch),
            Err(ExecError::MemoryLimitExceeded { .. })
        ));
    }

    #[test]
    fn scalar_and_exists_subqueries_stop_at_their_semantic_row_bounds() {
        let malformed_tail = RecordBatch::new(1, Vec::new()).expect("malformed tail");
        let exists_provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    1,
                    vec![
                        ColumnVector::new(DataType::Utf8, vec![Value::Utf8("present".to_owned())])
                            .expect("name"),
                    ],
                )
                .expect("first batch"),
                malformed_tail.clone(),
            ]),
        };
        let mut exists = Execution::start(
            physical("SELECT EXISTS (SELECT name FROM events)"),
            &exists_provider,
            8 * 1024,
        )
        .expect("EXISTS stops after one row");
        let batch = exists.next_batch().expect("pull").expect("batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Boolean(true))
        );

        let scalar_provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    2,
                    vec![
                        ColumnVector::new(
                            DataType::Utf8,
                            vec![
                                Value::Utf8("first".to_owned()),
                                Value::Utf8("second".to_owned()),
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("first batch"),
                malformed_tail,
            ]),
        };
        assert!(matches!(
            Execution::start(
                physical("SELECT (SELECT name FROM events)"),
                &scalar_provider,
                8 * 1024,
            ),
            Err(ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    #[test]
    fn rejects_source_batches_that_do_not_match_scan_projection() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(1, Vec::new()).expect("empty batch")]),
        };
        let plan = physical("SELECT name FROM events");
        let mut execution = Execution::start(plan, &provider, 4 * 1024).expect("execution");
        assert_eq!(
            execution.next_batch(),
            Err(ExecError::InvalidBatch(
                "scan batch column count differs from its projection"
            ))
        );
    }

    #[test]
    fn distinct_masks_duplicates_across_the_stream() {
        let names = ColumnVector::new(
            DataType::Utf8,
            vec![
                Value::Utf8("alpha".to_owned()),
                Value::Utf8("Alpha".to_owned()),
                Value::Utf8("Beta".to_owned()),
            ],
        )
        .expect("names");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(3, vec![names]).expect("batch")]),
        };
        let plan = physical("SELECT DISTINCT name FROM events");
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let names = batch
            .selection()
            .selected_rows()
            .map(|row| {
                batch
                    .column(0)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("name")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                Value::Utf8("alpha".to_owned()),
                Value::Utf8("Beta".to_owned()),
            ]
        );
        assert!(execution.next_batch().expect("end").is_none());
        assert!(execution.memory().used() > 0);
    }

    #[test]
    fn forced_standalone_distinct_spill_matches_the_memory_path() {
        let batches = (0..64)
            .map(|batch| {
                let names = (0..512)
                    .map(|row| {
                        let key = (batch * 512 + row) % 5_000;
                        Value::Utf8(format!("name-{key:05}-with-a-retained-payload"))
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |memory_limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(
                physical("SELECT DISTINCT name FROM events"),
                &provider,
                memory_limit,
            )
            .expect("execution");
            let mut values = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                for row in batch.selection().selected_rows() {
                    values.push(
                        batch
                            .column(0)
                            .and_then(|column| column.value(row))
                            .cloned()
                            .expect("distinct value"),
                    );
                }
            }
            (values, execution.spill_metrics())
        };
        let (memory, memory_spill) = execute(16 * 1024 * 1024);
        let (spilled, spill) = execute(1024 * 1024);
        assert_eq!(spilled, memory);
        assert_eq!(spilled.len(), 5_000);
        assert_eq!(memory_spill.files, 0);
        assert!(spill.files > 0, "the tight execution must use spill files");
        assert!(spill.written_bytes > 0);
    }

    #[test]
    fn forced_set_operation_spill_matches_multiset_semantics() {
        let batches = (0..64)
            .map(|batch| {
                let ids = (0..512)
                    .map(|row| Value::UInt64((batch * 512 + row) % 5_000))
                    .collect::<Vec<_>>();
                let names = ids
                    .iter()
                    .map(|id| match id {
                        Value::UInt64(id) => Value::Utf8(format!("name-{id:05}-payload")),
                        _ => unreachable!("generated ids are unsigned integers"),
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    ids.len(),
                    vec![
                        ColumnVector::new(DataType::UInt64, ids).expect("ids"),
                        ColumnVector::new(DataType::Utf8, names).expect("names"),
                    ],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |memory_limit, sql| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution =
                Execution::start(physical(sql), &provider, memory_limit).expect("execution");
            let mut counts = HashMap::<Vec<Value>, usize>::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                for row in batch.selection().selected_rows() {
                    let values = batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().expect("set value"))
                        .collect::<Vec<_>>();
                    *counts.entry(values).or_insert(0) += 1;
                }
            }
            (counts, execution.spill_metrics())
        };
        for sql in [
            "SELECT id, name FROM events WHERE id % 2 = 0 \
             INTERSECT SELECT id, name FROM events WHERE id % 3 = 0",
            "SELECT id, name FROM events WHERE id % 2 = 0 \
             INTERSECT ALL SELECT id, name FROM events WHERE id % 3 = 0",
            "SELECT id, name FROM events WHERE id % 2 = 0 \
             EXCEPT SELECT id, name FROM events WHERE id % 3 = 0",
            "SELECT id, name FROM events WHERE id % 2 = 0 \
             EXCEPT ALL SELECT id, name FROM events WHERE id % 3 = 0",
        ] {
            let (reference, memory_spill) = execute(16 * 1024 * 1024, sql);
            assert_eq!(memory_spill.files, 0);
            let (spilled, spill) = execute(1024 * 1024, sql);
            assert_eq!(spilled, reference);
            assert!(spill.files > 0, "the tight execution must use spill files");
            assert!(spill.written_bytes > 0);
        }
    }

    #[test]
    fn hash_joins_mysql_comparable_mixed_scalar_keys() {
        for key in ["CAST(e.id AS DOUBLE)", "CAST(e.id AS CHAR)"] {
            let ids = ColumnVector::new(
                DataType::UInt64,
                vec![Value::UInt64(1), Value::UInt64(2), Value::UInt64(3)],
            )
            .expect("ids");
            let provider = StaticProvider {
                batches: Mutex::new(vec![RecordBatch::new(3, vec![ids]).expect("batch")]),
            };
            let plan = physical(&format!(
                "SELECT e.id AS event_id, u.id AS user_id \
                 FROM events e INNER JOIN events u ON {key} = u.id \
                 ORDER BY event_id"
            ));
            let mut execution =
                Execution::start(plan, &provider, 64 * 1024).expect("mixed-key execution");
            let batch = execution.next_batch().expect("pull").expect("result batch");
            assert_eq!(
                batch.column(0).expect("left ids").values(),
                [Value::UInt64(1), Value::UInt64(2), Value::UInt64(3)]
            );
            assert_eq!(
                batch.column(1).expect("right ids").values(),
                [Value::UInt64(1), Value::UInt64(2), Value::UInt64(3)]
            );
        }
    }

    #[test]
    fn hash_joins_decimal_keys_after_exact_scale_coercion() {
        let table = TableEntry::new(
            TableId::new(1),
            "payments",
            TableSchema::new(
                1,
                vec![Column::new(
                    1,
                    "amount",
                    DataType::Decimal {
                        precision: 20,
                        scale: 2,
                    },
                    false,
                )],
            )
            .expect("schema"),
            TableStatistics::with_row_count(2),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(
            "SELECT p.amount FROM payments p \
             JOIN payments q ON p.amount = q.amount ORDER BY p.amount",
        )
        .expect("parse");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        let plan = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)))
            .expect("physical decimal equi-join");
        let amounts = ColumnVector::new(
            DataType::Decimal {
                precision: 20,
                scale: 2,
            },
            vec![
                Value::Utf8("1.00".to_owned()),
                Value::Utf8("2.00".to_owned()),
            ],
        )
        .expect("amounts");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(2, vec![amounts]).expect("batch")]),
        };
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024).expect("decimal-key execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("amounts").values(),
            [
                Value::Utf8("1.00".to_owned()),
                Value::Utf8("2.00".to_owned())
            ]
        );
    }

    #[test]
    fn aggregates_inner_joins_without_materializing_joined_rows() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    3,
                    vec![
                        ColumnVector::new(
                            DataType::UInt64,
                            vec![Value::UInt64(1), Value::UInt64(1), Value::UInt64(2)],
                        )
                        .expect("ids"),
                        ColumnVector::new(
                            DataType::Utf8,
                            vec![
                                Value::Utf8("alpha".to_owned()),
                                Value::Utf8("Alpha".to_owned()),
                                Value::Utf8("beta".to_owned()),
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let plan = physical(
            "SELECT u.name, COUNT(*) AS rows, SUM(e.id) AS total, MIN(e.name) AS first_name \
             FROM events e INNER JOIN events u ON e.id = u.id \
             GROUP BY u.name ORDER BY u.name",
        );
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");

        assert_eq!(batch.visible_row_count(), 2);
        assert_eq!(
            batch.column(0).expect("names").values(),
            [
                Value::Utf8("alpha".to_owned()),
                Value::Utf8("beta".to_owned())
            ]
        );
        assert_eq!(
            batch.column(1).expect("counts").values(),
            [Value::UInt64(4), Value::UInt64(1)]
        );
        assert_eq!(
            batch.column(2).expect("totals").values(),
            [Value::UInt64(4), Value::UInt64(2)]
        );
        assert_eq!(
            batch.column(3).expect("minimums").values(),
            [
                Value::Utf8("alpha".to_owned()),
                Value::Utf8("beta".to_owned())
            ]
        );
        assert!(execution.next_batch().expect("end").is_none());
    }

    #[test]
    fn executes_case_insensitive_grouping_distinct_aggregates_and_having() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    3,
                    vec![
                        ColumnVector::new(
                            DataType::UInt64,
                            vec![Value::UInt64(1), Value::UInt64(2), Value::UInt64(2)],
                        )
                        .expect("ids"),
                        ColumnVector::new(
                            DataType::Utf8,
                            vec![
                                Value::Utf8("alpha".to_owned()),
                                Value::Utf8("Alpha".to_owned()),
                                Value::Utf8("beta".to_owned()),
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let plan = physical(
            "SELECT name, COUNT(*) AS rows, SUM(DISTINCT id) AS total, \
             COUNT(DISTINCT id) AS unique_ids, AVG(id) AS average_id, \
             GROUP_CONCAT(DISTINCT id) AS ids, MIN(name), MAX(name) \
             FROM events GROUP BY name HAVING COUNT(*) > 1",
        );
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");

        assert_eq!(batch.visible_row_count(), 1);
        let row = batch
            .selection()
            .selected_rows()
            .next()
            .expect("selected aggregate row");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(row)),
            Some(&Value::Utf8("alpha".to_owned()))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(row)),
            Some(&Value::UInt64(2))
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(row)),
            Some(&Value::UInt64(3))
        );
        assert_eq!(
            batch.column(3).and_then(|column| column.value(row)),
            Some(&Value::UInt64(2))
        );
        assert_eq!(
            batch.column(4).and_then(|column| column.value(row)),
            // MySQL AVG over integers is DECIMAL widened by four fraction
            // digits, carried as canonical text.
            Some(&Value::Utf8("1.5000".to_owned()))
        );
        assert_eq!(
            batch.column(5).and_then(|column| column.value(row)),
            Some(&Value::Utf8("1,2".to_owned()))
        );
        assert_eq!(
            batch.column(6).and_then(|column| column.value(row)),
            Some(&Value::Utf8("alpha".to_owned()))
        );
        assert_eq!(
            batch.column(7).and_then(|column| column.value(row)),
            Some(&Value::Utf8("alpha".to_owned()))
        );
        assert!(execution.next_batch().expect("end").is_none());
    }

    #[test]
    fn group_concat_honors_session_byte_limit_and_reports_truncation() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    3,
                    vec![
                        ColumnVector::new(
                            DataType::Utf8,
                            vec![
                                Value::Utf8("é".to_owned()),
                                Value::Utf8("é".to_owned()),
                                Value::Utf8("é".to_owned()),
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        super::set_session_group_concat_max_len(Some(5));
        let plan = physical("SELECT GROUP_CONCAT(name SEPARATOR '') FROM events");
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8("éé".to_owned()))
        );
        assert_eq!(super::take_session_group_concat_warnings(), 1);
        super::set_session_group_concat_max_len(None);
    }

    #[test]
    fn aggregate_multi_expression_distinct_and_concat_follow_mysql_null_rules() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![
                RecordBatch::new(
                    4,
                    vec![
                        ColumnVector::new(
                            DataType::UInt64,
                            vec![
                                Value::UInt64(1),
                                Value::UInt64(1),
                                Value::UInt64(2),
                                Value::UInt64(3),
                            ],
                        )
                        .expect("ids"),
                        ColumnVector::new(
                            DataType::Utf8,
                            vec![
                                Value::Utf8("Alpha".to_owned()),
                                Value::Utf8("alpha".to_owned()),
                                Value::Utf8("beta".to_owned()),
                                Value::Null,
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let plan = physical(
            "SELECT COUNT(DISTINCT id, name), \
             GROUP_CONCAT(id, ':', name ORDER BY id SEPARATOR '|') FROM events",
        );
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::UInt64(2))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Utf8("1:Alpha|1:alpha|2:beta".to_owned()))
        );
    }

    #[test]
    fn global_aggregates_emit_sql_empty_input_results() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical("SELECT COUNT(*) AS rows, SUM(id) AS total FROM events");
        let mut execution = Execution::start(plan, &provider, 16 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");

        assert_eq!(batch.row_count(), 1);
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::UInt64(0))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::Null)
        );
    }

    #[test]
    fn count_star_uses_catalog_metadata_without_opening_a_scan() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical("SELECT COUNT(*) AS rows FROM events");
        let mut execution = Execution::start(plan, &provider, 4 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::UInt64(3))
        );
    }

    #[test]
    fn executes_case_insensitive_top_k_sort_with_mysql_null_ordering() {
        let names = ColumnVector::new(
            DataType::Utf8,
            vec![
                Value::Utf8("alpha".to_owned()),
                Value::Null,
                Value::Utf8("Gamma".to_owned()),
            ],
        )
        .expect("names");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(3, vec![names]).expect("batch")]),
        };
        let plan = physical("SELECT name AS label FROM events ORDER BY label DESC LIMIT 2");
        let crate::PhysicalPlan::Limit { input, .. } = &plan else {
            panic!("limit plan");
        };
        assert!(matches!(
            input.as_ref(),
            crate::PhysicalPlan::Sort { top_k: Some(2), .. }
        ));

        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("label").values(),
            [
                Value::Utf8("Gamma".to_owned()),
                Value::Utf8("alpha".to_owned()),
            ]
        );
    }

    #[test]
    fn top_k_releases_losing_rows_between_input_batches() {
        let batches = (1..=20)
            .rev()
            .map(|rank| {
                let name = format!("{rank:02}-{}", "x".repeat(200));
                let names =
                    ColumnVector::new(DataType::Utf8, vec![Value::Utf8(name)]).expect("names");
                RecordBatch::new(1, vec![names]).expect("batch")
            })
            .collect();
        let provider = StaticProvider {
            batches: Mutex::new(batches),
        };
        let plan = physical("SELECT name FROM events ORDER BY name LIMIT 2");
        let mut execution =
            Execution::start(plan, &provider, 2 * 1024).expect("bounded top-K execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        let prefixes = batch
            .column(0)
            .expect("names")
            .values()
            .iter()
            .map(|value| match value {
                Value::Utf8(value) => value[..2].to_owned(),
                _ => panic!("text result"),
            })
            .collect::<Vec<_>>();
        assert_eq!(prefixes, ["01", "02"]);
    }

    #[test]
    fn top_k_does_not_preallocate_a_user_supplied_limit() {
        let names = ColumnVector::new(
            DataType::Utf8,
            vec![
                Value::Utf8("beta".to_owned()),
                Value::Utf8("alpha".to_owned()),
            ],
        )
        .expect("names");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(2, vec![names]).expect("batch")]),
        };
        let plan = physical("SELECT name FROM events ORDER BY name LIMIT 18446744073709551615");
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("names").values(),
            [
                Value::Utf8("alpha".to_owned()),
                Value::Utf8("beta".to_owned())
            ]
        );
    }

    #[test]
    fn full_sort_places_nulls_first_for_mysql_ascending_order() {
        let names = ColumnVector::new(
            DataType::Utf8,
            vec![
                Value::Utf8("beta".to_owned()),
                Value::Null,
                Value::Utf8("Alpha".to_owned()),
            ],
        )
        .expect("names");
        let provider = StaticProvider {
            batches: Mutex::new(vec![RecordBatch::new(3, vec![names]).expect("batch")]),
        };
        let plan = physical("SELECT name FROM events ORDER BY name");
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("name").values(),
            [
                Value::Null,
                Value::Utf8("Alpha".to_owned()),
                Value::Utf8("beta".to_owned()),
            ]
        );
    }

    #[test]
    fn streams_union_all_branches_before_outer_sort_and_limit() {
        let provider = StaticProvider {
            batches: Mutex::new(Vec::new()),
        };
        let plan = physical(
            "SELECT 3 AS value UNION ALL SELECT 1 UNION ALL SELECT 2 ORDER BY value LIMIT 2",
        );
        let mut execution = Execution::start(plan, &provider, 16 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("value").values(),
            [Value::Int64(1), Value::Int64(2)]
        );
        assert!(execution.next_batch().expect("end").is_none());
    }

    #[test]
    fn rejects_cross_joins_above_the_cardinality_guard() {
        let database = DatabaseEntry::new(
            DatabaseId::new(1),
            "app",
            [
                catalog_table(1, "events", 2_000),
                catalog_table(2, "users", 2_000),
            ],
        )
        .expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement =
            parse_statement("SELECT events.id FROM events, users").expect("parse query");
        let bound = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind query");
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        assert_eq!(
            PhysicalPlanner::plan(logical),
            Err(ExecError::CrossJoinGuardExceeded {
                estimated_rows: 4_000_000,
                limit: crate::MAX_CROSS_JOIN_ROWS
            })
        );
    }
}
