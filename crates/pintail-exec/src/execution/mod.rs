mod aggregate;
pub use aggregate::take_fold_phase_timings;
mod budget;
mod columnar_sort;
mod error;
pub(crate) mod gather;
mod join;
mod key_lookup;
pub(crate) mod membership;
mod memo;
mod morsel;
mod order;
mod points;
mod sort;
mod two_pass;
mod watchdog;
mod window;
pub use watchdog::{ExecutionCancellation, cancel_query_under_memory_pressure};

pub(crate) use aggregate::compare_decimal_text;
/// Test-only accessor for the SMA fold-hit counter the storage tests assert on.
#[cfg(test)]
pub(crate) use aggregate::sma_fold_hits;
pub use budget::{MemoryBudget, MemoryScope};
pub use error::ExecError;
pub use join::compare_collated_text;

use aggregate::{AggregateState, CompiledAggregate, build_hash_aggregate};
use join::{
    HashJoinState, JoinHashKey, ProbePrefetch, build_hash_join_state, execute_nested_loop_join,
    next_hash_join_batch, normalized_collation_value, normalized_join_key, prefetch_probe,
    probe_prefetch_applies,
};
use memo::DependentMemo;
use sort::{
    DistinctRows, SetOpRows, SortedRows, build_distinct, build_set_operation, build_sort,
    compare_sort_values,
};
use window::{CompiledWindow, build_window};

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    hash::Hash,
    mem::{size_of, size_of_val},
    time::{Duration, Instant},
};

const HASH_ENTRY_OVERHEAD: usize = 3 * size_of::<usize>();

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundJoinKind, BoundOrderKey,
    BoundProjection, BoundQuery, BoundWindow, ScalarFunction, WindowFunction,
};
use pintail_types::{DataType, Value};

use crate::collation::Collation;

use crate::{
    ColumnVector, DEFAULT_BATCH_ROWS, LogicalPlan, LogicalPlanner, Optimizer, RecordBatch, Scan,
    expression::{CompiledExpr, bound_regex_memory_upper_bound, predicate_truth},
    spill,
};

const DEFAULT_GROUP_CONCAT_MAX_LEN: usize = 1024;

thread_local! {
    static SESSION_GROUP_CONCAT_MAX_LEN: std::cell::Cell<usize> =
        const { std::cell::Cell::new(DEFAULT_GROUP_CONCAT_MAX_LEN) };
    static SESSION_GROUP_CONCAT_WARNINGS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
    static SESSION_DIVISION_WARNINGS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
    static SESSION_CTE_MAX_RECURSION_DEPTH: std::cell::Cell<u64> =
        const { std::cell::Cell::new(DEFAULT_CTE_MAX_RECURSION_DEPTH) };
    static EXECUTION_CANCELLATION: std::cell::RefCell<Option<ExecutionCancellation>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs synchronous query setup/execution with a cancellation handle that is
/// captured by every [`MemoryTracker`] opened in the scope.
pub fn with_execution_cancellation<T>(
    cancellation: ExecutionCancellation,
    operation: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<ExecutionCancellation>);

    impl Drop for Restore {
        fn drop(&mut self) {
            EXECUTION_CANCELLATION.with(|current| {
                current.replace(self.0.take());
            });
        }
    }

    let previous = EXECUTION_CANCELLATION.with(|current| current.replace(Some(cancellation)));
    let _restore = Restore(previous);
    operation()
}

/// Installs the current connection's `group_concat_max_len` on this
/// synchronous execution thread. `None` restores `MySQL`'s default of 1024.
pub fn set_session_group_concat_max_len(limit: Option<usize>) {
    SESSION_GROUP_CONCAT_MAX_LEN.set(limit.unwrap_or(DEFAULT_GROUP_CONCAT_MAX_LEN).max(4));
    SESSION_GROUP_CONCAT_WARNINGS.set(0);
    SESSION_DIVISION_WARNINGS.set(0);
}

/// The `group_concat_max_len` this thread's executions observe.
#[must_use]
pub fn session_group_concat_max_len() -> usize {
    SESSION_GROUP_CONCAT_MAX_LEN.get()
}

/// The recursive-CTE iteration cap this thread's executions observe.
#[must_use]
pub fn session_cte_max_recursion_depth() -> u64 {
    SESSION_CTE_MAX_RECURSION_DEPTH.get()
}

/// Takes the number of `GROUP_CONCAT` results truncated by the last statement.
#[must_use]
pub fn take_session_group_concat_warnings() -> u64 {
    SESSION_GROUP_CONCAT_WARNINGS.replace(0)
}

/// Counts one division by zero answered with NULL, `MySQL`'s warning 1365.
pub(crate) fn note_division_by_zero() {
    note_divisions_by_zero(1);
}

/// Counts `count` divisions by zero answered with NULL.
pub(crate) fn note_divisions_by_zero(count: u64) {
    SESSION_DIVISION_WARNINGS.set(SESSION_DIVISION_WARNINGS.get().saturating_add(count));
}

/// Takes the number of divisions by zero the last statement answered with
/// NULL, each `MySQL`'s warning 1365 under `ERROR_FOR_DIVISION_BY_ZERO`.
#[must_use]
pub fn take_session_division_warnings() -> u64 {
    SESSION_DIVISION_WARNINGS.replace(0)
}

/// Installs the recursive-CTE iteration cap for queries on this thread.
/// `None` restores `MySQL`'s default of 1000 iterations.
pub fn set_session_cte_max_recursion_depth(limit: Option<u64>) {
    SESSION_CTE_MAX_RECURSION_DEPTH.set(limit.unwrap_or(DEFAULT_CTE_MAX_RECURSION_DEPTH));
}

/// The process-wide memory budget every query draws from.
///
/// Separate from the per-query ceiling: that one bounds a single query, this
/// one bounds their sum. Zero (the default) is unbounded, so a caller that
/// never configures a budget keeps exactly the previous behaviour.
static SHARED_MEMORY_BUDGET: std::sync::OnceLock<MemoryBudget> = std::sync::OnceLock::new();

/// Installs the process-wide memory budget. Called once at startup; later
/// calls are ignored so a stray caller cannot loosen a configured budget.
pub fn init_shared_memory_budget(limit: usize) {
    // Set in place rather than raced: a reader that arrived first would
    // otherwise have pinned the limit at zero - unbounded - for the process.
    shared_memory_budget().set_limit(limit);
}

/// The process-wide budget, unbounded when startup configured none.
#[must_use]
pub fn shared_memory_budget() -> &'static MemoryBudget {
    SHARED_MEMORY_BUDGET.get_or_init(|| MemoryBudget::new(0))
}

/// Client-visible output field metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputField {
    /// Result-column name.
    pub name: String,
    /// Result scalar type, or `None` for an untyped `NULL`.
    pub data_type: Option<DataType>,
    /// Whether the result can be `NULL`.
    pub nullable: bool,
    /// Whether the result is a spatial column, advertised on the wire as
    /// `MYSQL_TYPE_GEOMETRY` so clients decode it as `MySQL` does.
    pub geometry: bool,
    /// Whether the field is a source `TIMESTAMP`; drives the wire type byte.
    pub timestamp: bool,
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
        /// Catalog-derived result cardinality, when every input's row count
        /// is known; a hint for EXPLAIN only.
        estimated_rows: Option<u64>,
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
        /// Whether each key, the primary first and then the extras, is a
        /// null-safe `<=>` equality, under which NULL matches NULL.
        null_safe: Vec<bool>,
        /// Build-side key.
        right_key: BoundExpr,
        /// Row estimate of the probe input when statistics give one; it
        /// decides whether the probe side is read ahead of the build so
        /// its keys can filter the build.
        probe_estimate: Option<u64>,
        /// Row estimate of the build input, when statistics give one. The
        /// read-ahead pays only when the probe is much smaller than the
        /// build; a probe as large as the build filters nothing.
        build_estimate: Option<u64>,
        /// ON conjuncts that compare both inputs with something other than
        /// equality, applied to each candidate pair after the hash match.
        residual: Option<BoundExpr>,
    },
    /// Bounded nested-loop join for ON predicates containing a dependent
    /// subquery that cannot be represented as hash keys alone.
    /// A join that streams its driving input in that table's key order and
    /// finds each row's match by the other table's primary key. The output
    /// keeps the driving order, so a limit above it stops both inputs early.
    KeyLookupJoin {
        left: Box<Self>,
        right: Box<Self>,
        kind: BoundJoinKind,
        /// Whether the left input drives; the other is found by its key.
        driving_left: bool,
        driving_key: BoundExpr,
        lookup_key: BoundExpr,
        residual: Option<BoundExpr>,
    },
    NestedLoopJoin {
        /// Left input.
        left: Box<Self>,
        /// Right input.
        right: Box<Self>,
        /// Join semantics.
        kind: BoundJoinKind,
        /// Hashable equalities every accepted pair satisfies, oriented left
        /// then right. The right input is bucketed by them so a left row
        /// meets only the rows its keys reach; empty tests every pair.
        keys: Vec<(BoundExpr, BoundExpr)>,
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
        /// Per projected column: the collation it dedupes under; `None`
        /// falls back to the plan collation.
        key_collations: Vec<Option<String>>,
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
    #[allow(clippy::too_many_lines)] // one arm per plan node
    pub fn output_fields(&self) -> Vec<OutputField> {
        match self {
            Self::Project { expressions, .. } => expressions
                .iter()
                .map(|expression| OutputField {
                    name: expression.name.clone(),
                    data_type: expression.expr.data_type,
                    nullable: expression.expr.nullable,
                    geometry: matches!(
                        &expression.expr.kind,
                        pintail_sql::BoundExprKind::Column(column) if column.geometry
                    ),
                    timestamp: expression.expr.is_source_timestamp(),
                })
                .collect(),
            Self::SetOp { left: input, .. }
            | Self::Recursive { anchor: input, .. }
            | Self::Filter { input, .. }
            | Self::Distinct { input, .. }
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
                    geometry: false,
                    timestamp: false,
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
                    geometry: matches!(
                        &expression.kind,
                        pintail_sql::BoundExprKind::Column(column) if column.geometry
                    ),
                    timestamp: expression.is_source_timestamp(),
                })
                .chain(aggregates.iter().map(|aggregate| OutputField {
                    name: String::new(),
                    data_type: aggregate.data_type,
                    nullable: aggregate.nullable,
                    geometry: false,
                    timestamp: false,
                }))
                .collect(),
            Self::CrossJoin { inputs, .. } => inputs.iter().flat_map(Self::output_fields).collect(),
            Self::UnionAll { inputs } => inputs
                .first()
                .map_or_else(Vec::new, PhysicalPlan::output_fields),
            Self::HashJoin {
                left, right, kind, ..
            }
            | Self::KeyLookupJoin {
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
                    geometry: column.geometry,
                    timestamp: column.timestamp,
                })
                .collect(),
            Self::Derived { columns, .. } => columns
                .iter()
                .map(|column| OutputField {
                    name: column.name.clone(),
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                    geometry: column.geometry,
                    timestamp: column.timestamp,
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
    pub fn plan(logical: LogicalPlan, collation: Collation) -> Result<PhysicalPlan, ExecError> {
        match logical {
            LogicalPlan::Empty => Ok(PhysicalPlan::Empty),
            LogicalPlan::OneRow => Ok(PhysicalPlan::OneRow),
            LogicalPlan::Scan(scan) => Ok(PhysicalPlan::Scan(scan)),
            LogicalPlan::Derived { input, columns } => Ok(PhysicalPlan::Derived {
                input: Box::new(Self::plan(*input, collation)?),
                columns,
            }),
            LogicalPlan::Filter { input, predicate } => Ok(PhysicalPlan::Filter {
                input: Box::new(Self::plan(*input, collation)?),
                predicate,
            }),
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => Ok(PhysicalPlan::HashAggregate {
                input: Box::new(Self::plan(*input, collation)?),
                group_by,
                aggregates,
            }),
            LogicalPlan::Project { input, expressions } => Ok(PhysicalPlan::Project {
                input: Box::new(Self::plan(*input, collation)?),
                expressions,
            }),
            LogicalPlan::Limit { input, limit } => {
                plan_limit(*input, limit.offset, limit.count, collation)
            }
            LogicalPlan::CrossJoin { inputs } => {
                // Only a hint for EXPLAIN. A runaway product is stopped by the
                // query's memory ceiling and time limit, as MySQL stops it.
                let estimated_rows = inputs.iter().try_fold(1_u64, |rows, input| {
                    rows.checked_mul(input.estimated_rows()?)
                });
                Ok(PhysicalPlan::CrossJoin {
                    inputs: inputs
                        .into_iter()
                        .map(|input| Self::plan(input, collation))
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
                left: Box::new(Self::plan(*left, collation)?),
                right: Box::new(Self::plan(*right, collation)?),
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
                anchor: Box::new(Self::plan(*anchor, collation)?),
                member: Box::new(Self::plan(*member, collation)?),
            }),
            LogicalPlan::UnionAll { inputs } => Ok(PhysicalPlan::UnionAll {
                inputs: inputs
                    .into_iter()
                    .map(|input| Self::plan(input, collation))
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            LogicalPlan::Distinct {
                input,
                key_collations,
            } => Ok(PhysicalPlan::Distinct {
                input: Box::new(Self::plan(*input, collation)?),
                key_collations,
            }),
            LogicalPlan::Window {
                input,
                windows,
                outputs,
            } => Ok(PhysicalPlan::Window {
                input: Box::new(Self::plan(*input, collation)?),
                windows,
                outputs,
            }),
            LogicalPlan::Sort { input, keys, trim } => {
                // A scan already in the sort's order needs no sort.
                match order::scan_ordered_input(Self::plan(*input, collation)?, &keys, trim) {
                    (ordered, true) => Ok(ordered),
                    (input, false) => Ok(PhysicalPlan::Sort {
                        input: Box::new(input),
                        keys,
                        top_k: None,
                        trim,
                    }),
                }
            }
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
                        .and_then(|rows| rows.checked_mul(right.estimated_rows()?));
                    return Ok(PhysicalPlan::CrossJoin {
                        inputs: vec![
                            Self::plan(*left, collation)?,
                            Self::plan(*right, collation)?,
                        ],
                        estimated_rows,
                    });
                }
                let condition = condition.ok_or(ExecError::UnsupportedJoinCondition)?;
                if expression_has_subquery(&condition) {
                    let keys = dependent_join_keys(&condition, &left, &right, collation);
                    return Ok(PhysicalPlan::NestedLoopJoin {
                        left: Box::new(Self::plan(*left, collation)?),
                        right: Box::new(Self::plan(*right, collation)?),
                        kind,
                        keys,
                        condition,
                    });
                }
                // An ON clause routinely carries ordinary predicates beside
                // its join keys. Apply those to one input instead of letting
                // a single non-key conjunct reject the whole join.
                let original_condition = condition.clone();
                let (condition, left_filter, right_filter) =
                    split_join_condition(condition, &left, &right, kind);
                let Some(condition) = condition else {
                    // No conjunct spans the two inputs at all: a pure theta
                    // shape like ON a.x > 5, or an ON clause folded to a
                    // constant. Each side's own conjuncts filter it first,
                    // and the nested loop tests what is left pair by pair.
                    return plan_theta_join(
                        logically_filtered(left, left_filter),
                        logically_filtered(right, right_filter),
                        kind,
                        original_condition,
                        collation,
                    );
                };
                // An ON clause may also compare the two inputs with something
                // that is not equality. The hash join still runs on the
                // equalities; the rest becomes a residual tested per candidate
                // pair. Refusing the whole join for one such conjunct rejected
                // valid MySQL that names a perfectly good join key.
                let (equalities, residual) =
                    split_join_residual(&condition, &left, &right, collation);
                let Some(condition) = equalities else {
                    // Spanning conjuncts exist but none is a hashable
                    // equality - a range join. Same nested-loop fallback.
                    return plan_theta_join(left, right, kind, original_condition, collation);
                };
                let mut pairs = equi_join_key_pairs(&condition, &left, &right, collation)
                    .ok_or(ExecError::UnsupportedJoinCondition)?
                    .into_iter();
                let null_safe = pairs.as_slice().iter().map(|key| key.null_safe).collect();
                let primary = pairs.next().ok_or(ExecError::UnsupportedJoinCondition)?;
                let (left_key, right_key) = (primary.left, primary.right);
                let pairs = pairs.map(|key| (key.left, key.right)).collect();
                let probe_estimate = left.estimated_rows();
                let build_estimate = right.estimated_rows();
                let left_input = filtered(Self::plan(*left, collation)?, left_filter);
                let right_input = filtered(Self::plan(*right, collation)?, right_filter);
                // A left input pinned to a few of its keys reads the right
                // input's matches by key rather than building it whole.
                Ok(key_lookup::pinned_lookup(PhysicalPlan::HashJoin {
                    left: Box::new(left_input),
                    right: Box::new(right_input),
                    kind,
                    left_key,
                    extra_keys: pairs,
                    null_safe,
                    right_key,
                    probe_estimate,
                    build_estimate,
                    residual,
                }))
            }
        }
    }
}

/// A join with no hashable equality key: every left/right pair is tested
/// against the ON condition. The work is a filtered cross product whatever
/// the join kind, bounded by the query's memory ceiling and time limit.
fn plan_theta_join(
    left: Box<LogicalPlan>,
    right: Box<LogicalPlan>,
    kind: BoundJoinKind,
    condition: BoundExpr,
    collation: Collation,
) -> Result<PhysicalPlan, ExecError> {
    Ok(PhysicalPlan::NestedLoopJoin {
        left: Box::new(PhysicalPlanner::plan(*left, collation)?),
        right: Box::new(PhysicalPlanner::plan(*right, collation)?),
        kind,
        keys: Vec::new(),
        condition,
    })
}

/// The equalities a dependent join's candidate pairs must satisfy: the ON
/// conjuncts that are plain hashable equalities spanning the two inputs,
/// oriented left then right. The executor buckets the right input by them,
/// so a left row meets only the right rows its keys reach instead of every
/// row; the whole condition, subqueries included, still decides each pair.
/// An equality is one conjunct of an AND, so a pair failing it could never
/// have matched.
fn dependent_join_keys(
    condition: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
    collation: Collation,
) -> Vec<(BoundExpr, BoundExpr)> {
    // An alias visible on both sides through a derived input would make the
    // orientation ambiguous; the unbucketed loop answers that correctly.
    if !logical_tables(left).is_disjoint(&logical_tables(right)) {
        return Vec::new();
    }
    let mut conjuncts = Vec::new();
    and_conjuncts(condition, &mut conjuncts);
    // A null-safe equality is left to the condition: the buckets match NULL
    // with nothing.
    conjuncts
        .iter()
        .filter(|conjunct| !expression_has_subquery(conjunct))
        .filter_map(|conjunct| equi_join_key_pairs(conjunct, left, right, collation))
        .flatten()
        .filter(|key| !key.null_safe)
        .map(|key| (key.left, key.right))
        .collect()
}

fn plan_limit(
    input: LogicalPlan,
    offset: u64,
    count: u64,
    collation: Collation,
) -> Result<PhysicalPlan, ExecError> {
    let input = match input {
        LogicalPlan::Sort { input, keys, trim } => {
            let input = PhysicalPlanner::plan(*input, collation)?;
            // A scan, or a join driven by one, that already yields the
            // sort's order needs no sort, and the limit above it then stops
            // its inputs early.
            let (input, ordered) = order::scan_ordered_input(input, &keys, trim);
            let (input, ordered) = if ordered {
                (input, true)
            } else {
                key_lookup::ordered_input(input, &keys, trim)
            };
            match (input, ordered) {
                (ordered, true) => ordered,
                (input, false) => PhysicalPlan::Sort {
                    input: Box::new(input),
                    keys,
                    top_k: usize::try_from(offset.saturating_add(count)).ok(),
                    trim,
                },
            }
        }
        input => PhysicalPlanner::plan(input, collation)?,
    };
    Ok(PhysicalPlan::Limit {
        input: Box::new(input),
        offset,
        count,
    })
}

fn and_conjuncts(expr: &BoundExpr, out: &mut Vec<BoundExpr>) {
    if let BoundExprKind::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = &expr.kind
    {
        and_conjuncts(left, out);
        and_conjuncts(right, out);
    } else {
        out.push(expr.clone());
    }
}

fn and_all(parts: Vec<BoundExpr>) -> Option<BoundExpr> {
    parts.into_iter().reduce(|left, right| BoundExpr {
        nullable: left.nullable || right.nullable,
        data_type: Some(DataType::Boolean),
        kind: BoundExprKind::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
    })
}

fn filtered(input: PhysicalPlan, predicate: Option<BoundExpr>) -> PhysicalPlan {
    match predicate {
        None => input,
        Some(predicate) => PhysicalPlan::Filter {
            input: Box::new(input),
            predicate,
        },
    }
}

fn logically_filtered(input: Box<LogicalPlan>, predicate: Option<BoundExpr>) -> Box<LogicalPlan> {
    match predicate {
        None => input,
        Some(predicate) => Box::new(LogicalPlan::Filter { input, predicate }),
    }
}

/// Whether an expression reads no row: literals combined by comparison,
/// logic and NULL tests. Functions are left out, so a volatile one is still
/// evaluated per candidate pair.
fn constant_expression(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Binary { left, right, .. } => {
            constant_expression(left) && constant_expression(right)
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            constant_expression(expr)
        }
        _ => false,
    }
}

/// Separates an ON clause into conjuncts that span both inputs and conjuncts
/// confined to one of them, so an ordinary predicate sitting beside the join
/// keys does not make the whole join unplannable.
///
/// A conjunct over only the RIGHT input decides which rows are eligible to
/// match, so evaluating it on that input first is exact for every kind: inner
/// and semi keep the same matches, anti observes the same absence of a match,
/// and left/scalar null-extend exactly the same unmatched rows.
///
/// A conjunct over only the LEFT input is not equivalent under a
/// row-preserving kind — a left row failing it must still be emitted
/// NULL-extended rather than dropped — so it moves only for INNER, where ON
/// and WHERE mean the same thing.
fn split_join_condition(
    condition: BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
    kind: BoundJoinKind,
) -> (Option<BoundExpr>, Option<BoundExpr>, Option<BoundExpr>) {
    let left_tables = logical_tables(left);
    let right_tables = logical_tables(right);
    // Provenance is per relation INSTANCE, so a self-join's sides are
    // disjoint like any other join's and its conjuncts split normally. An
    // overlap can still occur when the same alias is visible on both sides
    // through a derived input; splitting would misroute those, so leave the
    // condition whole for the extractor's two-orientation test.
    if !left_tables.is_disjoint(&right_tables) {
        return (Some(condition), None, None);
    }
    let mut conjuncts = Vec::new();
    and_conjuncts(&condition, &mut conjuncts);
    let (mut spanning, mut left_only, mut right_only) = (Vec::new(), Vec::new(), Vec::new());
    for conjunct in conjuncts {
        // A constant conjunct - `1 = 0`, or an ON clause folded to FALSE - reads
        // neither input. Filtering the right one by it is what it means for
        // every join kind: it decides whether any right row can match.
        if expression_belongs_to(&conjunct, &right_tables) || constant_expression(&conjunct) {
            right_only.push(conjunct);
        } else if matches!(kind, BoundJoinKind::Inner)
            && expression_belongs_to(&conjunct, &left_tables)
        {
            left_only.push(conjunct);
        } else {
            spanning.push(conjunct);
        }
    }
    (and_all(spanning), and_all(left_only), and_all(right_only))
}

/// Splits a join condition into oriented equality key pairs. Every AND-ed
/// conjunct must be a hashable equality spanning the two sides; anything
/// else rejects the whole condition.
/// Separates an ON clause into hash-join equalities and everything else.
///
/// "Everything else" is any conjunct referencing BOTH inputs that is not an
/// equality the hash join can key on - single-side conjuncts have already
/// been pushed into the inputs by `split_join_condition`. Returns `None` for
/// the equalities when none remain, which is the genuine "no join key" case
/// the caller still refuses.
fn split_join_residual(
    condition: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
    collation: Collation,
) -> (Option<BoundExpr>, Option<BoundExpr>) {
    let mut conjuncts = Vec::new();
    and_conjuncts(condition, &mut conjuncts);
    let mut equalities = Vec::new();
    let mut residuals = Vec::new();
    for conjunct in conjuncts {
        if equi_join_key_pairs(&conjunct, left, right, collation).is_some() {
            equalities.push(conjunct);
        } else {
            residuals.push(conjunct);
        }
    }
    (and_all(equalities), and_all(residuals))
}

/// One equality a hash join matches its inputs on.
struct EquiKey {
    /// The left input's side.
    left: BoundExpr,
    /// The right input's side.
    right: BoundExpr,
    /// `<=>`: NULL matches NULL.
    null_safe: bool,
}

fn equi_join_key_pairs(
    condition: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
    collation: Collation,
) -> Option<Vec<EquiKey>> {
    let conjuncts_of = and_conjuncts;
    let left_tables = logical_tables(left);
    let right_tables = logical_tables(right);
    let mut conjuncts = Vec::new();
    conjuncts_of(condition, &mut conjuncts);
    let mut pairs = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let (first, second, null_safe) = if let BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: first,
            right: second,
        } = &conjunct.kind
        {
            (first.as_ref(), second.as_ref(), false)
        } else {
            let (first, second) = null_safe_equality(&conjunct)?;
            (first, second, true)
        };
        hash_join_key_mode(first.data_type, second.data_type, collation)?;
        let (left, right) = if expression_belongs_to(first, &left_tables)
            && expression_belongs_to(second, &right_tables)
        {
            (first, second)
        } else if expression_belongs_to(first, &right_tables)
            && expression_belongs_to(second, &left_tables)
        {
            (second, first)
        } else {
            return None;
        };
        pairs.push(EquiKey {
            left: left.clone(),
            right: right.clone(),
            null_safe,
        });
    }
    if pairs.is_empty() { None } else { Some(pairs) }
}

/// The operands of `l <=> r`, which binds as
/// `COALESCE((l IS NULL AND r IS NULL) OR l = r, FALSE)`, when its equality
/// compares the very operands its NULL tests read. A conversion between
/// them could turn a value into NULL where the tests see none, and such a
/// NULL matches nothing, so that form is left to be tested pair by pair.
fn null_safe_equality(expr: &BoundExpr) -> Option<(&BoundExpr, &BoundExpr)> {
    let BoundExprKind::Scalar {
        function: ScalarFunction::Coalesce,
        args,
    } = &expr.kind
    else {
        return None;
    };
    let [either, fallback] = args.as_slice() else {
        return None;
    };
    if !matches!(fallback.kind, BoundExprKind::Literal(Value::Boolean(false))) {
        return None;
    }
    let BoundExprKind::Binary {
        op: BinaryOp::Or,
        left: both_null,
        right: equal,
    } = &either.kind
    else {
        return None;
    };
    let BoundExprKind::Binary {
        op: BinaryOp::And,
        left: first_null,
        right: second_null,
    } = &both_null.kind
    else {
        return None;
    };
    let (
        BoundExprKind::IsNull {
            expr: first,
            negated: false,
        },
        BoundExprKind::IsNull {
            expr: second,
            negated: false,
        },
    ) = (&first_null.kind, &second_null.kind)
    else {
        return None;
    };
    let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left,
        right,
    } = &equal.kind
    else {
        return None;
    };
    (left == first && right == second).then_some((left.as_ref(), right.as_ref()))
}

/// How one join key's values are normalized to match.
#[derive(Clone, Copy, Debug)]
struct JoinKeyMode {
    form: KeyForm,
    /// NULL matches NULL, as `<=>` compares; under `=` it matches nothing.
    null_safe: bool,
}

#[derive(Clone, Copy, Debug)]
enum KeyForm {
    /// Text keys, carrying the collation the plan resolved. Held here rather
    /// than passed alongside because this mode already travels to every site
    /// that builds or probes a key, and a second parameter could go out of
    /// step with it.
    CollatedText(Collation),
    Binary,
    Boolean,
    Integer,
    MysqlNumber,
}

/// The collation one key compares under: its own, falling back to the plan's
/// where the key reads no text (an integer key, or one whose operands span
/// two collations - which the binder refuses before it reaches here).
/// The collation each grouping key folds under, plus the single shared
/// collation when every text key agrees.
///
/// Grouping never compares one key column AGAINST another - each key column
/// is its own equivalence relation - so keys of two collations are
/// answerable, each folded by its own rules. The uniform collation, when it
/// exists, is what the specialized aggregation paths (dictionary codes,
/// two-pass interning, the fused join) key their single shared tables on;
/// mixed keys route to the general per-key path instead of being refused.
fn grouping_key_collations(
    keys: &[BoundExpr],
    fallback: Collation,
) -> (Vec<Collation>, Option<Collation>) {
    let per_key: Vec<Collation> = keys
        .iter()
        .map(|key| key_collation_of(key, fallback))
        .collect();
    let mut resolved: Option<Collation> = None;
    let mut mixed = false;
    for key in keys {
        let Some(collation) = key.text_collation().and_then(Collation::from_mysql_name) else {
            continue;
        };
        match resolved {
            Some(existing) if existing != collation => mixed = true,
            _ => resolved = Some(collation),
        }
    }
    let uniform = if mixed {
        None
    } else {
        Some(resolved.unwrap_or(fallback))
    };
    (per_key, uniform)
}

fn key_collation_of(key: &BoundExpr, fallback: Collation) -> Collation {
    // A JSON key folds under the JSON ladder, not a text collation: two
    // spellings of one document are one group.
    if key.data_type == Some(DataType::Json) {
        return Collation::Json;
    }
    key.text_collation()
        .and_then(Collation::from_mysql_name)
        .unwrap_or(fallback)
}

fn hash_join_key_mode(
    left: Option<DataType>,
    right: Option<DataType>,
    collation: Collation,
) -> Option<JoinKeyMode> {
    let form = match (left?.storage_type(), right?.storage_type()) {
        (DataType::Utf8, DataType::Utf8) => KeyForm::CollatedText(collation),
        (DataType::Binary, DataType::Binary) => KeyForm::Binary,
        (DataType::Boolean, DataType::Boolean) => KeyForm::Boolean,
        (DataType::Int64 | DataType::UInt64, DataType::Int64 | DataType::UInt64) => {
            KeyForm::Integer
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
        ) => KeyForm::MysqlNumber,
        _ => return None,
    };
    Some(JoinKeyMode {
        form,
        null_safe: false,
    })
}

/// One relation instance: the physical identity plus the alias it is visible
/// through. Two aliases of one table are distinct here, which is what lets a
/// self-join's sides stay disjoint and its keys stay oriented.
type RelationKey = (DatabaseId, TableId, String);

fn logical_tables(plan: &LogicalPlan) -> BTreeSet<RelationKey> {
    let mut tables = BTreeSet::new();
    collect_logical_tables(plan, &mut tables);
    tables
}

fn collect_logical_tables(plan: &LogicalPlan, tables: &mut BTreeSet<RelationKey>) {
    match plan {
        LogicalPlan::Recursive { anchor, member, .. } => {
            collect_logical_tables(anchor, tables);
            collect_logical_tables(member, tables);
        }
        LogicalPlan::Scan(scan) => {
            tables.insert((
                scan.table.database_id,
                scan.table.table_id,
                scan.table.relation_name.to_ascii_lowercase(),
            ));
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
                tables.insert((
                    column.database_id,
                    column.table_id,
                    column.relation_name.to_ascii_lowercase(),
                ));
            }
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Distinct { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => collect_logical_tables(input, tables),
        LogicalPlan::Empty | LogicalPlan::OneRow => {}
    }
}

fn expression_belongs_to(expression: &BoundExpr, tables: &BTreeSet<RelationKey>) -> bool {
    let mut references = BTreeSet::new();
    collect_expression_tables(expression, &mut references);
    !references.is_empty() && references.is_subset(tables)
}

fn collect_expression_tables(expression: &BoundExpr, tables: &mut BTreeSet<RelationKey>) {
    match &expression.kind {
        BoundExprKind::Column(column) => {
            tables.insert((
                column.database_id,
                column.table_id,
                column.relation_name.to_ascii_lowercase(),
            ));
        }
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
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

    /// Per-segment spans a grouped aggregate can fold and keep.
    fn grouped_fold_input(&self) -> Option<GroupedFoldInput> {
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

/// What a grouped aggregate needs to fold one segment at a time.
///
/// A segment file is never rewritten, so a grouped fold taken over its key
/// span stays true while that file is in the manifest. A query can
/// therefore keep the fold and, on its next run, redo only the spans the
/// memtable has since touched - which under replication is the newest one
/// rather than all of them. The settled result memo cannot do this: one
/// ingest invalidates the whole answer and the next query pays for the
/// whole table.
#[derive(Clone)]
pub struct GroupedFoldInput {
    /// A reader-owned view; cloning one is a few `Arc` bumps, and it lets
    /// the fold open its own ranged reads without borrowing the provider.
    pub(crate) snapshot: pintail_store::TableSnapshot,
    pub(crate) directory: std::path::PathBuf,
    /// Key-disjoint segment spans in key order.
    pub(crate) spans: Vec<pintail_store::GroupedFoldSpan>,
    /// Projected column ids, index-aligned with aggregate column indexes.
    pub(crate) column_ids: Vec<u32>,
    /// The table's key columns, so a span read masks the memtable rows it
    /// covers instead of merging row by row.
    pub(crate) key_column_ids: Vec<u32>,
    pub(crate) types: Vec<DataType>,
    /// Parallel to `types`: declared ENUM labels and SET members, so the
    /// fold reattaches the declaration index the way every other scan path
    /// does. Without them a grouped fold would sort an ENUM by its text.
    pub(crate) enum_labels: Vec<Option<std::sync::Arc<Vec<String>>>>,
    pub(crate) set_members: Vec<Option<std::sync::Arc<Vec<String>>>>,
    /// Projected memtable rows outside every segment span.
    pub(crate) outside: Vec<Vec<Value>>,
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
    /// When the scanned table declares key columns, its rows arrive in the
    /// order of those columns: the planner leaves out a sort the scan
    /// already satisfies. A source that cannot keep that order declares no
    /// key columns.
    ///
    /// # Errors
    ///
    /// Returns a source-specific execution error.
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError>;

    /// A provider for one table that the caller may keep for the rest of
    /// the query, so an operator can open ranged scans of that table while
    /// it runs rather than all at build time. `None` when this provider
    /// cannot hand one out; callers then read the table once.
    fn table_provider(
        &self,
        _database_id: DatabaseId,
        _table_id: TableId,
    ) -> Option<Box<dyn ScanProvider + Send + Sync>> {
        None
    }
}

/// Hard per-query memory accounting.
/// One operator's share of a profiled execution.
#[derive(Clone, Debug, Default)]
pub struct OperatorProfile {
    /// The plan node, as `EXPLAIN` names it.
    pub label: String,
    /// Depth in the plan tree, root at zero; operators are in preorder.
    pub depth: usize,
    /// Batches this operator produced.
    pub batches: u64,
    /// Visible rows this operator produced.
    pub rows: u64,
    /// Time spent inside this operator's pulls, its inputs included.
    pub inclusive: Duration,
    /// Time from the first pull to the first batch, its inputs included.
    pub first_batch: Option<Duration>,
    /// The highest query reservation seen right after one of its pulls.
    pub peak_reserved: usize,
    /// Time spent building the operator, its inputs included. Some
    /// operators do their work here: a nested-loop join and a dependent or
    /// recursive query run when they are built, and a subquery resolves
    /// before the tree exists, so a profile that counted pulls alone
    /// showed their scans and nothing above them.
    pub construction: Duration,
    /// What became of the operator beyond its pulls, when anything did:
    /// a join fused into the aggregate above it, for one.
    pub note: Option<String>,
}

/// Where a profiled execution's operators record what they did. Built once
/// per execution when profiling is on; absent otherwise, so an unprofiled
/// query pays nothing.
#[derive(Debug, Default)]
pub struct ProfileSink {
    slots: std::sync::Mutex<Vec<OperatorProfile>>,
    depth: std::sync::atomic::AtomicUsize,
}

impl ProfileSink {
    fn enter(&self, label: String) -> usize {
        let depth = self
            .depth
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.push(OperatorProfile {
            label,
            depth,
            ..OperatorProfile::default()
        });
        slots.len() - 1
    }

    fn leave(&self, slot: usize, construction: Duration) {
        self.depth
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = slots.get_mut(slot) {
            entry.construction += construction;
            entry.inclusive += construction;
        }
    }

    fn annotate(&self, slot: usize, note: &str) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = slots.get_mut(slot) {
            entry.note = Some(note.to_owned());
        }
    }

    fn record(&self, slot: usize, elapsed: Duration, rows: Option<usize>, reserved: usize) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = slots.get_mut(slot) else {
            return;
        };
        entry.inclusive += elapsed;
        if let Some(rows) = rows {
            entry.batches += 1;
            entry.rows += u64::try_from(rows).unwrap_or(u64::MAX);
            if entry.first_batch.is_none() {
                entry.first_batch = Some(entry.inclusive);
            }
        }
        entry.peak_reserved = entry.peak_reserved.max(reserved);
    }

    fn operators(&self) -> Vec<OperatorProfile> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What a profiled execution did, operator by operator.
#[derive(Clone, Debug)]
pub struct ExecutionProfile {
    /// Operators in plan preorder.
    pub operators: Vec<OperatorProfile>,
    /// Wall time from the execution's start to the snapshot.
    pub total: Duration,
    /// The execution's spill counters at the snapshot.
    pub spill: spill::QuerySpillMetrics,
}

impl ExecutionProfile {
    /// Time inside one operator excluding its inputs: its inclusive time
    /// less the inclusive time of the nearest inputs that ran. An input
    /// that never ran on its own, because the operator above took its
    /// inputs directly, contributes what those inputs took instead, so a
    /// fused join does not leave its scans on the aggregate's account.
    #[must_use]
    pub fn exclusive(&self, index: usize) -> Duration {
        let Some(node) = self.operators.get(index) else {
            return Duration::ZERO;
        };
        node.inclusive
            .saturating_sub(self.active_descendants(index))
    }

    fn active_descendants(&self, index: usize) -> Duration {
        let node = &self.operators[index];
        let mut total = Duration::ZERO;
        for (offset, child) in self.operators[index + 1..].iter().enumerate() {
            if child.depth <= node.depth {
                break;
            }
            if child.depth != node.depth + 1 {
                continue;
            }
            // A node that was only built - one a parent fused, reading its
            // inputs directly - ran in its inputs' time, not its own.
            let ran = child.batches > 0 || child.inclusive > child.construction;
            total += if ran {
                child.inclusive
            } else {
                self.active_descendants(index + 1 + offset)
            };
        }
        total
    }

    /// The profile as indented text, one operator per line.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // display only
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let millis = |duration: Duration| duration.as_secs_f64() * 1_000.0;
        let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        let mut output = String::new();
        let _ = writeln!(
            output,
            "Profile total={:.1}ms spill_files={} spill_bytes={} peak_spill_handles={}",
            millis(self.total),
            self.spill.files,
            self.spill.written_bytes,
            self.spill.peak_handles
        );
        for (index, node) in self.operators.iter().enumerate() {
            for _ in 0..node.depth {
                output.push_str("  ");
            }
            let _ = write!(
                output,
                "{} total={:.1}ms self={:.1}ms",
                node.label,
                millis(node.inclusive),
                millis(self.exclusive(index))
            );
            if let Some(first) = node.first_batch {
                let _ = write!(output, " first={:.1}ms", millis(first));
            }
            if !node.construction.is_zero() {
                let _ = write!(output, " build={:.1}ms", millis(node.construction));
            }
            let _ = write!(
                output,
                " batches={} rows={} peak_reserved={:.1}MiB",
                node.batches,
                node.rows,
                mib(node.peak_reserved)
            );
            if let Some(note) = &node.note {
                let _ = write!(output, " ({note})");
            }
            output.push('\n');
        }
        output
    }
}

/// Whether `PINTAIL_PROFILE` asks for every execution to be profiled. A
/// development switch: the server logs each query's profile, which is
/// far too much for production and is not forwarded by the compose file.
fn profiling_requested() -> bool {
    static REQUESTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REQUESTED.get_or_init(|| {
        std::env::var_os("PINTAIL_PROFILE").is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// The label a profile shows for a plan node: what `EXPLAIN` prints, less
/// the detail that would not fit on one line.
fn plan_label(plan: &PhysicalPlan) -> String {
    match plan {
        PhysicalPlan::Empty => "Empty".to_owned(),
        PhysicalPlan::OneRow => "OneRow".to_owned(),
        PhysicalPlan::Scan(scan) => format!(
            "Scan {}.{} predicates={}",
            scan.table.database_name,
            scan.table.table_name,
            scan.predicates.len()
        ),
        PhysicalPlan::Derived { .. } => "Derived".to_owned(),
        PhysicalPlan::CrossJoin { inputs, .. } => format!("CrossJoin inputs={}", inputs.len()),
        PhysicalPlan::UnionAll { inputs } => format!("UnionAll inputs={}", inputs.len()),
        PhysicalPlan::SetOp {
            keep_matching, all, ..
        } => format!(
            "SetOp {}{}",
            if *keep_matching {
                "intersect"
            } else {
                "except"
            },
            if *all { " all" } else { "" }
        ),
        PhysicalPlan::Recursive { .. } => "Recursive".to_owned(),
        PhysicalPlan::KeyLookupJoin { kind, residual, .. } => format!(
            "KeyLookupJoin kind={kind:?} residual={}",
            residual.is_some()
        ),
        PhysicalPlan::HashJoin {
            kind,
            extra_keys,
            residual,
            ..
        } => format!(
            "HashJoin kind={kind:?} keys={} residual={}",
            extra_keys.len() + 1,
            residual.is_some()
        ),
        PhysicalPlan::NestedLoopJoin { kind, .. } => format!("NestedLoopJoin kind={kind:?}"),
        PhysicalPlan::Filter { .. } => "Filter".to_owned(),
        PhysicalPlan::HashAggregate {
            group_by,
            aggregates,
            ..
        } => format!(
            "HashAggregate keys={} aggregates={}",
            group_by.len(),
            aggregates.len()
        ),
        PhysicalPlan::Project { expressions, .. } => {
            format!("Project columns={}", expressions.len())
        }
        PhysicalPlan::Distinct { .. } => "Distinct".to_owned(),
        PhysicalPlan::Sort { keys, top_k, .. } => match top_k {
            Some(k) => format!("Sort keys={} top_k={k}", keys.len()),
            None => format!("Sort keys={}", keys.len()),
        },
        PhysicalPlan::Window { windows, .. } => format!("Window functions={}", windows.len()),
        PhysicalPlan::Limit { offset, count, .. } => format!("Limit offset={offset} count={count}"),
    }
}

#[derive(Debug)]
pub struct MemoryTracker {
    limit: usize,
    deadline: Option<Instant>,
    cancellation: Option<ExecutionCancellation>,
    /// Atomic so parallel operators can reserve from worker threads through
    /// a shared `&MemoryTracker` (experiments/RESULTS.md e02: thread-local
    /// partial state + merge is the adopted parallel-aggregation shape).
    used: std::sync::atomic::AtomicUsize,
    /// Whether this tracker charges the process-wide budget. Worker
    /// trackers are accounting-independent clones of a parent that already
    /// charged it, so they must not charge it twice.
    charges_shared: bool,
    /// Bytes this tracker has taken from the process-wide budget and not yet
    /// returned.
    ///
    /// Separate from `used`, which is the query's own ceiling, because the two
    /// answer different questions: `used` is how much the query is holding,
    /// this is how much THIS tracker owes the shared pool. A clone inherits
    /// the first and not the second - it has borrowed nothing itself - which
    /// is what stops two trackers from repaying one debt twice.
    shared_charged: std::sync::atomic::AtomicUsize,
    spill: spill::QuerySpill,
    /// Present while the execution is profiled; operators built under this
    /// tracker wrap themselves and record here.
    profile: Option<std::sync::Arc<ProfileSink>>,
}

impl Clone for MemoryTracker {
    fn clone(&self) -> Self {
        Self {
            limit: self.limit,
            deadline: self.deadline,
            cancellation: self.cancellation.clone(),
            used: std::sync::atomic::AtomicUsize::new(self.used()),
            charges_shared: self.charges_shared,
            // Zero: the clone has taken nothing from the shared budget, so it
            // owes nothing and must not repay the original's debt.
            shared_charged: std::sync::atomic::AtomicUsize::new(0),
            spill: self.spill.clone(),
            profile: self.profile.clone(),
        }
    }
}

/// Returns whatever the tracker still owes the process-wide budget.
///
/// Releases were explicit, so every path that ended early - an error, a `?`,
/// an operator dropped before it finished - kept its reservation forever. The
/// budget is process-wide and nothing refills it, so a long-running server
/// walked one way: a benchmark phase exhausted 12GiB after about 1,500
/// queries and then refused every query, while replication stayed healthy and
/// nothing was logged. Correctness here cannot rest on remembering to call
/// release on every path, so it rests on the type system instead.
impl Drop for MemoryTracker {
    fn drop(&mut self) {
        if !self.charges_shared {
            return;
        }
        let owed = self
            .shared_charged
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if owed > 0 {
            shared_memory_budget().release(owed);
            if let Some(cancellation) = &self.cancellation {
                cancellation.release(owed);
            }
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
            cancellation: Some(
                EXECUTION_CANCELLATION
                    .with(|current| current.borrow().clone())
                    .unwrap_or_default(),
            ),
            used: std::sync::atomic::AtomicUsize::new(0),
            charges_shared: true,
            shared_charged: std::sync::atomic::AtomicUsize::new(0),
            spill: spill::QuerySpill::new(),
            profile: None,
        }
    }

    /// Builds an accounting-independent worker tracker that retains the
    /// parent query's interruption state.
    fn unbounded_worker(&self) -> Self {
        Self {
            limit: usize::MAX,
            deadline: self.deadline,
            cancellation: self.cancellation.clone(),
            used: std::sync::atomic::AtomicUsize::new(0),
            charges_shared: false,
            shared_charged: std::sync::atomic::AtomicUsize::new(0),
            spill: self.spill.clone(),
            profile: self.profile.clone(),
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
        self.check_interruption()?;
        let outcome = self.used.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |used| {
                let requested = used.saturating_add(bytes);
                (requested <= self.limit).then_some(requested)
            },
        );
        match outcome {
            Ok(_) => {
                if self.charges_shared {
                    if let Err(error) = shared_memory_budget().reserve(bytes) {
                        // The query ceiling already accepted these bytes. Give
                        // them back before reporting, or a refused reservation
                        // permanently shrinks this query's own allowance.
                        self.release_local(bytes);
                        return Err(error);
                    }
                    if let Some(cancellation) = &self.cancellation {
                        cancellation.reserve(bytes);
                    }
                    self.shared_charged
                        .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            }
            Err(used) => Err(ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit,
                scope: MemoryScope::Query,
            }),
        }
    }

    fn release_local(&self, bytes: usize) {
        let _ = self.used.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |used| Some(used.saturating_sub(bytes)),
        );
    }

    /// Releases persistent operator memory from both ceilings.
    pub fn release(&self, bytes: usize) {
        self.release_local(bytes);
        if self.charges_shared {
            self.repay_shared(bytes);
        }
    }

    /// Returns up to `bytes` of this tracker's shared debt.
    ///
    /// Clamped to what it actually owes: callers release what an operator
    /// held, which is not always what this tracker borrowed, and repaying
    /// more than was taken would hand the pool memory that was never in it.
    fn repay_shared(&self, bytes: usize) {
        let repaid = self
            .shared_charged
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |owed| Some(owed.saturating_sub(bytes)),
            )
            .map_or(0, |owed| owed.min(bytes));
        if repaid > 0 {
            shared_memory_budget().release(repaid);
            if let Some(cancellation) = &self.cancellation {
                cancellation.release(repaid);
            }
        }
    }

    fn ensure_transient(&self, bytes: usize) -> Result<(), ExecError> {
        self.check_interruption()?;
        let used = self.used();
        if used.saturating_add(bytes) > self.limit {
            return Err(ExecError::MemoryLimitExceeded {
                used,
                requested: bytes,
                limit: self.limit,
                scope: MemoryScope::Query,
            });
        }
        let shared = shared_memory_budget();
        if self.charges_shared && !shared.would_fit(bytes) {
            return Err(ExecError::MemoryLimitExceeded {
                used: shared.used(),
                requested: bytes,
                limit: shared.limit(),
                scope: MemoryScope::Server,
            });
        }
        Ok(())
    }

    fn check_interruption(&self) -> Result<(), ExecError> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(ExecutionCancellation::is_cancelled)
        {
            Err(ExecError::QueryCancelled)
        } else if self
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
    /// When the execution was built, for the profile's total.
    started: Instant,
    /// Held for the execution's life so a test measuring the process-wide
    /// budget can be sure no sibling test is charging it. Absent outside
    /// tests: nothing in production needs queries serialized.
    #[cfg(test)]
    _budget_serial: budget_serial::Serial,
}

/// Serializes tests that CHARGE the process-wide memory budget against the
/// tests that MEASURE it.
///
/// The budget is a process-wide singleton, so a test asserting that a query
/// returned exactly what it borrowed is reading a counter every other test in
/// the binary is moving. Run alone those tests pass; run in the suite - which
/// is how the gate runs them - they fail on another test's reservation and
/// say nothing about leaks. Every query goes through `Execution::start`, so
/// one door is enough to close.
#[cfg(test)]
pub(crate) mod budget_serial {
    use std::{
        cell::Cell,
        sync::{Mutex, MutexGuard, PoisonError},
    };

    static LOCK: Mutex<()> = Mutex::new(());

    thread_local! {
        /// Whether this thread already holds the lock. A measuring test runs
        /// queries of its own, and those come back through the same door -
        /// which a plain mutex would deadlock on.
        static HELD: Cell<bool> = const { Cell::new(false) };
    }

    /// Exclusive right to charge the shared budget, for as long as it lives.
    pub(crate) struct Serial {
        guard: Option<MutexGuard<'static, ()>>,
    }

    impl Serial {
        /// Takes the lock, or takes nothing if this thread already holds it.
        pub(crate) fn acquire() -> Self {
            if HELD.with(Cell::get) {
                return Self { guard: None };
            }
            // A panicking test poisons the lock; the data is `()`, so there
            // is nothing to be corrupted and the next test may proceed.
            let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            HELD.with(|held| held.set(true));
            Self { guard: Some(guard) }
        }
    }

    impl Drop for Serial {
        fn drop(&mut self) {
            if self.guard.is_some() {
                HELD.with(|held| held.set(false));
            }
        }
    }
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
        | PhysicalPlan::Distinct { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => nested(input),
        PhysicalPlan::CrossJoin { inputs, .. } | PhysicalPlan::UnionAll { inputs } => inputs
            .iter()
            .fold(0_usize, |bytes, input| bytes.saturating_add(nested(input))),
        PhysicalPlan::SetOp { left, right, .. } => nested(left).saturating_add(nested(right)),
        PhysicalPlan::Recursive { anchor, member, .. } => {
            nested(anchor).saturating_add(nested(member))
        }
        PhysicalPlan::KeyLookupJoin {
            left,
            right,
            driving_key,
            lookup_key,
            residual,
            ..
        } => nested(left)
            .saturating_add(nested(right))
            .saturating_add(bound_regex_memory_upper_bound(driving_key))
            .saturating_add(bound_regex_memory_upper_bound(lookup_key))
            .saturating_add(residual.as_ref().map_or(0, bound_regex_memory_upper_bound)),
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
        collation: Collation,
    ) -> Result<Self, ExecError> {
        Self::start_with_deadline(plan, provider, memory_limit, None, collation)
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
        plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
        deadline: Option<Instant>,
        collation: Collation,
    ) -> Result<Self, ExecError> {
        Self::start_inner(
            plan,
            provider,
            memory_limit,
            deadline,
            collation,
            profiling_requested(),
        )
    }

    /// Starts an execution that records a per-operator profile, whatever
    /// the environment says; `EXPLAIN ANALYZE` uses it.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::start_with_deadline`].
    pub fn start_profiled(
        plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
        deadline: Option<Instant>,
        collation: Collation,
    ) -> Result<Self, ExecError> {
        Self::start_inner(plan, provider, memory_limit, deadline, collation, true)
    }

    fn start_inner(
        mut plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
        deadline: Option<Instant>,
        collation: Collation,
        profiled: bool,
    ) -> Result<Self, ExecError> {
        let started = Instant::now();
        // Taken before anything reserves: subquery resolution charges the
        // budget too, and a lock taken after it would leave that charge
        // outside the window a measuring test believes it owns.
        #[cfg(test)]
        let serial = budget_serial::Serial::acquire();
        let mut subquery_bytes = 0;
        let query_spill = spill::QuerySpill::new();
        resolve_plan_subqueries(
            &mut plan,
            provider,
            memory_limit,
            deadline,
            &mut subquery_bytes,
            collation,
            &query_spill,
        )?;
        let output_fields = plan.output_fields();
        let mut memory = MemoryTracker::with_deadline(memory_limit, deadline);
        memory.spill = query_spill;
        if profiled {
            memory.profile = Some(std::sync::Arc::new(ProfileSink::default()));
        }
        memory.check_interruption()?;
        memory.reserve(subquery_bytes.saturating_add(plan_regex_memory_upper_bound(&plan)))?;
        let (root, _) = build_operator(plan, provider, &memory, collation)?;
        Ok(Self {
            root,
            memory,
            output_fields,
            started,
            #[cfg(test)]
            _budget_serial: serial,
        })
    }

    /// The per-operator profile, when this execution records one.
    #[must_use]
    pub fn profile(&self) -> Option<ExecutionProfile> {
        let sink = self.memory.profile.as_ref()?;
        Some(ExecutionProfile {
            operators: sink.operators(),
            total: self.started.elapsed(),
            spill: self.memory.spill_metrics(),
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
    collation: Collation,
    query_spill: &spill::QuerySpill,
) -> Result<(), ExecError> {
    match plan {
        PhysicalPlan::Recursive { anchor, member, .. } => {
            resolve_plan_subqueries(
                anchor,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_plan_subqueries(
                member,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        PhysicalPlan::Scan(scan) => {
            for predicate in &mut scan.predicates {
                resolve_expr_subqueries(
                    predicate,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
            }
        }
        PhysicalPlan::Derived { input, .. }
        | PhysicalPlan::Distinct { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => {
            resolve_plan_subqueries(
                input,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        PhysicalPlan::SetOp { left, right, .. } => {
            resolve_plan_subqueries(
                left,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_plan_subqueries(
                right,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
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
                        collation,
                        query_spill,
                    )?;
                }
                for expr in &mut window.partition_by {
                    resolve_expr_subqueries(
                        expr,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                        collation,
                        query_spill,
                    )?;
                }
                for key in &mut window.order_by {
                    resolve_expr_subqueries(
                        &mut key.expr,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                        collation,
                        query_spill,
                    )?;
                }
            }
            resolve_plan_subqueries(
                input,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        PhysicalPlan::CrossJoin { inputs, .. } | PhysicalPlan::UnionAll { inputs } => {
            for input in inputs {
                resolve_plan_subqueries(
                    input,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
            }
        }
        PhysicalPlan::KeyLookupJoin {
            left,
            right,
            driving_key,
            lookup_key,
            residual,
            ..
        } => {
            for input in [left, right] {
                resolve_plan_subqueries(
                    input,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
            }
            for expression in [driving_key, lookup_key]
                .into_iter()
                .chain(residual.as_mut())
            {
                resolve_expr_subqueries(
                    expression,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
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
            resolve_plan_subqueries(
                left,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_plan_subqueries(
                right,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_expr_subqueries(
                left_key,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_expr_subqueries(
                right_key,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            for (extra_left, extra_right) in extra_keys {
                resolve_expr_subqueries(
                    extra_left,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
                resolve_expr_subqueries(
                    extra_right,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
            }
        }
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            condition,
            ..
        } => {
            resolve_plan_subqueries(
                left,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_plan_subqueries(
                right,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_expr_subqueries(
                condition,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        PhysicalPlan::Filter { input, predicate } => {
            resolve_plan_subqueries(
                input,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_expr_subqueries(
                predicate,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => {
            resolve_plan_subqueries(
                input,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            for expression in group_by {
                resolve_expr_subqueries(
                    expression,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
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
                        collation,
                        query_spill,
                    )?;
                }
                for (key, _) in &mut aggregate.order_within {
                    resolve_expr_subqueries(
                        key,
                        provider,
                        memory_limit,
                        deadline,
                        retained_bytes,
                        collation,
                        query_spill,
                    )?;
                }
            }
        }
        PhysicalPlan::Project { input, expressions } => {
            resolve_plan_subqueries(
                input,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            for projection in expressions {
                resolve_expr_subqueries(
                    &mut projection.expr,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
                )?;
            }
        }
        PhysicalPlan::Empty | PhysicalPlan::OneRow => {}
    }
    Ok(())
}

// One arm per expression kind that can hold a subquery; splitting it would
// scatter the walk without making any arm clearer.
#[allow(clippy::too_many_lines)]
fn resolve_expr_subqueries(
    expression: &mut BoundExpr,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<Instant>,
    retained_bytes: &mut usize,
    collation: Collation,
    query_spill: &spill::QuerySpill,
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
                collation,
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
                collation,
            )?;
            let exists = !values.is_empty();
            expression.kind = BoundExprKind::Literal(Value::Boolean(exists != *negated));
        }
        BoundExprKind::InSubquery {
            expr,
            query,
            negated,
        } => {
            resolve_expr_subqueries(
                expr,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            if bound_query_has_outer_refs(query) {
                return Ok(());
            }
            let projection_type = query
                .projection
                .first()
                .and_then(|projection| projection.expr.data_type);
            let member_collation = expr
                .text_collation()
                .and_then(Collation::from_mysql_name)
                .unwrap_or(collation);
            // Answered once for the whole query, so a large set is worth
            // indexing: each probe then costs a hash lookup rather than a
            // comparison per member.
            let materialized = match membership::materialize_membership(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
                deadline,
                member_collation,
                query_spill,
                expr.data_type,
                projection_type,
            )? {
                membership::MaterializedMembership::Memory(values) => membership::index_members(
                    values,
                    member_collation,
                    expr.data_type,
                    projection_type,
                ),
                prepared @ membership::MaterializedMembership::Prepared(..) => prepared,
            };
            let values = match materialized {
                membership::MaterializedMembership::Memory(values) => values,
                membership::MaterializedMembership::Prepared(membership, bytes) => {
                    if retained_bytes.saturating_add(bytes) > memory_limit {
                        return Err(ExecError::MemoryLimitExceeded {
                            used: *retained_bytes,
                            requested: bytes,
                            limit: memory_limit,
                            scope: MemoryScope::Query,
                        });
                    }
                    *retained_bytes += bytes;
                    expression.kind = BoundExprKind::PreparedIn {
                        expr: expr.clone(),
                        membership,
                        negated: *negated,
                    };
                    return Ok(());
                }
            };
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
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
            resolve_expr_subqueries(
                expr,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            resolve_expr_subqueries(
                left,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
            resolve_expr_subqueries(
                right,
                provider,
                memory_limit,
                deadline,
                retained_bytes,
                collation,
                query_spill,
            )?;
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                resolve_expr_subqueries(
                    argument,
                    provider,
                    memory_limit,
                    deadline,
                    retained_bytes,
                    collation,
                    query_spill,
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
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => bound_expr_has_outer_refs(expr),
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
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => expression_has_dependent_subquery(expr),
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
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => expression_has_subquery(expr),
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

/// Writes one outer row's values into `query` as literals, and records
/// every value written, in visit order, into `key`. The walk is
/// deterministic, so two rows that write the same values produce the same
/// key - which is exactly the tuple the dependent memo shares results by.
fn substitute_outer_query(
    query: &mut BoundQuery,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
    key: &mut Vec<Value>,
) -> Result<(), ExecError> {
    for projection in &mut query.projection {
        substitute_outer_expr(&mut projection.expr, batch, row, columns, key)?;
    }
    if let Some(filter) = &mut query.filter {
        substitute_outer_expr(filter, batch, row, columns, key)?;
    }
    for expression in &mut query.group_by {
        substitute_outer_expr(expression, batch, row, columns, key)?;
    }
    for aggregate in &mut query.aggregates {
        if let Some(expression) = &mut aggregate.expr {
            substitute_outer_expr(expression, batch, row, columns, key)?;
        }
        for (expression, _) in &mut aggregate.order_within {
            substitute_outer_expr(expression, batch, row, columns, key)?;
        }
    }
    for window in &mut query.windows {
        match &mut window.function {
            WindowFunction::Aggregate(aggregate) => {
                if let Some(expression) = &mut aggregate.expr {
                    substitute_outer_expr(expression, batch, row, columns, key)?;
                }
            }
            WindowFunction::Offset { expr, default, .. } => {
                substitute_outer_expr(expr, batch, row, columns, key)?;
                if let Some(default) = default {
                    substitute_outer_expr(default, batch, row, columns, key)?;
                }
            }
            WindowFunction::Extreme { expr, .. } => {
                substitute_outer_expr(expr, batch, row, columns, key)?;
            }
            WindowFunction::RowNumber
            | WindowFunction::Rank
            | WindowFunction::DenseRank
            | WindowFunction::NTile(_) => {}
        }
        for expression in &mut window.partition_by {
            substitute_outer_expr(expression, batch, row, columns, key)?;
        }
        for order_key in &mut window.order_by {
            substitute_outer_expr(&mut order_key.expr, batch, row, columns, key)?;
        }
    }
    if let Some(having) = &mut query.having {
        substitute_outer_expr(having, batch, row, columns, key)?;
    }
    for source in &mut query.from {
        if let Some(input) = &mut source.base.input {
            substitute_outer_query(input, batch, row, columns, key)?;
        }
        for join in &mut source.joins {
            if let Some(input) = &mut join.table.input {
                substitute_outer_query(input, batch, row, columns, key)?;
            }
            if let Some(condition) = &mut join.condition {
                substitute_outer_expr(condition, batch, row, columns, key)?;
            }
        }
    }
    for branch in &mut query.union_all {
        substitute_outer_query(branch, batch, row, columns, key)?;
    }
    for (_, right) in &mut query.set_ops {
        substitute_outer_query(right, batch, row, columns, key)?;
    }
    if let Some(recursive) = &mut query.recursive {
        substitute_outer_query(&mut recursive.member, batch, row, columns, key)?;
    }
    Ok(())
}

fn substitute_outer_expr(
    expression: &mut BoundExpr,
    batch: &RecordBatch,
    row: usize,
    columns: &[BoundColumn],
    key: &mut Vec<Value>,
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
                key.push(value.clone());
                expression.kind = BoundExprKind::Literal(value);
            }
        }
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
            substitute_outer_expr(expr, batch, row, columns, key)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            substitute_outer_expr(left, batch, row, columns, key)?;
            substitute_outer_expr(right, batch, row, columns, key)?;
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                substitute_outer_expr(argument, batch, row, columns, key)?;
            }
        }
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            substitute_outer_query(query, batch, row, columns, key)?;
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            substitute_outer_expr(expr, batch, row, columns, key)?;
            substitute_outer_query(query, batch, row, columns, key)?;
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
    Ok(())
}

/// The row context a dependent subquery resolves in. One struct rather
/// than seven arguments now that the memo rides along with them.
pub(super) struct DependentRow<'a> {
    pub(super) batch: &'a RecordBatch,
    pub(super) row: usize,
    pub(super) columns: &'a [BoundColumn],
    pub(super) provider: &'a dyn ScanProvider,
    pub(super) memory: &'a MemoryTracker,
    pub(super) collation: Collation,
}

/// Answers one subquery for the current row: from the memo when the
/// substituted outer tuple has been seen, otherwise by executing it and
/// recording the answer. `maximum_rows` is the same early stop the
/// un-memoized path used, so a cached answer is exactly what a fresh one
/// would have been.
fn dependent_subquery_values(
    query: &BoundQuery,
    context: &DependentRow<'_>,
    memo: &mut DependentMemo,
    maximum_rows: Option<usize>,
) -> Result<Vec<Value>, ExecError> {
    let slot = memo.next_slot();
    let mut query = query.clone();
    let mut key = Vec::new();
    substitute_outer_query(
        &mut query,
        context.batch,
        context.row,
        context.columns,
        &mut key,
    )?;
    if let Some(values) = memo.get(slot, &key) {
        return Ok(values);
    }
    let values = materialize_subquery(
        query,
        context.provider,
        dependent_subquery_memory_limit(context.memory, context.batch)?,
        context.memory.deadline,
        maximum_rows,
        context.collation,
    )?;
    memo.insert(context.memory, slot, key, &values);
    Ok(values)
}

// One arm per scalar expression shape, including the membership storage choice.
#[allow(clippy::too_many_lines)]
pub(super) fn resolve_dependent_expr_subqueries(
    expression: &mut BoundExpr,
    context: &DependentRow<'_>,
    memo: &mut DependentMemo,
) -> Result<(), ExecError> {
    context.memory.check_interruption()?;
    match &mut expression.kind {
        BoundExprKind::ScalarSubquery(query) => {
            let values = dependent_subquery_values(query, context, memo, Some(2))?;
            let value = match values.as_slice() {
                [] => Value::Null,
                [value] => value.clone(),
                _ => return Err(ExecError::ScalarSubqueryRows { rows: values.len() }),
            };
            expression.nullable = matches!(value, Value::Null);
            expression.kind = BoundExprKind::Literal(value);
        }
        BoundExprKind::ExistsSubquery { query, negated } => {
            let values = dependent_subquery_values(query, context, memo, Some(1))?;
            expression.kind = BoundExprKind::Literal(Value::Boolean(values.is_empty() == *negated));
            expression.nullable = false;
        }
        BoundExprKind::InSubquery {
            expr,
            query,
            negated,
        } => {
            resolve_dependent_expr_subqueries(expr, context, memo)?;
            let projection_type = query
                .projection
                .first()
                .and_then(|projection| projection.expr.data_type);
            let slot = memo.next_slot();
            let mut resolved = (**query).clone();
            let mut key = Vec::new();
            substitute_outer_query(
                &mut resolved,
                context.batch,
                context.row,
                context.columns,
                &mut key,
            )?;
            let values = if let Some(values) = memo.get(slot, &key) {
                values
            } else {
                match membership::materialize_membership(
                    resolved,
                    context.provider,
                    dependent_subquery_memory_limit(context.memory, context.batch)?,
                    context.memory.deadline,
                    expr.text_collation()
                        .and_then(Collation::from_mysql_name)
                        .unwrap_or(context.collation),
                    context.memory.spill(),
                    expr.data_type,
                    projection_type,
                )? {
                    membership::MaterializedMembership::Memory(values) => {
                        memo.insert(context.memory, slot, key, &values);
                        values
                    }
                    membership::MaterializedMembership::Prepared(membership, bytes) => {
                        context.memory.reserve(bytes)?;
                        let needle =
                            CompiledExpr::compile(expr, context.columns, context.collation)?
                                .evaluate(context.batch, context.row)?;
                        let value = membership
                            .0
                            .lookup(&needle)
                            .map_err(membership::execution_error)?;
                        drop(membership);
                        context.memory.release(bytes);
                        expression.kind = BoundExprKind::Literal(match value {
                            Value::Boolean(value) => Value::Boolean(value != *negated),
                            other => other,
                        });
                        return Ok(());
                    }
                }
            };
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
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => {
            resolve_dependent_expr_subqueries(expr, context, memo)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            resolve_dependent_expr_subqueries(left, context, memo)?;
            resolve_dependent_expr_subqueries(right, context, memo)?;
        }
        BoundExprKind::Scalar { function, args } => {
            resolve_dependent_scalar_args(*function, args, context, memo)?;
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
    Ok(())
}

/// Short-circuiting functions resolve only the branch they take. The memo
/// slot walk must still account for the skipped branch's subqueries, or
/// the slots after it would name the wrong subquery on this row - so a
/// skipped branch advances the cursor past its subqueries without
/// executing or caching any of them.
fn resolve_dependent_scalar_args(
    function: ScalarFunction,
    args: &mut [BoundExpr],
    context: &DependentRow<'_>,
    memo: &mut DependentMemo,
) -> Result<(), ExecError> {
    match function {
        ScalarFunction::If if args.len() == 3 => {
            resolve_dependent_expr_subqueries(&mut args[0], context, memo)?;
            let condition = evaluate_and_literalize(&mut args[0], context)?;
            let selected = if predicate_truth(&condition)? { 1 } else { 2 };
            let skipped = if selected == 1 { 2 } else { 1 };
            if selected == 1 {
                resolve_dependent_expr_subqueries(&mut args[1], context, memo)?;
                memo.skip_subqueries_in(&args[2]);
            } else {
                memo.skip_subqueries_in(&args[1]);
                resolve_dependent_expr_subqueries(&mut args[2], context, memo)?;
            }
            args[skipped].nullable = true;
            args[skipped].kind = BoundExprKind::Literal(Value::Null);
        }
        ScalarFunction::Coalesce => {
            let mut settled = None;
            for (index, argument) in args.iter_mut().enumerate() {
                resolve_dependent_expr_subqueries(argument, context, memo)?;
                let value = evaluate_and_literalize(argument, context)?;
                if !matches!(value, Value::Null) {
                    settled = Some(index);
                    break;
                }
            }
            if let Some(index) = settled {
                for skipped in &mut args[index + 1..] {
                    memo.skip_subqueries_in(skipped);
                    skipped.nullable = true;
                    skipped.kind = BoundExprKind::Literal(Value::Null);
                }
            }
        }
        _ => {
            for argument in args {
                resolve_dependent_expr_subqueries(argument, context, memo)?;
            }
        }
    }
    Ok(())
}

fn evaluate_and_literalize(
    expression: &mut BoundExpr,
    context: &DependentRow<'_>,
) -> Result<Value, ExecError> {
    let value = CompiledExpr::compile(expression, context.columns, context.collation)?
        .evaluate(context.batch, context.row)?;
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

/// Inner executions run by the dependent (per-outer-row) subquery path since
/// process start. A measurement hook: the ratio benchmark reads it to prove
/// which shapes take this path and how many times, which a wall-clock
/// reading alone cannot separate from planning cost.
static DEPENDENT_SUBQUERY_EXECUTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many times the dependent subquery path has executed an inner query.
#[must_use]
pub fn dependent_subquery_executions() -> u64 {
    DEPENDENT_SUBQUERY_EXECUTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

static DEPENDENT_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DEPENDENT_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DEPENDENT_MEMO_DISABLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Inner executions the dependent memo answered from a previous row
/// instead of running, since process start.
#[must_use]
pub fn dependent_memo_hits() -> u64 {
    DEPENDENT_MEMO_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Operators whose memo gave up - a refused memory charge or the entry
/// cap - and finished un-memoized, since process start. A rising count
/// means the ceiling, not the memo, is what is bounding those queries.
#[must_use]
pub fn dependent_memo_disabled() -> u64 {
    DEPENDENT_MEMO_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Memo lookups that found nothing and ran the inner query, since process
/// start. Hits over hits plus misses is the share of inner executions the
/// memo removed.
#[must_use]
pub fn dependent_memo_misses() -> u64 {
    DEPENDENT_MEMO_MISSES.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn record_dependent_memo(stats: memo::DependentMemoStats) {
    DEPENDENT_MEMO_HITS.fetch_add(stats.hits, std::sync::atomic::Ordering::Relaxed);
    DEPENDENT_MEMO_MISSES.fetch_add(stats.misses, std::sync::atomic::Ordering::Relaxed);
    if stats.disabled {
        DEPENDENT_MEMO_DISABLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn materialize_subquery(
    query: pintail_sql::BoundQuery,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    deadline: Option<Instant>,
    maximum_rows: Option<usize>,
    collation: Collation,
) -> Result<Vec<Value>, ExecError> {
    DEPENDENT_SUBQUERY_EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let logical = Optimizer::optimize(LogicalPlanner::plan(query));
    let physical = PhysicalPlanner::plan(logical, collation)?;
    if physical.output_fields().len() != 1 {
        return Err(ExecError::InvalidPhysicalPlan(
            "scalar or IN subquery must produce exactly one column",
        ));
    }
    let mut execution =
        Execution::start_with_deadline(physical, provider, memory_limit, deadline, collation)?;
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
                    scope: MemoryScope::Query,
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
            scope: MemoryScope::Query,
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
    KeyLookupJoin(Box<key_lookup::KeyLookupJoin>),
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
        /// Compiled residual ON predicate and the combined columns it reads.
        residual: Option<CompiledExpr>,
        residual_columns: Vec<BoundColumn>,
        /// The plan's collation. The key mode carries it for hashing; this is
        /// for the row-level work either side of the probe.
        collation: Collation,
        /// Whether to read the probe side ahead of the build, so that when
        /// it is small its keys can filter the build side.
        probe_prefetch: bool,
    },
    Filter {
        input: Box<Self>,
        predicate: CompiledExpr,
    },
    /// Records what the operator beneath it does: wrapped around every
    /// plan node of a profiled execution, absent otherwise.
    Profiled {
        input: Box<Self>,
        slot: usize,
        sink: std::sync::Arc<ProfileSink>,
    },
    /// Drops build-side rows whose join key no probe row carries, before
    /// they reach the hash table. Wrapped around the build input when the
    /// probe side was read in full first.
    KeyFilter {
        input: Box<Self>,
        key: CompiledExpr,
        key_mode: JoinKeyMode,
        keys: std::sync::Arc<std::collections::HashSet<JoinHashKey>>,
        /// The same keys as a packed integer membership test, when every
        /// key is an integer; a typed integer key column is filtered
        /// through this without a `Value` per row.
        integers: Option<join::IntegerKeySet>,
    },
    HashAggregate {
        input: Box<Self>,
        group_by: Vec<CompiledExpr>,
        aggregates: Vec<CompiledAggregate>,
        column_types: Vec<DataType>,
        state: Option<MaterializedRows>,
        /// The plan's collation, fixed when the operator was built. Grouping
        /// is an equivalence relation over text, so it has to be decided once
        /// for the operator rather than per batch.
        collation: Collation,
        /// Each grouping key's own collation, in key order. Uniform keys
        /// repeat one collation; mixed keys are what the general path folds
        /// per key.
        key_collations: Vec<Collation>,
    },
    Project {
        input: Box<Self>,
        expressions: Vec<(CompiledExpr, Option<DataType>)>,
    },
    Distinct {
        input: Box<Self>,
        column_types: Vec<DataType>,
        state: Option<DistinctRows>,
        /// The plan's collation: DISTINCT decides row identity for keys
        /// without one of their own.
        collation: Collation,
        /// Per projected column, from the expression's coercibility ladder.
        key_collations: Vec<Option<String>>,
    },
    SetOp {
        left: Option<Box<Self>>,
        right: Option<Box<Self>>,
        keep_matching: bool,
        all: bool,
        column_types: Vec<DataType>,
        state: Option<SetOpRows>,
        /// The plan's collation: set operations compare whole rows.
        collation: Collation,
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
        /// The plan's collation: ORDER BY on text is decided by it.
        collation: Collation,
    },
    Window {
        input: Box<Self>,
        windows: Vec<CompiledWindow>,
        /// The plan's collation: window ORDER BY and PARTITION BY use it.
        collation: Collation,
        column_types: Vec<DataType>,
        state: Option<SortedRows>,
    },
    Limit {
        input: Box<Self>,
        skip: u64,
        take: u64,
    },
}

impl PullOperator {
    /// Find an unstarted scan below an inner-join input, translating the
    /// output position at each join. Outer joins and layout-changing nodes
    /// are boundaries: pushing through them could remove required rows.
    fn nested_probe_scan(
        &mut self,
        position: usize,
        crossed_join: bool,
    ) -> Option<(&mut Self, usize)> {
        match self {
            Self::Profiled { input, .. } | Self::Filter { input, .. } => {
                input.nested_probe_scan(position, crossed_join)
            }
            Self::HashJoin {
                left,
                right,
                kind: BoundJoinKind::Inner,
                column_types,
                right_width,
                state: None,
                ..
            } => {
                let left_width = column_types.len().checked_sub(*right_width)?;
                if position < left_width {
                    left.nested_probe_scan(position, true)
                } else if position < column_types.len() {
                    right.nested_probe_scan(position - left_width, true)
                } else {
                    None
                }
            }
            Self::Scan { .. } if crossed_join => Some((self, position)),
            _ => None,
        }
    }

    /// Forwards a probe-side key restriction to the underlying scan, passing
    /// through filters only — any other operator changes row identity or
    /// layout and stops the pushdown.
    fn restrict_probe_range(&mut self, position: usize, min: &Value, max: &Value) {
        match self {
            Self::Scan { stream, .. } => stream.restrict_key_position_range(position, min, max),
            Self::Filter { input, .. }
            | Self::KeyFilter { input, .. }
            | Self::Profiled { input, .. } => {
                input.restrict_probe_range(position, min, max);
            }
            _ => {}
        }
    }

    /// Keeps only build rows whose key is in `keys`, the set the probe side
    /// carries. A row whose key no probe row has can match nothing, so for
    /// inner, left, semi and anti joins it contributes nothing to the
    /// output and need not be built. The key span goes to the scan first,
    /// so storage can prune on it where the key is the table's own key.
    fn restrict_build_keys(
        &mut self,
        key: CompiledExpr,
        key_mode: JoinKeyMode,
        keys: std::sync::Arc<std::collections::HashSet<JoinHashKey>>,
    ) {
        if let Some(position) = key.column_index()
            && let Some((minimum, maximum)) = join::integer_key_span(&keys)
        {
            self.restrict_probe_range(position, &minimum, &maximum);
        }
        let integers = join::IntegerKeySet::from_keys(&keys);
        let input = std::mem::replace(self, Self::Empty);
        *self = Self::KeyFilter {
            input: Box::new(input),
            key,
            key_mode,
            keys,
            integers,
        };
    }

    /// Transient headroom the underlying scan needs to pull one more batch.
    /// Operators that buffer their whole input (the two-pass aggregate) must
    /// keep at least this much budget free or the scan itself stops pulling.
    fn scan_transient_floor(&self) -> usize {
        match self {
            Self::Scan { stream, .. } => stream.next_batch_memory_upper_bound(usize::MAX),
            Self::Filter { input, .. }
            | Self::KeyFilter { input, .. }
            | Self::Profiled { input, .. } => input.scan_transient_floor(),
            _ => 0,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, memory: &MemoryTracker) -> Result<Option<RecordBatch>, ExecError> {
        memory.check_interruption()?;
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
                let per_row = state.next_batch_memory_upper_bound(1, column_types.len());
                let planned = affordable_batch_rows(memory, per_row);
                let transient = state.next_batch_memory_upper_bound(planned, column_types.len());
                memory.ensure_transient(transient)?;
                let rows = state.next_rows(planned);
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
                residual,
                residual_columns,
                collation,
                probe_prefetch,
            } => {
                if state.is_none() {
                    // A small probe side is read in full first: its keys
                    // then filter the build side before the hash table is
                    // built, which is what turns a join against a large
                    // table into one against the rows that can match.
                    let prefetch = if *probe_prefetch {
                        prefetch_probe(left, left_key, *key_mode, memory)?
                    } else {
                        ProbePrefetch::default()
                    };
                    if let Some(keys) = &prefetch.keys {
                        right.restrict_build_keys(
                            right_key.clone(),
                            *key_mode,
                            std::sync::Arc::clone(keys),
                        );
                    }
                    let mut built = build_hash_join_state(
                        right,
                        right_key,
                        *key_mode,
                        extra_keys,
                        left.scan_transient_floor(),
                        memory,
                        *collation,
                    )?;
                    // Inner and semi joins cannot match probe rows outside
                    // the build side's key range, so the probe scan can prune
                    // storage before decoding anything. Left/anti joins need
                    // every probe row, and so does a null-safe key, whose
                    // NULL lies outside any range yet may match.
                    if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Semi)
                        && !key_mode.null_safe
                        && let Some((minimum, maximum)) = &built.key_bounds
                        && let Some(position) = left_key.column_index()
                    {
                        left.restrict_probe_range(position, minimum, maximum);
                    }
                    built.adopt_prefetch(prefetch);
                    if matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Semi)
                        && matches!(key_mode.form, KeyForm::Integer)
                        && !extra_keys.is_empty()
                        && let Some(position) = left_key.column_index()
                        && let Some((scan, position)) = left.nested_probe_scan(position, false)
                        && let Some(keys) = built.primary_membership(memory)
                    {
                        scan.restrict_build_keys(CompiledExpr::Column(position), *key_mode, keys);
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
                    residual.as_ref(),
                    residual_columns,
                    state.as_mut().expect("initialized above"),
                    memory,
                )
            }
            Self::Profiled { input, slot, sink } => {
                let started = Instant::now();
                let result = input.next_batch(memory);
                let rows = match &result {
                    Ok(Some(batch)) => Some(batch.visible_row_count()),
                    _ => None,
                };
                sink.record(*slot, started.elapsed(), rows, memory.used());
                result
            }
            Self::KeyFilter {
                input,
                key,
                key_mode,
                keys,
                integers,
            } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    // Release the retained membership storage before the
                    // owning join returns its reservation at probe exhaustion.
                    *keys = std::sync::Arc::new(std::collections::HashSet::new());
                    *integers = None;
                    return Ok(None);
                };
                let batch_bytes = batch.estimated_bytes();
                // A typed integer key column against integer probe keys is
                // one bit test per row. Rows to drop are collected first,
                // since the selection cannot change while it is being read.
                let dropped = integers.as_ref().and_then(|set| {
                    let column = batch.column(key.column_index()?)?;
                    let (typed, validity) = column.typed()?;
                    let mut dropped = Vec::new();
                    match typed {
                        crate::batch::TypedValues::Int64(values) => {
                            for row in batch.selection().selected_rows() {
                                if !validity.is_valid(row) || !set.contains(i128::from(values[row]))
                                {
                                    dropped.push(row);
                                }
                            }
                        }
                        crate::batch::TypedValues::UInt64(values) => {
                            for row in batch.selection().selected_rows() {
                                if !validity.is_valid(row) || !set.contains(i128::from(values[row]))
                                {
                                    dropped.push(row);
                                }
                            }
                        }
                        _ => return None,
                    }
                    Some(dropped)
                });
                if let Some(dropped) = dropped {
                    memory.ensure_transient(batch_bytes)?;
                    for row in dropped {
                        batch.selection_mut().set(row, false)?;
                    }
                } else {
                    for row in 0..batch.row_count() {
                        if !batch.selection().is_selected(row) {
                            continue;
                        }
                        memory.ensure_transient(
                            batch_bytes.saturating_add(key.allocation_upper_bound(&batch, row)),
                        )?;
                        let keep = normalized_join_key(key.evaluate(&batch, row)?, *key_mode)?
                            .is_some_and(|candidate| keys.contains(&candidate));
                        if !keep {
                            batch.selection_mut().set(row, false)?;
                        }
                    }
                }
                if batch.visible_row_count() > 0 {
                    return Ok(Some(batch));
                }
            },
            Self::Filter { input, predicate } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                // Typed batch kernel: comparison predicates over packed
                // columns resolve in one pass. Anything else the kernel tree
                // answers - a function of a column, arithmetic, a decimal
                // comparison - resolves as a Boolean column, which is the
                // same answer a batch at a time. Only an expression no
                // kernel answers falls to the row-at-a-time path below.
                let mask = match predicate.evaluate_filter_mask(&batch)? {
                    Some(mask) => Some(mask),
                    None => predicate.evaluate_mask(&batch),
                };
                if let Some(mask) = mask {
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
                collation,
                key_collations,
            } => {
                if state.is_none() {
                    *state = Some(build_hash_aggregate(
                        input,
                        group_by,
                        aggregates,
                        memory,
                        *collation,
                        key_collations,
                    )?);
                }
                let state = state.as_mut().expect("initialized above");
                if state.position >= state.rows.len()
                    && let Some(spilled) = state.spilled.as_mut()
                {
                    let rows = spilled.next_chunk(aggregates, memory)?;
                    if rows.is_empty() {
                        spilled.finish(memory);
                        state.spilled = None;
                        return Ok(None);
                    }
                    state.rows = rows;
                    state.position = 0;
                }
                next_materialized_batch(state, column_types, memory)
            }
            Self::Project { input, expressions } => {
                let Some(batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                // A bare column of the declared type passes through: the
                // output shares its buffers, and it is never evaluated row by
                // row. Only computed expressions evaluate.
                let passthrough: Vec<Option<usize>> = expressions
                    .iter()
                    .map(|(expression, data_type)| {
                        let position = expression.column_index()?;
                        let column = batch.column(position)?;
                        data_type
                            .is_none_or(|data_type| data_type == column.data_type())
                            .then_some(position)
                    })
                    .collect();
                if passthrough.iter().all(Option::is_some) {
                    let positions = passthrough.into_iter().flatten().collect::<Vec<_>>();
                    memory.ensure_transient(
                        batch.estimated_bytes().saturating_add(
                            positions
                                .len()
                                .saturating_mul(size_of::<ColumnVector>() * 2),
                        ),
                    )?;
                    return batch.project_columns(&positions).map(Some).ok_or(
                        ExecError::InvalidBatch("projection column is outside its input"),
                    );
                }
                let computed = expressions
                    .iter()
                    .zip(&passthrough)
                    .filter(|(_, position)| position.is_none())
                    .map(|((expression, _), _)| expression)
                    .collect::<Vec<_>>();
                let batch_bytes = batch.estimated_bytes();
                let expression_memory = computed
                    .iter()
                    .map(|expression| {
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
                        computed
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
                for ((expression, data_type), position) in expressions.iter().zip(&passthrough) {
                    if let Some(position) = position {
                        columns.push(batch.columns()[*position].clone());
                        continue;
                    }
                    // A whole batch at a time over packed units where the
                    // expression has kernels; row by row where it has not.
                    if let Some(column) = expression.evaluate_column(&batch, *data_type) {
                        columns.push(column);
                        continue;
                    }
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
                    crate::counters::count(|counters| {
                        counters.rows_projected_scalar = counters
                            .rows_projected_scalar
                            .saturating_add(u64::try_from(batch.row_count()).unwrap_or(u64::MAX));
                    });
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
                collation,
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
                        *collation,
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
                collation,
                key_collations,
            } => {
                if state.is_none() {
                    *state = Some(build_distinct(
                        input,
                        column_types,
                        key_collations,
                        memory,
                        *collation,
                    )?);
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
                collation,
            } => {
                if state.is_none() {
                    *state = Some(build_window(
                        input,
                        windows,
                        column_types,
                        memory,
                        *collation,
                    )?);
                }
                state
                    .as_mut()
                    .expect("initialized above")
                    .next_batch(column_types, memory)
            }
            Self::Sort {
                input,
                keys,
                column_types,
                top_k,
                trim,
                state,
                collation,
            } => {
                if state.is_none() {
                    let trim_to = if *trim > 0 {
                        // Hidden sort-only columns ordered the rows; the
                        // output layout never contains them.
                        Some(column_types.len())
                    } else {
                        None
                    };
                    *state = Some(build_sort(
                        input, keys, *top_k, trim_to, memory, *collation,
                    )?);
                }
                state
                    .as_mut()
                    .expect("initialized above")
                    .next_batch(column_types, memory)
            }
            Self::KeyLookupJoin(join) => join.next_batch(memory),
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

/// Lowers one plan node, wrapped in a profile recorder when the execution
/// is profiled. The recursion below goes through here, so every node of
/// a profiled plan is measured and an unprofiled plan is built as before.
fn build_operator(
    plan: PhysicalPlan,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<(PullOperator, Vec<BoundColumn>), ExecError> {
    let Some(sink) = memory.profile.as_ref() else {
        return build_operator_inner(plan, provider, memory, collation);
    };
    let slot = sink.enter(plan_label(&plan));
    let started = Instant::now();
    let built = build_operator_inner(plan, provider, memory, collation);
    sink.leave(slot, started.elapsed());
    let (operator, columns) = built?;
    Ok((
        PullOperator::Profiled {
            input: Box::new(operator),
            slot,
            sink: std::sync::Arc::clone(sink),
        },
        columns,
    ))
}

/// The operator beneath any profile recorders, for callers that inspect
/// an input's shape rather than pull from it.
fn unprofiled(operator: &mut PullOperator) -> &mut PullOperator {
    match operator {
        PullOperator::Profiled { input, .. } => unprofiled(input),
        other => other,
    }
}

/// The same, read-only.
fn unprofiled_ref(operator: &PullOperator) -> &PullOperator {
    match operator {
        PullOperator::Profiled { input, .. } => unprofiled_ref(input),
        other => other,
    }
}

/// Records on an operator's profile that something other than its own
/// pulls decided its fate; a no-op when the execution is not profiled.
fn annotate_profile(operator: &PullOperator, note: &str) {
    if let PullOperator::Profiled { slot, sink, .. } = operator {
        sink.annotate(*slot, note);
    }
}

#[allow(clippy::too_many_lines)]
fn build_operator_inner(
    plan: PhysicalPlan,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
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
                .map(|predicate| CompiledExpr::compile(predicate, &columns, collation))
                .collect::<Result<Vec<_>, _>>()?;
            let stream = match points::open(&scan, provider, memory.remaining())? {
                Some(runs) => runs,
                None => provider.open_scan(&scan, memory.remaining())?,
            };
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
            // A field with no data type is an untyped NULL projection
            // (SELECT NULL AS k) - compatible with any bound column type,
            // and refusing it made GROUP BY over a NULL-only derived table
            // an internal error (conformance suite).
            if fields.len() != columns.len()
                || fields.iter().zip(&columns).any(|(field, column)| {
                    field
                        .data_type
                        .is_some_and(|data_type| data_type != column.data_type)
                })
            {
                return Err(ExecError::InvalidPhysicalPlan(
                    "derived input layout does not match its bound columns",
                ));
            }
            let (input, _) = build_operator(*input, provider, memory, collation)?;
            Ok((input, columns))
        }
        PhysicalPlan::CrossJoin {
            inputs,
            estimated_rows: _,
        } => {
            let mut built = Vec::with_capacity(inputs.len());
            for input in inputs {
                built.push(build_operator(input, provider, memory, collation)?);
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
                built.push(build_operator(input, provider, memory, collation)?);
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
            null_safe,
            probe_estimate,
            build_estimate,
            residual,
        } => {
            let probe_prefetch = probe_prefetch_applies(kind, probe_estimate, build_estimate);
            let (left, left_columns) = build_operator(*left, provider, memory, collation)?;
            let (right, right_columns) = build_operator(*right, provider, memory, collation)?;
            // Each join key decides its own collation from the columns it
            // compares, so a plan may join general_ci here and 0900_ai_ci in
            // the next operator. Both sides of ONE key must agree - that is
            // the undecidable case, and the binder has already refused it -
            // so taking the left side's is safe.
            let key_collation = key_collation_of(&left_key, collation);
            let null_safe_at = |key: usize| null_safe.get(key).copied().unwrap_or(false);
            let key_mode =
                hash_join_key_mode(left_key.data_type, right_key.data_type, key_collation)
                    .map(|mode| JoinKeyMode {
                        null_safe: null_safe_at(0),
                        ..mode
                    })
                    .ok_or(ExecError::InvalidPhysicalPlan(
                        "hash join keys have incompatible scalar types",
                    ))?;
            let extra_keys = extra_keys
                .into_iter()
                .enumerate()
                .map(|(index, (extra_left, extra_right))| {
                    let mode = hash_join_key_mode(
                        extra_left.data_type,
                        extra_right.data_type,
                        key_collation_of(&extra_left, collation),
                    )
                    .map(|mode| JoinKeyMode {
                        null_safe: null_safe_at(index + 1),
                        ..mode
                    })
                    .ok_or(ExecError::InvalidPhysicalPlan(
                        "hash join keys have incompatible scalar types",
                    ))?;
                    Ok((
                        CompiledExpr::compile(&extra_left, &left_columns, collation)?,
                        CompiledExpr::compile(&extra_right, &right_columns, collation)?,
                        mode,
                    ))
                })
                .collect::<Result<Vec<_>, ExecError>>()?;
            let left_key = CompiledExpr::compile(&left_key, &left_columns, collation)?;
            let right_key = CompiledExpr::compile(&right_key, &right_columns, collation)?;
            let right_width = right_columns.len();
            // The residual reads both sides, so it compiles against the
            // concatenation - which is NOT output_columns, because semi and
            // anti joins project the left side alone.
            let mut residual_columns = left_columns.clone();
            residual_columns.extend(right_columns.iter().cloned());
            let residual = residual
                .map(|predicate| CompiledExpr::compile(&predicate, &residual_columns, collation))
                .transpose()?;
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
                    residual,
                    residual_columns,
                    collation,
                    probe_prefetch,
                },
                output_columns,
            ))
        }
        PhysicalPlan::KeyLookupJoin {
            left,
            right,
            kind,
            driving_left,
            driving_key,
            lookup_key,
            residual,
        } => key_lookup::build(
            key_lookup::Inputs {
                left: *left,
                right: *right,
                kind,
                driving_left,
                driving_key,
                lookup_key,
                residual,
            },
            provider,
            memory,
            collation,
        ),
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            kind,
            keys,
            condition,
        } => {
            let (mut left, left_columns) = build_operator(*left, provider, memory, collation)?;
            let (mut right, right_columns) = build_operator(*right, provider, memory, collation)?;
            let mut output_columns = left_columns.clone();
            if !matches!(kind, BoundJoinKind::Semi | BoundJoinKind::Anti) {
                output_columns.extend(right_columns.clone());
            }
            let column_types = output_columns
                .iter()
                .map(|column| column.data_type)
                .collect::<Vec<_>>();
            let rows = execute_nested_loop_join(
                &mut left,
                &mut right,
                &left_columns,
                &right_columns,
                kind,
                &keys,
                &condition,
                provider,
                memory,
                collation,
            )?;
            Ok((
                PullOperator::Sort {
                    input: Box::new(PullOperator::Empty),
                    keys: Vec::new(),
                    trim: column_types.len(),
                    column_types,
                    top_k: None,
                    state: Some(rows),
                    collation,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Filter { input, predicate } => {
            let dependent = expression_has_dependent_subquery(&predicate);
            let (mut input, columns) = build_operator(*input, provider, memory, collation)?;
            if dependent {
                let column_types = columns
                    .iter()
                    .map(|column| column.data_type)
                    .collect::<Vec<_>>();
                let mut rows = Vec::new();
                let mut memo = DependentMemo::for_expressions(std::iter::once(&predicate));
                while let Some(batch) = input.next_batch(memory)? {
                    let batch_bytes = batch.estimated_bytes();
                    for row in batch.selection().selected_rows() {
                        let mut expression = predicate.clone();
                        let context = DependentRow {
                            batch: &batch,
                            row,
                            columns: &columns,
                            provider,
                            memory,
                            collation,
                        };
                        memo.begin_row();
                        resolve_dependent_expr_subqueries(&mut expression, &context, &mut memo)?;
                        let compiled = CompiledExpr::compile(&expression, &columns, collation)?;
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
                record_dependent_memo(memo.finish(memory));
                return Ok((
                    PullOperator::Rows {
                        rows,
                        cursor: 0,
                        column_types,
                    },
                    columns,
                ));
            }
            let predicate = CompiledExpr::compile(&predicate, &columns, collation)?;
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
            let (input, columns) = build_operator(*input, provider, memory, collation)?;
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
                    enum_labels: None,
                    geometry: false,
                    timestamp: false,
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
            // Each grouping key folds under ITS OWN collation - grouping
            // never compares one key column against another, so keys of two
            // collations are answerable, exactly as a sort orders each key
            // by its own rules. The uniform collation, when the keys share
            // one, keeps the specialized single-table paths eligible; mixed
            // keys take the general per-key path.
            let (key_collations, uniform_key_collation) =
                grouping_key_collations(&group_by, collation);
            let group_collation = uniform_key_collation.unwrap_or(collation);
            let group_by = group_by
                .iter()
                .zip(&key_collations)
                .map(|(expression, key_collation)| {
                    CompiledExpr::compile(expression, &columns, *key_collation)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let aggregates = aggregates
                .iter()
                .map(|aggregate| CompiledAggregate::compile(aggregate, &columns, collation))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                PullOperator::HashAggregate {
                    input: Box::new(input),
                    group_by,
                    aggregates,
                    column_types,
                    state: None,
                    collation: group_collation,
                    key_collations,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Project { input, expressions } => {
            let dependent = expressions
                .iter()
                .any(|projection| expression_has_dependent_subquery(&projection.expr));
            let (mut input, columns) = build_operator(*input, provider, memory, collation)?;
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
                        enum_labels: None,
                        geometry: false,
                        timestamp: false,
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
                let mut memo = DependentMemo::for_expressions(
                    expressions.iter().map(|projection| &projection.expr),
                );
                while let Some(batch) = input.next_batch(memory)? {
                    let batch_bytes = batch.estimated_bytes();
                    for row in batch.selection().selected_rows() {
                        let mut values = Vec::with_capacity(expressions.len());
                        let context = DependentRow {
                            batch: &batch,
                            row,
                            columns: &columns,
                            provider,
                            memory,
                            collation,
                        };
                        memo.begin_row();
                        for projection in &expressions {
                            let mut expression = projection.expr.clone();
                            resolve_dependent_expr_subqueries(
                                &mut expression,
                                &context,
                                &mut memo,
                            )?;
                            let compiled = CompiledExpr::compile(&expression, &columns, collation)?;
                            values.push(compiled.evaluate(&batch, row)?);
                        }
                        let row_bytes = estimated_row_payload_bytes(&values);
                        memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
                        memory.reserve(row_bytes)?;
                        rows.push(values);
                    }
                }
                record_dependent_memo(memo.finish(memory));
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
                        CompiledExpr::compile(&projection.expr, &columns, collation)?,
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
            let (mut anchor_op, columns) = build_operator(*anchor, provider, memory, collation)?;
            let mut seen: HashSet<Vec<Value>> = HashSet::new();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let mut delta = drain_recursive_rows(
                &mut anchor_op,
                distinct,
                &mut seen,
                &mut rows,
                memory,
                collation,
            )?;
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
                let (mut member_op, _) =
                    build_operator((*member).clone(), &overlay, memory, collation)?;
                delta = drain_recursive_rows(
                    &mut member_op,
                    distinct,
                    &mut seen,
                    &mut rows,
                    memory,
                    collation,
                )?;
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
            let (left, columns) = build_operator(*left, provider, memory, collation)?;
            let (right, _) = build_operator(*right, provider, memory, collation)?;
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
                    collation,
                },
                columns,
            ))
        }
        PhysicalPlan::Distinct {
            input,
            key_collations,
        } => {
            let column_types = input
                .output_fields()
                .into_iter()
                .map(|field| field.data_type.unwrap_or(DataType::Utf8))
                .collect();
            let (input, columns) = build_operator(*input, provider, memory, collation)?;
            Ok((
                PullOperator::Distinct {
                    input: Box::new(input),
                    column_types,
                    state: None,
                    collation,
                    key_collations,
                },
                columns,
            ))
        }
        PhysicalPlan::Window {
            input,
            windows,
            outputs,
        } => {
            let (input_op, mut columns) = build_operator(*input, provider, memory, collation)?;
            let compiled = windows
                .iter()
                .map(|window| CompiledWindow::compile(window, &columns, collation))
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
                    collation,
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
            let (input, mut columns) = build_operator(*input, provider, memory, collation)?;
            columns.truncate(visible);
            Ok((
                PullOperator::Sort {
                    input: Box::new(input),
                    keys,
                    column_types,
                    top_k,
                    trim,
                    state: None,
                    collation,
                },
                columns,
            ))
        }
        PhysicalPlan::Limit {
            input,
            offset,
            count,
        } => {
            let (input, columns) = build_operator(*input, provider, memory, collation)?;
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

fn scalar_string_memory_upper_bound(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int64(_) | Value::UInt64(_) | Value::Float64(_) => 24,
        Value::Utf8(value) | Value::Enum { label: value, .. } => value.len(),
        Value::DecimalAverage(average) => {
            let value = &average.label;
            value.len()
        }
        Value::Binary(value) => value.len(),
    }
}

fn batch_row(batch: &RecordBatch, row: usize) -> Result<Vec<Value>, ExecError> {
    batch
        .columns()
        .iter()
        .map(|column| {
            column.value_owned(row).ok_or(ExecError::InvalidBatch(
                "join row is outside an input column",
            ))
        })
        .collect()
}

struct MaterializedRows {
    rows: Vec<Vec<Value>>,
    position: usize,
    /// An aggregate whose group map spilled serves its merged result from
    /// here, a chunk at a time into `rows`, instead of holding it whole.
    spilled: Option<aggregate::SpilledGroupMerge>,
}

fn next_materialized_batch(
    state: &mut MaterializedRows,
    column_types: &[DataType],
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    if state.position >= state.rows.len() {
        return Ok(None);
    }
    let per_row = estimated_record_batch_bytes(
        &state.rows[state.position..state.rows.len().min(state.position + 1)],
        column_types.len(),
    );
    let end = state
        .position
        .saturating_add(affordable_batch_rows(memory, per_row))
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

fn rows_to_columns(
    rows: &[Vec<Value>],
    column_types: &[DataType],
) -> Result<Vec<ColumnVector>, ExecError> {
    crate::counters::count(|counters| {
        counters.cells_regathered = counters.cells_regathered.saturating_add(
            u64::try_from(rows.len().saturating_mul(column_types.len())).unwrap_or(u64::MAX),
        );
    });
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
    // Grow geometrically. Growing by exactly the requested slot asked the
    // allocator for a larger block on every push, and only in-place
    // reallocation kept that from copying the whole vector each time. The
    // charge below already assumed a power-of-two capacity, so doubling
    // spends nothing the budget did not already count.
    let growth = required
        .saturating_sub(old_capacity)
        .max(minimum_growth)
        .max(old_capacity.max(4));
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

/// Rows to plan for, given what the query has left to spend.
///
/// `per_row_bytes` MUST come from the same function that will reserve. That is
/// the whole difficulty: `estimated_row_payload_bytes` reads about 33 bytes for
/// a row that `estimated_record_batch_bytes` charges 433 for once column and
/// validity overhead are counted, so a cap computed from the wrong estimate
/// does not bind and the reservation still fails.
///
/// A fixed row count makes a tight ceiling fail on the first pull when the
/// honest answer is a smaller batch - which is what the `BatchStream` contract
/// already asks for, and what the storage scan already does. With a normal
/// budget this returns the target unchanged, so it is inert until memory is
/// actually short.
///
/// At least one row is always planned, so a budget below a single row fails on
/// the real reservation with a truthful number rather than yielding nothing
/// forever.
pub(super) fn affordable_batch_rows(memory: &MemoryTracker, per_row_bytes: usize) -> usize {
    let remaining = memory.limit().saturating_sub(memory.used());
    let affordable = (remaining / per_row_bytes.max(1)).max(1);
    DEFAULT_BATCH_ROWS.min(affordable)
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
            let bytes = column
                .scalar_heap_bytes(row)
                .ok_or(ExecError::InvalidBatch("row is outside an input column"))?;
            Ok::<_, ExecError>(heap_bytes.saturating_add(bytes))
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
    collation: Collation,
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
                    .map(|value| normalized_collation_value(value, collation))
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
    if batch.row_count() > crate::batch::MAX_SCAN_BATCH_ROWS {
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

#[cfg(test)]
mod tests {
    use crate::collation::Collation;
    use std::{
        collections::{HashMap, VecDeque},
        mem::size_of,
        sync::Mutex,
    };

    #[test]
    fn parallel_worker_trackers_inherit_query_cancellation() {
        let cancellation = super::ExecutionCancellation::new();
        let parent = super::with_execution_cancellation(cancellation.clone(), || {
            super::MemoryTracker::new(usize::MAX)
        });
        cancellation.cancel();

        let (outcome, ()) = rayon::join(|| parent.unbounded_worker().check_interruption(), || ());
        assert!(matches!(outcome, Err(super::ExecError::QueryCancelled)));
    }

    #[test]
    fn dictionary_grouping_merges_collation_equivalent_codes() {
        let provider = StaticProvider {
            batches: Mutex::new(vec![
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
                                Value::Utf8("É".to_owned()),
                                Value::Utf8("e".to_owned()),
                                Value::Utf8("z".to_owned()),
                            ],
                        )
                        .expect("names"),
                    ],
                )
                .expect("batch"),
            ]),
        };
        let plan =
            physical("SELECT name, COUNT(id) AS rows FROM events GROUP BY name ORDER BY name");
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");

        assert_eq!(batch.visible_row_count(), 2);
        assert_eq!(
            batch.column(0).expect("names").values(),
            [Value::Utf8("É".to_owned()), Value::Utf8("z".to_owned())]
        );
        assert_eq!(
            batch.column(1).expect("counts").values(),
            [Value::UInt64(2), Value::UInt64(1)]
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
        PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical")
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");

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
        let mut execution = Execution::start(plan, &provider, 4 * 1024, Collation::default())
            .expect("decimal comparison execution");
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
        let mut execution = Execution::start(plan, &provider, 4 * 1024, Collation::default())
            .expect("decimal set execution");
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
        let mut execution = Execution::start(plan, &provider, 4 * 1024, Collation::default())
            .expect("decimal chain execution");
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
            PhysicalPlanner::plan(
                Optimizer::optimize(LogicalPlanner::plan(bound)),
                Collation::default(),
            )
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
            Collation::default(),
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
            Collation::default(),
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
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
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
        let mut execution = Execution::start(plan, &provider, 64 * 1024, Collation::default())
            .expect("JSON execution");
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
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
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
        let mut execution = Execution::start(plan, &provider, 64 * 1024, Collation::default())
            .expect("JSON execution");
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
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
                Value::Int64(100),
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 16 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
                Value::Int64(2024),
                Value::Int64(2),
                Value::Int64(29),
                Value::Int64(12),
                Value::Int64(34),
                Value::Int64(56),
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
        let mut execution = Execution::start(invalid, &provider, 32 * 1024, Collation::default())
            .expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 32 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 1, Collation::default()).expect("execution");
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
            Execution::start(
                plan.clone(),
                &provider,
                program_memory - 1,
                Collation::default()
            ),
            Err(ExecError::MemoryLimitExceeded { .. })
        ));
        let limit = program_memory + 4 * 1024;
        let mut execution =
            Execution::start(plan, &provider, limit, Collation::default()).expect("execution");
        assert!(matches!(
            execution.next_batch(),
            Err(ExecError::MemoryLimitExceeded {
                limit: actual,
                .. }) if actual == limit
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
                Collation::default(),
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
            Execution::start(plan, &provider, 800, Collation::default()),
            Err(ExecError::MemoryLimitExceeded { limit, .. }) if limit <= 800
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
            Collation::default(),
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
                Collation::default(),
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
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
    fn large_in_membership_spills_and_preserves_nulls() {
        let batches = (0..64)
            .map(|batch| {
                let names = (0..128)
                    .map(|row| {
                        if batch == 0 && row == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(format!(
                                "key-{:05}-with-a-retained-membership-payload",
                                batch * 128 + row
                            ))
                        }
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit, sql: &str| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution =
                Execution::start(physical(sql), &provider, limit, Collation::default())
                    .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|column| column.value(row).cloned().expect("value"))
                            .collect::<Vec<_>>(),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let sql = "SELECT 'missing' NOT IN (SELECT name FROM events), 'key-00001-with-a-retained-membership-payload' IN (SELECT name FROM events), 'KEY-00001-WITH-A-RETAINED-MEMBERSHIP-PAYLOAD' COLLATE utf8mb4_bin IN (SELECT name FROM events)";
        let (wide, _) = execute(64 * 1024 * 1024, sql);
        let (tight, spill) = execute(512 * 1024, sql);
        assert_eq!(tight, wide);
        assert_eq!(
            tight,
            vec![vec![Value::Null, Value::Boolean(true), Value::Null]]
        );
        assert!(spill.files > 0);
        let correlated = "SELECT l.k IN (SELECT r.name FROM events r WHERE LENGTH(r.name) > LENGTH(l.k)) FROM (SELECT 'x' AS k) l";
        let (wide, _) = execute(64 * 1024 * 1024, correlated);
        let (tight, spill) = execute(512 * 1024, correlated);
        assert_eq!(tight, wide);
        assert_eq!(tight, vec![vec![Value::Boolean(false)]]);
        assert!(spill.files > 0);
    }

    #[test]
    fn oversized_grace_key_streams_every_match() {
        let batches = (0..64)
            .map(|batch| {
                let ids = (0..8)
                    .map(|row| Value::UInt64(batch * 8 + row))
                    .collect::<Vec<_>>();
                let names = vec![Value::Utf8("x".repeat(1024)); 8];
                RecordBatch::new(
                    8,
                    vec![
                        ColumnVector::new(DataType::UInt64, ids).expect("ids"),
                        ColumnVector::new(DataType::Utf8, names).expect("names"),
                    ],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(
                physical(
                    "SELECT r.id FROM events l JOIN events r ON l.name = r.name WHERE l.id = 0",
                ),
                &provider,
                limit,
                Collation::default(),
            )
            .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .column(0)
                            .expect("column")
                            .value(row)
                            .cloned()
                            .expect("value"),
                    );
                }
            }
            rows.sort_by(|left, right| match (left, right) {
                (Value::UInt64(left), Value::UInt64(right)) => left.cmp(right),
                _ => unreachable!(),
            });
            (rows, execution.spill_metrics())
        };
        let (wide, _) = execute(64 * 1024 * 1024);
        let (tight, spill) = execute(512 * 1024);
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 512);
        assert!(spill.files > 0);
    }

    /// The dependent path a correlated subquery in an OUTER join's ON still
    /// takes. An INNER join's ON hoists to WHERE and decorrelates, so this
    /// shape has to be an outer join to reach the replay it is testing, and
    /// it correlates to the join's RIGHT side, which is the side that still
    /// answers: correlating to the left is refused, because there the
    /// subquery ran without its outer context and the join matched too few
    /// rows. The decorrelated form is covered by the test below.
    #[test]
    fn correlated_join_on_spills_its_replayed_side_and_output() {
        let batches = (0..64)
            .map(|batch| {
                let names = (0..8)
                    .map(|row| {
                        Value::Utf8(format!("key-{:04}-{}", batch * 8 + row, "x".repeat(1024)))
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(physical("SELECT l.name FROM events l LEFT JOIN events r ON l.name = r.name AND EXISTS (SELECT 1 FROM events z WHERE z.name = r.name)"), &provider, limit, Collation::default()).expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .column(0)
                            .expect("column")
                            .value(row)
                            .cloned()
                            .expect("value"),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let (wide, _) = execute(64 * 1024 * 1024);
        let (tight, spill) = execute(256 * 1024);
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 512);
        assert!(spill.files > 0);
    }

    /// The same question as the test above with an INNER join, which hoists
    /// its correlated EXISTS to WHERE and becomes a semi-join. The point is
    /// that the path it moves ONTO still answers under a budget and still
    /// spills: a rewrite that traded a spilling plan for one that dies at a
    /// memory ceiling would be a poor trade however much faster it is when
    /// it fits.
    #[test]
    fn a_decorrelated_join_condition_still_spills_under_a_budget() {
        let batches = (0..64)
            .map(|batch| {
                let names = (0..8)
                    .map(|row| {
                        Value::Utf8(format!("key-{:04}-{}", batch * 8 + row, "x".repeat(1024)))
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(
                physical(
                    "SELECT l.name FROM events l JOIN events r ON l.name = r.name AND EXISTS \
                     (SELECT 1 FROM events z WHERE z.name = l.name)",
                ),
                &provider,
                limit,
                Collation::default(),
            )
            .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .column(0)
                            .expect("column")
                            .value(row)
                            .cloned()
                            .expect("value"),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        // No ORDER BY, so row order is unspecified and the grace-partitioned
        // build hands them back in partition order once it spills. The rows
        // are the claim; their sequence is not.
        let (mut wide, _) = execute(64 * 1024 * 1024);
        let (mut tight, spill) = execute(4 * 1024 * 1024);
        wide.sort();
        tight.sort();
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 512);
        assert!(
            spill.files > 0,
            "the decorrelated plan spills rather than failing",
        );
    }

    #[test]
    fn collection_aggregates_spill_fragments_in_order() {
        super::set_session_group_concat_max_len(Some(127));
        let batches = (0..64)
            .map(|batch| {
                let names = (0..128)
                    .map(|row| {
                        Value::Utf8(format!(
                            "key-{:04}-value-{:04}",
                            (batch * 128 + row) % 256,
                            batch % 16
                        ))
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(physical("SELECT LEFT(name, 8) AS k, GROUP_CONCAT(name ORDER BY name DESC SEPARATOR '|'), GROUP_CONCAT(DISTINCT name ORDER BY name DESC), JSON_ARRAYAGG(name) FROM events GROUP BY k"), &provider, limit, Collation::default()).expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|column| column.value(row).cloned().expect("value"))
                            .collect::<Vec<_>>(),
                    );
                }
            }
            rows.sort_by(|left, right| match (&left[0], &right[0]) {
                (Value::Utf8(left), Value::Utf8(right)) => left.cmp(right),
                _ => unreachable!(),
            });
            (rows, execution.spill_metrics())
        };
        let (wide, _) = execute(64 * 1024 * 1024);
        let (tight, spill) = execute(1024 * 1024);
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 256);
        assert!(spill.files > 0);
        super::set_session_group_concat_max_len(None);
    }

    #[test]
    fn partitioned_window_output_spills_under_the_ceiling() {
        let batches = (0..64)
            .map(|batch| {
                let names = (0..128)
                    .map(|row| Value::Utf8(format!("key-{:04}", (batch * 128 + row) % 512)))
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    names.len(),
                    vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(
                physical("SELECT name, ROW_NUMBER() OVER (PARTITION BY name ORDER BY name), COUNT(*) OVER (PARTITION BY name) FROM events"),
                &provider, limit, Collation::default(),
            ).expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|column| column.value(row).cloned().expect("value"))
                            .collect::<Vec<_>>(),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let (wide, _) = execute(64 * 1024 * 1024);
        let (tight, spill) = execute(512 * 1024);
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 8192);
        assert!(spill.files > 0);
    }

    #[test]
    fn expanded_window_values_spill_from_a_small_input() {
        let names = (0..512)
            .map(|row| Value::Utf8(format!("key-{:04}", row % 2)))
            .collect::<Vec<_>>();
        let batch = RecordBatch::new(
            names.len(),
            vec![ColumnVector::new(DataType::Utf8, names).expect("names")],
        )
        .expect("batch");
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(vec![batch.clone()]),
            };
            let mut execution = Execution::start(
                physical("SELECT GROUP_CONCAT(name) OVER (PARTITION BY name) FROM events"),
                &provider,
                limit,
                Collation::default(),
            )
            .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                assert!(execution.memory().used() <= limit);
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .column(0)
                            .and_then(|column| column.value(row))
                            .cloned()
                            .expect("value"),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let (wide, _) = execute(64 * 1024 * 1024);
        let (tight, spill) = execute(512 * 1024);
        assert_eq!(tight, wide);
        assert_eq!(tight.len(), 512);
        assert!(spill.files > 0);
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
                Collation::default(),
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
    fn a_spilled_sort_orders_enums_by_declaration_like_an_unspilled_one() {
        // MySQL orders an ENUM by its declared position, not its label, so
        // a sort that spills has to carry the ordinal through the run. The
        // labels here are deliberately in the opposite order to the
        // declaration: comparing text would reverse the result.
        let labels = ["zulu", "yankee", "xray", "whiskey", "victor"];
        let batches = (0..48)
            .map(|batch| {
                let values = (0..512)
                    .map(|row| {
                        let ordinal = (batch * 512 + row) % 5;
                        Value::Enum {
                            index: u64::try_from(ordinal + 1).expect("small"),
                            label: format!(
                                "{}-{:04}",
                                labels[usize::try_from(ordinal).expect("small")],
                                (batch * 512 + row) % 97
                            ),
                        }
                    })
                    .collect::<Vec<_>>();
                RecordBatch::new(
                    values.len(),
                    vec![ColumnVector::new(DataType::Utf8, values).expect("grades")],
                )
                .expect("batch")
            })
            .collect::<Vec<_>>();
        let execute = |limit| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution = Execution::start(
                physical("SELECT name FROM events ORDER BY name"),
                &provider,
                limit,
                Collation::default(),
            )
            .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .column(0)
                            .and_then(|column| column.value(row))
                            .cloned()
                            .expect("value"),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let (memory, memory_spill) = execute(64 * 1024 * 1024);
        let (spilled, spill) = execute(512 * 1024);
        assert_eq!(memory_spill.files, 0);
        assert!(spill.files > 0, "the tight execution must use spill files");
        assert_eq!(
            spilled, memory,
            "a spilled sort must order enums the way an in-memory one does"
        );
    }

    #[test]
    fn a_spilled_aggregate_serves_a_result_larger_than_its_ceiling() {
        // 20,000 groups whose finished rows come to several times the tight
        // ceiling: the map spills while grouping, and the merged result must
        // then be served in chunks rather than held whole, or the query
        // fails on its own output after spilling correctly. The expression
        // key keeps this on the general path; the streaming two-pass path
        // over an input that reports no transient floor is a separate case.
        let batches = (0..64)
            .map(|batch| {
                let names = (0..512)
                    .map(|row| {
                        let key = (batch * 512 + row) % 20_000;
                        Value::Utf8(format!(
                            "name-{key:05}-with-a-payload-that-outgrows-the-ceiling"
                        ))
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
                physical("SELECT name, LENGTH(name) AS n, COUNT(*) FROM events GROUP BY name, n"),
                &provider,
                memory_limit,
                Collation::default(),
            )
            .expect("execution");
            let mut rows = Vec::new();
            let mut peak = 0;
            while let Some(batch) = execution.next_batch().expect("pull") {
                peak = peak.max(execution.memory().used());
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|column| column.value(row).cloned().expect("value"))
                            .collect::<Vec<_>>(),
                    );
                }
            }
            rows.sort_by(|left, right| match (&left[0], &right[0]) {
                (Value::Utf8(left), Value::Utf8(right)) => left.cmp(right),
                _ => unreachable!("group keys are text"),
            });
            (rows, execution.spill_metrics(), peak, memory_limit)
        };
        let (memory, memory_spill, _, _) = execute(64 * 1024 * 1024);
        let (spilled, spill, peak, limit) = execute(1024 * 1024);
        assert_eq!(spilled.len(), 20_000);
        assert_eq!(spilled, memory);
        assert_eq!(memory_spill.files, 0);
        assert!(spill.files > 0, "the tight execution must use spill files");
        assert!(
            peak <= limit,
            "served under the ceiling: peak {peak} of {limit}"
        );
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
                Execution::start(physical(sql), &provider, memory_limit, Collation::default())
                    .expect("execution");
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

    /// A join whose build side outgrows the ceiling drains its resident map
    /// into grace partitions, and the answer must not change. The fused
    /// join-aggregate spine reads that resident map directly, so a build side
    /// that moved out from under it would answer with silence rather than an
    /// error - the worst shape a wrong answer can take.
    ///
    /// Grouped on each side of the join in turn, because which relation the
    /// planner builds from decides which spine runs, and the answer has to
    /// hold either way.
    #[test]
    fn a_join_aggregate_still_counts_every_row_when_the_build_side_spills() {
        let batches = (0..48)
            .map(|batch| {
                let ids = (0..512)
                    .map(|row| Value::UInt64(batch * 512 + row))
                    .collect::<Vec<_>>();
                let names = ids
                    .iter()
                    .map(|id| match id {
                        Value::UInt64(id) => Value::Utf8(format!(
                            "region-{}-padded-so-the-build-side-is-heavy",
                            id % 4
                        )),
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
        let execute = |memory_limit, sql: &str| {
            let provider = StaticProvider {
                batches: Mutex::new(batches.clone()),
            };
            let mut execution =
                Execution::start(physical(sql), &provider, memory_limit, Collation::default())
                    .expect("execution");
            let mut rows = Vec::new();
            while let Some(batch) = execution.next_batch().expect("pull") {
                for row in batch.selection().selected_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|column| column.value(row).cloned().expect("set value"))
                            .collect::<Vec<_>>(),
                    );
                }
            }
            (rows, execution.spill_metrics())
        };
        let total = |rows: &[Vec<Value>]| {
            rows.iter()
                .filter_map(|row| match row.get(1) {
                    Some(Value::UInt64(count)) => Some(*count),
                    _ => None,
                })
                .sum::<u64>()
        };
        for side in ["u", "e"] {
            let sql = format!(
                "SELECT {side}.name AS region, COUNT(*) AS n \
                 FROM events e INNER JOIN events u ON e.id = u.id \
                 GROUP BY {side}.name ORDER BY region"
            );
            let (reference, _) = execute(64 * 1024 * 1024, &sql);
            assert_eq!(
                total(&reference),
                48 * 512,
                "grouped on {side}, the roomy run joins every row"
            );
            let (tight, spill) = execute(3 * 1024 * 1024, &sql);
            assert!(
                spill.files > 0,
                "grouped on {side}, the tight run must actually spill"
            );
            assert_eq!(
                tight, reference,
                "grouped on {side}, a build side that spilled must still be joined and counted"
            );
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
            let mut execution = Execution::start(plan, &provider, 64 * 1024, Collation::default())
                .expect("mixed-key execution");
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
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
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
        let mut execution = Execution::start(plan, &provider, 64 * 1024, Collation::default())
            .expect("decimal-key execution");
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
            batch
                .column(4)
                .and_then(|column| column.value(row))
                .and_then(Value::text),
            // Integer averages render at four fractional digits.
            Some("1.5000")
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 16 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 4 * 1024, Collation::default()).expect("execution");
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

        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution = Execution::start(plan, &provider, 2 * 1024, Collation::default())
            .expect("bounded top-K execution");
        // Drained rather than read in one pull. Under a 2KB ceiling the
        // producer now hands back as many rows as that affords, which for
        // 200-byte values can be one at a time; delivering both in a single
        // batch was an artifact of a fixed row count, not part of what this
        // test pins. The property is unchanged: the correct top two survive
        // while the eighteen losers are released as the input is consumed.
        let mut prefixes = Vec::new();
        while let Some(batch) = execution.next_batch().expect("pull") {
            for value in batch.column(0).expect("names").values() {
                match value {
                    Value::Utf8(value) => prefixes.push(value[..2].to_owned()),
                    _ => panic!("text result"),
                }
            }
        }
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 64 * 1024, Collation::default()).expect("execution");
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
        let mut execution =
            Execution::start(plan, &provider, 16 * 1024, Collation::default()).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");
        assert_eq!(
            batch.column(0).expect("value").values(),
            [Value::Int64(1), Value::Int64(2)]
        );
        assert!(execution.next_batch().expect("end").is_none());
    }

    #[test]
    fn plans_a_cross_join_whatever_its_size() {
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
        let plan = PhysicalPlanner::plan(logical, Collation::default())
            .expect("a cross join plans whatever its size");
        let rendered = format!("{plan:?}");
        assert!(
            rendered.contains("CrossJoin") && rendered.contains("estimated_rows: Some(4000000)"),
            "{rendered}"
        );
    }
}
