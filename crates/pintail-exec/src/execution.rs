use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet, hash_map::Entry},
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    mem::{size_of, size_of_val},
};

const HASH_ENTRY_OVERHEAD: usize = 3 * size_of::<usize>();

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundJoinKind, BoundOrderKey, BoundProjection, BoundWindow, WindowFunction,
};
use pintail_types::{DataType, Value};
use rayon::prelude::*;

use crate::{
    BatchError, ColumnVector, DEFAULT_BATCH_ROWS, LogicalPlan, LogicalPlanner, Optimizer,
    RecordBatch, Scan,
    expression::{
        CompiledExpr, compare_mysql, compare_utf8_mysql, mysql_f64, mysql_i64, mysql_u64,
        predicate_truth,
    },
};

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
        /// Build-side key.
        right_key: BoundExpr,
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
            Self::Filter { input, .. }
            | Self::Distinct { input }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. } => input.output_fields(),
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
            } => {
                let mut fields = left.output_fields();
                if !matches!(kind, BoundJoinKind::Semi | BoundJoinKind::Anti) {
                    let mut right_fields = right.output_fields();
                    if *kind == BoundJoinKind::Left {
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
            LogicalPlan::Sort { input, keys } => Ok(PhysicalPlan::Sort {
                input: Box::new(Self::plan(*input)?),
                keys,
                top_k: None,
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
                let (left_key, right_key) = equi_join_keys(&condition, &left, &right)
                    .ok_or(ExecError::UnsupportedJoinCondition)?;
                Ok(PhysicalPlan::HashJoin {
                    left: Box::new(Self::plan(*left)?),
                    right: Box::new(Self::plan(*right)?),
                    kind,
                    left_key,
                    right_key,
                })
            }
        }
    }
}

fn plan_limit(input: LogicalPlan, offset: u64, count: u64) -> Result<PhysicalPlan, ExecError> {
    let input = match input {
        LogicalPlan::Sort { input, keys } => PhysicalPlan::Sort {
            input: Box::new(PhysicalPlanner::plan(*input)?),
            keys,
            top_k: usize::try_from(offset.saturating_add(count)).ok(),
        },
        input => PhysicalPlanner::plan(input)?,
    };
    Ok(PhysicalPlan::Limit {
        input: Box::new(input),
        offset,
        count,
    })
}

fn equi_join_keys(
    condition: &BoundExpr,
    left: &LogicalPlan,
    right: &LogicalPlan,
) -> Option<(BoundExpr, BoundExpr)> {
    let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left: first,
        right: second,
    } = &condition.kind
    else {
        return None;
    };
    hash_join_key_mode(first.data_type, second.data_type)?;
    let left_tables = logical_tables(left);
    let right_tables = logical_tables(right);
    if expression_belongs_to(first, &left_tables) && expression_belongs_to(second, &right_tables) {
        Some(((**first).clone(), (**second).clone()))
    } else if expression_belongs_to(first, &right_tables)
        && expression_belongs_to(second, &left_tables)
    {
        Some(((**second).clone(), (**first).clone()))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
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
        LogicalPlan::Scan(scan) => {
            tables.insert((scan.table.database_id, scan.table.table_id));
        }
        LogicalPlan::CrossJoin { inputs } | LogicalPlan::UnionAll { inputs } => {
            for input in inputs {
                collect_logical_tables(input, tables);
            }
        }
        LogicalPlan::Join { left, right, .. } => {
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
    #[must_use]
    fn next_batch_memory_upper_bound(&self) -> usize;

    /// Narrows the stream to rows whose value in the projected column at
    /// `position` lies within `[min, max]`. Best-effort: streams that cannot
    /// prune (already started, non-key column, type mismatch) ignore it.
    fn restrict_key_position_range(&mut self, _position: usize, _min: &Value, _max: &Value) {}
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
    /// Atomic so parallel operators can reserve from worker threads through
    /// a shared `&MemoryTracker` (experiments/RESULTS.md e02: thread-local
    /// partial state + merge is the adopted parallel-aggregation shape).
    used: std::sync::atomic::AtomicUsize,
}

impl Clone for MemoryTracker {
    fn clone(&self) -> Self {
        Self {
            limit: self.limit,
            used: std::sync::atomic::AtomicUsize::new(self.used()),
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
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: std::sync::atomic::AtomicUsize::new(0),
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

    /// Reserves persistent operator memory.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryLimitExceeded`] before exceeding the query
    /// limit.
    pub fn reserve(&self, bytes: usize) -> Result<(), ExecError> {
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
}

/// Running pull-based query execution.
pub struct Execution {
    root: PullOperator,
    memory: MemoryTracker,
    output_fields: Vec<OutputField>,
}

impl Execution {
    /// Opens every scan and prepares a physical plan for pulling.
    ///
    /// # Errors
    ///
    /// Returns an error when a scan cannot open or an expression references a
    /// column absent from its physical input.
    pub fn start(
        mut plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
    ) -> Result<Self, ExecError> {
        let mut subquery_bytes = 0;
        resolve_plan_subqueries(&mut plan, provider, memory_limit, &mut subquery_bytes)?;
        let output_fields = plan.output_fields();
        let memory = MemoryTracker::new(memory_limit);
        memory.reserve(subquery_bytes)?;
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
}

#[allow(clippy::too_many_lines)]
fn resolve_plan_subqueries(
    plan: &mut PhysicalPlan,
    provider: &dyn ScanProvider,
    memory_limit: usize,
    retained_bytes: &mut usize,
) -> Result<(), ExecError> {
    match plan {
        PhysicalPlan::Scan(scan) => {
            for predicate in &mut scan.predicates {
                resolve_expr_subqueries(predicate, provider, memory_limit, retained_bytes)?;
            }
        }
        PhysicalPlan::Derived { input, .. }
        | PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => {
            resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
        }
        PhysicalPlan::Window { input, windows, .. } => {
            for window in windows {
                if let WindowFunction::Aggregate(aggregate) = &mut window.function
                    && let Some(expr) = &mut aggregate.expr
                {
                    resolve_expr_subqueries(expr, provider, memory_limit, retained_bytes)?;
                }
                for expr in &mut window.partition_by {
                    resolve_expr_subqueries(expr, provider, memory_limit, retained_bytes)?;
                }
                for key in &mut window.order_by {
                    resolve_expr_subqueries(&mut key.expr, provider, memory_limit, retained_bytes)?;
                }
            }
            resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
        }
        PhysicalPlan::CrossJoin { inputs, .. } | PhysicalPlan::UnionAll { inputs } => {
            for input in inputs {
                resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
            }
        }
        PhysicalPlan::HashJoin {
            left,
            right,
            left_key,
            right_key,
            ..
        } => {
            resolve_plan_subqueries(left, provider, memory_limit, retained_bytes)?;
            resolve_plan_subqueries(right, provider, memory_limit, retained_bytes)?;
            resolve_expr_subqueries(left_key, provider, memory_limit, retained_bytes)?;
            resolve_expr_subqueries(right_key, provider, memory_limit, retained_bytes)?;
        }
        PhysicalPlan::Filter { input, predicate } => {
            resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
            resolve_expr_subqueries(predicate, provider, memory_limit, retained_bytes)?;
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => {
            resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
            for expression in group_by {
                resolve_expr_subqueries(expression, provider, memory_limit, retained_bytes)?;
            }
            for aggregate in aggregates {
                if let Some(expression) = &mut aggregate.expr {
                    resolve_expr_subqueries(expression, provider, memory_limit, retained_bytes)?;
                }
            }
        }
        PhysicalPlan::Project { input, expressions } => {
            resolve_plan_subqueries(input, provider, memory_limit, retained_bytes)?;
            for projection in expressions {
                resolve_expr_subqueries(
                    &mut projection.expr,
                    provider,
                    memory_limit,
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
    retained_bytes: &mut usize,
) -> Result<(), ExecError> {
    match &mut expression.kind {
        BoundExprKind::ScalarSubquery(query) => {
            let values = materialize_subquery(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
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
        BoundExprKind::InSubquery {
            expr,
            query,
            negated,
        } => {
            resolve_expr_subqueries(expr, provider, memory_limit, retained_bytes)?;
            let values = materialize_subquery(
                (**query).clone(),
                provider,
                memory_limit.saturating_sub(*retained_bytes),
            )?;
            reserve_subquery_values(&values, memory_limit, retained_bytes)?;
            let mut args = Vec::with_capacity(values.len() + 1);
            args.push((**expr).clone());
            args.extend(values.into_iter().map(|value| BoundExpr {
                data_type: value.data_type(),
                nullable: matches!(value, Value::Null),
                kind: BoundExprKind::Literal(value),
            }));
            expression.kind = BoundExprKind::Scalar {
                function: pintail_sql::ScalarFunction::InList { negated: *negated },
                args,
            };
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            resolve_expr_subqueries(expr, provider, memory_limit, retained_bytes)?;
        }
        BoundExprKind::Binary { left, right, .. } => {
            resolve_expr_subqueries(left, provider, memory_limit, retained_bytes)?;
            resolve_expr_subqueries(right, provider, memory_limit, retained_bytes)?;
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                resolve_expr_subqueries(argument, provider, memory_limit, retained_bytes)?;
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

fn materialize_subquery(
    query: pintail_sql::BoundQuery,
    provider: &dyn ScanProvider,
    memory_limit: usize,
) -> Result<Vec<Value>, ExecError> {
    let logical = Optimizer::optimize(LogicalPlanner::plan(query));
    let physical = PhysicalPlanner::plan(logical)?;
    if physical.output_fields().len() != 1 {
        return Err(ExecError::InvalidPhysicalPlan(
            "scalar or IN subquery must produce exactly one column",
        ));
    }
    let mut execution = Execution::start(physical, provider, memory_limit)?;
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
        seen: HashSet<Vec<Value>>,
    },
    Sort {
        input: Box<Self>,
        keys: Vec<BoundOrderKey>,
        column_types: Vec<DataType>,
        top_k: Option<usize>,
        state: Option<MaterializedRows>,
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
            Self::Scan { stream, .. } => stream.next_batch_memory_upper_bound(),
            Self::Filter { input, .. } => input.scan_transient_floor(),
            _ => 0,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, memory: &MemoryTracker) -> Result<Option<RecordBatch>, ExecError> {
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
                memory.ensure_transient(stream.next_batch_memory_upper_bound())?;
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
                key_mode,
                column_types,
                right_width,
                state,
            } => {
                if state.is_none() {
                    let built = build_hash_join_state(right, right_key, *key_mode, memory)?;
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
            Self::Distinct { input, seen } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                let batch_bytes = batch.estimated_bytes();
                reserve_hash_set_entries(
                    seen,
                    batch.visible_row_count(),
                    size_of::<Vec<Value>>().saturating_add(HASH_ENTRY_OVERHEAD),
                    batch_bytes,
                    memory,
                )?;
                for row in 0..batch.row_count() {
                    if !batch.selection().is_selected(row) {
                        continue;
                    }
                    let row_upper = estimated_normalized_batch_row_bytes(&batch, row)?;
                    memory.ensure_transient(batch_bytes.saturating_add(row_upper))?;
                    let key = batch
                        .columns()
                        .iter()
                        .map(|column| {
                            column
                                .value(row)
                                .cloned()
                                .map(normalized_collation_value)
                                .ok_or(ExecError::InvalidBatch(
                                    "distinct row is outside an input column",
                                ))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if seen.contains(&key) {
                        batch.selection_mut().set(row, false)?;
                    } else {
                        let row_bytes = estimated_row_payload_bytes(&key);
                        memory.ensure_transient(batch_bytes.saturating_add(row_bytes))?;
                        memory.reserve(row_bytes)?;
                        seen.insert(key);
                    }
                }
                if batch.visible_row_count() > 0 {
                    return Ok(Some(batch));
                }
            },
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
                state,
            } => {
                if state.is_none() {
                    *state = Some(build_sort(input, keys, *top_k, memory)?);
                }
                next_materialized_batch(
                    state.as_mut().expect("initialized above"),
                    column_types,
                    memory,
                )
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
        } => {
            let (left, left_columns) = build_operator(*left, provider, memory)?;
            let (right, right_columns) = build_operator(*right, provider, memory)?;
            let key_mode = hash_join_key_mode(left_key.data_type, right_key.data_type).ok_or(
                ExecError::InvalidPhysicalPlan("hash join keys have incompatible scalar types"),
            )?;
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
                    key_mode,
                    column_types,
                    right_width,
                    state: None,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Filter { input, predicate } => {
            let (input, columns) = build_operator(*input, provider, memory)?;
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
                Vec::new(),
            ))
        }
        PhysicalPlan::Project { input, expressions } => {
            let (input, columns) = build_operator(*input, provider, memory)?;
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
                Vec::new(),
            ))
        }
        PhysicalPlan::Distinct { input } => {
            let (input, columns) = build_operator(*input, provider, memory)?;
            Ok((
                PullOperator::Distinct {
                    input: Box::new(input),
                    seen: HashSet::new(),
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
        PhysicalPlan::Sort { input, keys, top_k } => {
            let column_types = input
                .output_fields()
                .into_iter()
                .map(|field| field.data_type.unwrap_or(DataType::Utf8))
                .collect::<Vec<_>>();
            if keys.iter().any(|key| key.index >= column_types.len()) {
                return Err(ExecError::InvalidPhysicalPlan(
                    "sort key is outside the projected result layout",
                ));
            }
            let (input, columns) = build_operator(*input, provider, memory)?;
            Ok((
                PullOperator::Sort {
                    input: Box::new(input),
                    keys,
                    column_types,
                    top_k,
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

/// Widest key span the dense join table will materialize (~4M slots).
const MAX_DENSE_SPAN: i128 = 1 << 22;

struct HashJoinState {
    build: HashMap<JoinHashKey, Vec<Vec<Value>>>,
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

fn build_hash_join_state(
    right: &mut PullOperator,
    right_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    memory: &MemoryTracker,
) -> Result<HashJoinState, ExecError> {
    let mut build: HashMap<JoinHashKey, Vec<Vec<Value>>> = HashMap::new();
    let mut key_bounds: Option<(Value, Value)> = None;
    let bound_order = BoundOrderKey {
        index: 0,
        ascending: true,
        nulls_first: true,
    };
    while let Some(batch) = right.next_batch(memory)? {
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
            memory.reserve(key_bytes)?;
            let bucket = build.entry(key).or_default();
            reserve_vec_elements(bucket, 1, 64, memory)?;
            memory.reserve(row_payload)?;
            let values = batch_row(&batch, row)?;
            bucket.push(values);
        }
    }
    Ok(HashJoinState {
        build,
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
fn next_hash_join_batch(
    left: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    right_width: usize,
    column_types: &[DataType],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    let mut rows = Vec::<Vec<Value>>::with_capacity(DEFAULT_BATCH_ROWS);
    let mut buffered_bytes = 0_usize;
    while rows.len() < DEFAULT_BATCH_ROWS {
        if state.left_values.is_none()
            && !prepare_hash_join_left(left, left_key, key_mode, state, memory)?
        {
            break;
        }
        let left_values = state
            .left_values
            .as_ref()
            .expect("prepared join row is present");
        let matches = state.left_key.as_ref().and_then(|key| state.build.get(key));
        let output = match kind {
            BoundJoinKind::Inner | BoundJoinKind::Left => {
                if let Some(right_values) =
                    matches.and_then(|matches| matches.get(state.match_index))
                {
                    state.match_index += 1;
                    let mut output = left_values.clone();
                    output.extend(right_values.iter().cloned());
                    Some(output)
                } else if kind == BoundJoinKind::Left && state.match_index == 0 {
                    state.match_index = 1;
                    let mut output = left_values.clone();
                    output.extend(std::iter::repeat_n(Value::Null, right_width));
                    Some(output)
                } else {
                    None
                }
            }
            BoundJoinKind::Semi if matches.is_some() => Some(left_values.clone()),
            BoundJoinKind::Anti if matches.is_none() => Some(left_values.clone()),
            BoundJoinKind::Semi | BoundJoinKind::Anti => None,
            BoundJoinKind::Cross => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "cross semantics reached hash join",
                ));
            }
        };
        let complete = match kind {
            BoundJoinKind::Inner | BoundJoinKind::Left => {
                state.match_index >= matches.map_or(1, Vec::len)
            }
            BoundJoinKind::Semi | BoundJoinKind::Anti => true,
            BoundJoinKind::Cross => unreachable!("handled above"),
        };
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

fn prepare_hash_join_left(
    left: &mut PullOperator,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
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
        state.left_key = normalized_join_key(left_key.evaluate(batch, row)?, key_mode)?;
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
    distinct: bool,
    data_type: Option<DataType>,
}

impl CompiledAggregate {
    fn compile(aggregate: &BoundAggregate, columns: &[BoundColumn]) -> Result<Self, ExecError> {
        Ok(Self {
            function: aggregate.function,
            expr: aggregate
                .expr
                .as_ref()
                .map(|expression| CompiledExpr::compile(expression, columns))
                .transpose()?,
            distinct: aggregate.distinct,
            data_type: aggregate.data_type,
        })
    }
}

struct AggregateGroup {
    values: Vec<Value>,
    states: Vec<AggregateState>,
}

#[derive(Clone)]
struct AggregateState {
    value: AggregateValue,
    seen: Option<HashSet<Value>>,
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
    },
    Average {
        sum: f64,
        count: u64,
    },
    Minimum(Option<Value>),
    Maximum(Option<Value>),
    GroupConcat(Vec<String>),
}

impl AggregateState {
    fn new(aggregate: &CompiledAggregate) -> Self {
        let value = match aggregate.function {
            AggregateFunction::Count => AggregateValue::Count(0),
            AggregateFunction::Sum => AggregateValue::Sum(None),
            AggregateFunction::Average => AggregateValue::Average { sum: 0.0, count: 0 },
            AggregateFunction::Minimum => AggregateValue::Minimum(None),
            AggregateFunction::Maximum => AggregateValue::Maximum(None),
            AggregateFunction::GroupConcat => AggregateValue::GroupConcat(Vec::new()),
        };
        Self {
            value,
            seen: aggregate.distinct.then(HashSet::new),
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

    fn update_with_number(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        number: Option<f64>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            let key = normalized_hash_key(value.clone()).unwrap_or(Value::Null);
            reserve_hash_set_entries(
                seen,
                1,
                size_of::<Value>().saturating_add(HASH_ENTRY_OVERHEAD),
                0,
                memory,
            )?;
            if seen.contains(&key) {
                return Ok(());
            }
            memory.reserve(key.heap_bytes())?;
            seen.insert(key);
        }
        match &mut self.value {
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::DecimalSum { units, scale } => {
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
            AggregateValue::GroupConcat(values) => {
                let value_bytes = scalar_string_memory_upper_bound(value);
                reserve_vec_elements(values, 1, 64, memory)?;
                memory.reserve(value_bytes)?;
                let value = aggregate_string(value)?;
                values.push(value);
            }
        }
        Ok(())
    }

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
            for value in other.seen.take().unwrap_or_default() {
                self.update(aggregate, &value, memory)?;
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
            (AggregateValue::Sum(_), AggregateValue::Sum(None))
            | (AggregateValue::Minimum(_), AggregateValue::Minimum(None))
            | (AggregateValue::Maximum(_), AggregateValue::Maximum(None)) => {}
            (
                AggregateValue::DecimalSum { units: left, .. },
                AggregateValue::DecimalSum { units: right, .. },
            ) => {
                *left = left.checked_add(right).ok_or(ExecError::NumericOverflow)?;
            }
            (value @ AggregateValue::Sum(None), AggregateValue::DecimalSum { units, scale }) => {
                *value = AggregateValue::DecimalSum { units, scale };
            }
            (AggregateValue::DecimalSum { units, scale }, AggregateValue::Sum(Some(right))) => {
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
    fn update_decimal_sum_units(&mut self, units: i128, scale: u8) -> Result<(), ExecError> {
        match &mut self.value {
            AggregateValue::DecimalSum {
                units: total,
                scale: existing,
            } if *existing == scale => {
                *total = total.checked_add(units).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            value @ AggregateValue::Sum(None) => {
                *value = AggregateValue::DecimalSum { units, scale };
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "decimal unit sum applied to an incompatible aggregate state",
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

    #[allow(clippy::cast_precision_loss)]
    fn finish(self, memory: &MemoryTracker) -> Result<Value, ExecError> {
        Ok(match self.value {
            AggregateValue::Count(count) => Value::UInt64(count),
            AggregateValue::DecimalSum { units, scale } => {
                Value::Utf8(pintail_types::format_decimal_scaled(units, scale))
            }
            AggregateValue::Sum(value)
            | AggregateValue::Minimum(value)
            | AggregateValue::Maximum(value) => value.unwrap_or(Value::Null),
            AggregateValue::Average { sum: _, count: 0 } => Value::Null,
            AggregateValue::Average { sum, count } => Value::float64(sum / count as f64),
            AggregateValue::GroupConcat(values) if values.is_empty() => Value::Null,
            AggregateValue::GroupConcat(values) => {
                let joined_bytes = values
                    .iter()
                    .map(String::len)
                    .fold(values.len().saturating_sub(1), usize::saturating_add);
                memory.reserve(joined_bytes)?;
                Value::Utf8(values.join(","))
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
fn build_hash_aggregate(
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
        if aggregates
            .iter()
            .all(|aggregate| aggregate.function != AggregateFunction::GroupConcat)
        {
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
    if let Some([column]) = direct_columns
        && first_batch.column(*column).is_some_and(|values| {
            matches!(
                values.data_type().storage_type(),
                DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
            )
        })
    {
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
    let mut first_batch = Some(first_batch);
    loop {
        let mut batches = Vec::with_capacity(8);
        let mut batch_reserved = 0_usize;
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
            group_by
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
                direct_columns.map_or_else(
                    || build_local_expression_groups(batch, group_by, aggregates),
                    |columns| build_local_direct_groups(batch, columns, aggregates),
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
    finish_aggregate_groups(groups.into_values(), memory)
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
        key_mode,
        column_types,
        right_width,
        state,
    } = input
    else {
        return Ok(None);
    };
    let left_width = column_types.len().saturating_sub(*right_width);
    if *kind != BoundJoinKind::Inner
        || state.is_some()
        || *right_width > column_types.len()
        || group_columns
            .iter()
            .any(|column| *column < left_width || *column >= column_types.len())
        || aggregates.iter().any(|aggregate| {
            aggregate.distinct || aggregate.function == AggregateFunction::GroupConcat
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
    let join = build_hash_join_state(right, right_key, *key_mode, memory)?;
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
                    &right_group_columns,
                    aggregates,
                    &join.build,
                    dense.as_ref(),
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
    right_group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    build: &HashMap<JoinHashKey, Vec<Vec<Value>>>,
    dense: Option<&DenseJoinTable<'_>>,
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    let mut groups = Vec::<AggregateGroup>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
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
        for right_values in matches {
            let raw_hash = joined_right_group_hash(right_values, right_group_columns)?;
            let existing = raw_index
                .get(&raw_hash)
                .copied()
                .filter(|index| {
                    joined_right_group_matches(
                        &groups[*index].values,
                        right_values,
                        right_group_columns,
                        true,
                    )
                })
                .or_else(|| {
                    groups.iter().position(|group| {
                        joined_right_group_matches(
                            &group.values,
                            right_values,
                            right_group_columns,
                            false,
                        )
                    })
                });
            let group_index = if let Some(index) = existing {
                index
            } else {
                let values = right_group_columns
                    .iter()
                    .map(|column| {
                        right_values
                            .get(*column)
                            .cloned()
                            .ok_or(ExecError::InvalidPhysicalPlan(
                                "join aggregate group is outside the build-side layout",
                            ))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let index = groups.len();
                groups.push(AggregateGroup {
                    values,
                    states: aggregates.iter().map(AggregateState::new).collect(),
                });
                index
            };
            raw_index.entry(raw_hash).or_insert(group_index);
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

fn joined_right_group_hash(values: &[Value], columns: &[usize]) -> Result<u64, ExecError> {
    let mut hasher = DefaultHasher::new();
    for column in columns {
        values
            .get(*column)
            .ok_or(ExecError::InvalidPhysicalPlan(
                "join aggregate group is outside the build-side layout",
            ))?
            .hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn joined_right_group_matches(
    grouped: &[Value],
    values: &[Value],
    columns: &[usize],
    exact: bool,
) -> bool {
    grouped.iter().zip(columns).all(|(left, column)| {
        values.get(*column).is_some_and(|right| {
            if exact {
                return left == right;
            }
            match (left, right) {
                (Value::Utf8(left), Value::Utf8(right)) => {
                    if left.is_ascii() && right.is_ascii() {
                        left.eq_ignore_ascii_case(right)
                    } else {
                        compare_utf8_mysql(left, right) == Ordering::Equal
                    }
                }
                _ => left == right,
            }
        })
    })
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
            AggregateFunction::Sum | AggregateFunction::Average
                if aggregate_uses_float(aggregate)
                    || aggregate.function == AggregateFunction::Average => {}
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
    if let [column] = *group_columns {
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
        let group_type = head.column(column).map(ColumnVector::data_type);
        let int_typed = group_type.is_some_and(|data_type| {
            matches!(
                data_type.storage_type(),
                DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
            )
        });
        let lanes = int_typed
            .then(|| two_pass_lanes(aggregates, &head))
            .flatten();
        if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
            eprintln!(
                "[agg] direct path: int_typed={int_typed} lanes={:?} group_type={group_type:?}",
                lanes.as_ref().map(std::vec::Vec::len)
            );
        }
        if let (Some(lanes), Some(group_type)) = (lanes, group_type) {
            // Streaming scatter (phase-0 profile, 2026-08-02): retaining
            // RecordBatches cost ~118 bytes/row and forced the sequential
            // Value-hashmap fallback on real 20M-row inputs. Scattering
            // (key bits, lane bits) as batches arrive costs the exact
            // 8*(1+lanes)+1 bytes/row and never falls back.
            return build_streaming_two_pass_aggregate(
                input, head, column, group_type, &lanes, aggregates, memory,
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
                return None;
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
                        // Integer sums stay on the exact integer branch.
                        DataType::Int64 | DataType::UInt64 => Some(TwoPassLane::Int {
                            column,
                            data_type: storage,
                        }),
                        DataType::Float64 => Some(TwoPassLane::Float { column }),
                        _ => matches!(batch.column(column)?.data_type(), DataType::Decimal { .. })
                            .then_some(TwoPassLane::Float { column }),
                    }
                }
                AggregateFunction::Minimum | AggregateFunction::Maximum => matches!(
                    storage,
                    DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Boolean
                )
                .then_some(TwoPassLane::Exact {
                    column,
                    data_type: storage,
                }),
                AggregateFunction::GroupConcat => None,
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
    group_column: usize,
    group_type: DataType,
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
    let mut maps: Vec<HashMap<(u64, bool), Vec<AggregateState>>> =
        (0..partitions).map(|_| HashMap::new()).collect();
    let mut bucket_reserved = 0_usize;
    let mut group_reserved = 0_usize;
    let mut flushes = 0_u32;

    let mut batch = Some(first);
    loop {
        let Some(current) = batch.take() else {
            break;
        };
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
        two_pass_scatter_batch(&current, group_column, lanes, partitions, &mut buckets)?;
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
    two_pass_flush(
        &mut buckets,
        &mut maps,
        lanes,
        aggregates,
        memory,
        &mut group_reserved,
    )?;
    memory.release(bucket_reserved);
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
            let mut rows = Vec::with_capacity(map.len());
            let mut payload = 0_usize;
            for ((bits, null), states) in map {
                let mut row = Vec::with_capacity(1 + states.len());
                row.push(two_pass_key_value(bits, null, group_type));
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
    let lane_count = lanes.len();
    let group_values = batch.column(group_column).ok_or(ExecError::InvalidBatch(
        "grouping column is outside the input batch",
    ))?;
    for row in batch.selection().selected_rows() {
        let value = group_values.value(row).ok_or(ExecError::InvalidBatch(
            "grouping row is outside the input batch",
        ))?;
        let (key_bits, key_null) = two_pass_key_bits(value)
            .ok_or(ExecError::InvalidBatch("two-pass key is not scalar"))?;
        let mut mask = u8::from(key_null) << 7;
        let bucket = &mut buckets[usize::try_from(
            crate::batch::mix64(key_bits ^ u64::from(key_null)) % partitions as u64,
        )
        .expect("partition index fits usize")];
        let lane_base = bucket.lanes.len();
        bucket.lanes.resize(lane_base + lane_count, 0);
        for (lane_index, lane) in lanes.iter().enumerate() {
            match lane {
                TwoPassLane::CountStar => {}
                TwoPassLane::Float { column } => {
                    let number = batch
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
                        });
                    match number {
                        Some(number) => {
                            bucket.lanes[lane_base + lane_index] = number.to_bits();
                        }
                        None => mask |= 1 << lane_index,
                    }
                }
                TwoPassLane::Int { column, .. } | TwoPassLane::Exact { column, .. } => {
                    match batch.column(*column).and_then(|column| column.value(row)) {
                        Some(Value::Int64(value)) => {
                            bucket.lanes[lane_base + lane_index] =
                                u64::from_ne_bytes(value.to_ne_bytes());
                        }
                        Some(Value::UInt64(value)) => {
                            bucket.lanes[lane_base + lane_index] = *value;
                        }
                        Some(Value::Float64(value)) => {
                            bucket.lanes[lane_base + lane_index] = value.get().to_bits();
                        }
                        Some(Value::Boolean(value)) => {
                            bucket.lanes[lane_base + lane_index] = u64::from(*value);
                        }
                        _ => mask |= 1 << lane_index,
                    }
                }
            }
        }
        bucket.keys.push(key_bits);
        bucket.masks.push(mask);
    }
    Ok(())
}

/// Pass 2: fold every partition\'s scattered rows into its typed group
/// map, in parallel, then clear the buckets (keeping capacity).
fn two_pass_flush(
    buckets: &mut [TwoPassBucket],
    maps: &mut [HashMap<(u64, bool), Vec<AggregateState>>],
    lanes: &[TwoPassLane],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    group_reserved: &mut usize,
) -> Result<(), ExecError> {
    let lane_count = lanes.len();
    let per_group_bytes = size_of::<(u64, bool)>()
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(32);
    let added = maps
        .par_iter_mut()
        .zip(buckets.par_iter_mut())
        .map(|(map, bucket)| -> Result<usize, ExecError> {
            let before = map.len();
            for (row, (key, mask)) in bucket.keys.iter().zip(&bucket.masks).enumerate() {
                let key_null = mask & (1 << 7) != 0;
                let states = map
                    .entry((*key, key_null))
                    .or_insert_with(|| aggregates.iter().map(AggregateState::new).collect());
                for (lane_index, (lane, aggregate)) in lanes.iter().zip(aggregates).enumerate() {
                    if mask & (1 << lane_index) != 0 {
                        continue;
                    }
                    let bits = bucket.lanes[row * lane_count + lane_index];
                    match lane {
                        TwoPassLane::CountStar => {
                            states[lane_index].update(aggregate, &Value::UInt64(1), memory)?;
                        }
                        TwoPassLane::Float { .. } => {
                            let number = f64::from_bits(bits);
                            states[lane_index].update_with_number(
                                aggregate,
                                &Value::float64(number),
                                Some(number),
                                memory,
                            )?;
                        }
                        TwoPassLane::Int { data_type, .. } => {
                            let value = two_pass_key_value(bits, false, *data_type);
                            // number=None keeps integer sums on the
                            // exact integer branch, as sequential does.
                            states[lane_index]
                                .update_with_number(aggregate, &value, None, memory)?;
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
                            states[lane_index]
                                .update_with_number(aggregate, &value, number, memory)?;
                        }
                    }
                }
            }
            bucket.keys.clear();
            bucket.masks.clear();
            bucket.lanes.clear();
            let new_groups = map.len().saturating_sub(before);
            let bytes = new_groups.saturating_mul(per_group_bytes);
            memory.reserve(bytes)?;
            Ok(bytes)
        })
        .collect::<Result<Vec<_>, _>>()?;
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
            && aggregate.function != AggregateFunction::GroupConcat;
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
        match &aggregate.expr {
            None => state.update(aggregate, &Value::Boolean(true), memory)?,
            Some(expression) => {
                if let Some(column) = expression.column_index() {
                    // Typed-first: numeric aggregation over packed units
                    // never touches the column's Value cells or lazy text
                    // (2026-08-02 phase-0 profile: whole-column text
                    // forcing dominated the string-keyed paths).
                    if !aggregate.distinct
                        && let Some((typed, validity)) =
                            batch.column(column).and_then(super::ColumnVector::typed)
                    {
                        if !validity.is_valid(row) {
                            continue; // NULL joins no aggregate
                        }
                        match aggregate.function {
                            AggregateFunction::Count => {
                                state.update_with_number(
                                    aggregate,
                                    &Value::Boolean(true),
                                    None,
                                    memory,
                                )?;
                                continue;
                            }
                            AggregateFunction::Sum if !aggregate_uses_float(aggregate) => {
                                if let (Some(units), Some(scale)) =
                                    (typed.units_at(row), typed.decimal_scale())
                                {
                                    state.update_decimal_sum_units(units, scale)?;
                                    continue;
                                }
                            }
                            AggregateFunction::Sum | AggregateFunction::Average => {
                                if let Some(number) = typed.number_at(row) {
                                    state.update_with_number(
                                        aggregate,
                                        &Value::Boolean(true),
                                        Some(number),
                                        memory,
                                    )?;
                                    continue;
                                }
                            }
                            AggregateFunction::Minimum | AggregateFunction::Maximum => {
                                if let Some(units) = typed.units_at(row) {
                                    state.update_extreme_units(
                                        aggregate,
                                        units,
                                        || typed.format_unit(row),
                                        memory,
                                    )?;
                                    continue;
                                }
                            }
                            AggregateFunction::GroupConcat => {}
                        }
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
    aggregate.function == AggregateFunction::Average
        || (aggregate.function == AggregateFunction::Sum
            && aggregate.data_type == Some(DataType::Float64))
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

fn compare_decimal_text(left: &str, right: &str) -> Result<Ordering, ExecError> {
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum JoinHashKey {
    NegativeInteger(i64),
    NonNegativeInteger(u64),
    MysqlNumber(pintail_types::Float64),
    Scalar(Value),
}

impl JoinHashKey {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Scalar(value) => value.heap_bytes(),
            Self::NegativeInteger(_) | Self::NonNegativeInteger(_) | Self::MysqlNumber(_) => 0,
        }
    }
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
        Value::Utf8(value) => Value::Utf8(value.to_lowercase()),
        value => value,
    }
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

/// One window computation compiled against its input's column layout.
struct CompiledWindow {
    function: CompiledWindowFunction,
    partition: Vec<CompiledExpr>,
    /// Order keys with `(ascending, nulls_first)`.
    order: Vec<(CompiledExpr, bool, bool)>,
}

enum CompiledWindowFunction {
    RowNumber,
    Rank,
    DenseRank,
    /// The aggregate plus its compiled argument; `COUNT(*)` compiles a
    /// constant 1 so every row counts.
    Aggregate(CompiledAggregate, CompiledExpr),
}

impl CompiledWindow {
    fn compile(window: &BoundWindow, columns: &[BoundColumn]) -> Result<Self, ExecError> {
        let function = match &window.function {
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
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
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
                for (expr, _, _) in &window.order {
                    row_keys.push(expr.evaluate(&batch, row)?);
                }
                if let CompiledWindowFunction::Aggregate(_, argument) = &window.function {
                    row_keys.push(argument.evaluate(&batch, row)?);
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
    let order_key = |ascending: bool, nulls_first: bool| BoundOrderKey {
        index: 0,
        ascending,
        nulls_first,
    };
    let compare_rows = |left: usize, right: usize| {
        let left_keys = &keys[left];
        let right_keys = &keys[right];
        for position in 0..partition_len {
            let ordering = compare_sort_values(
                &left_keys[position],
                &right_keys[position],
                order_key(true, true),
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        for (position, (_, ascending, nulls_first)) in window.order.iter().enumerate() {
            let ordering = compare_sort_values(
                &left_keys[partition_len + position],
                &right_keys[partition_len + position],
                order_key(*ascending, *nulls_first),
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
                order_key(true, true),
            ) == Ordering::Equal
        })
    };
    let same_peers = |left: usize, right: usize| {
        window.order.iter().enumerate().all(|(position, key)| {
            compare_sort_values(
                &keys[left][partition_len + position],
                &keys[right][partition_len + position],
                order_key(key.1, key.2),
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
            CompiledWindowFunction::Aggregate(aggregate, _) => {
                let argument_position = partition_len + window.order.len();
                if window.order.is_empty() {
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

fn build_sort(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    top_k: Option<usize>,
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let compare = |left: &Vec<Value>, right: &Vec<Value>| compare_sort_rows(left, right, keys);
    let mut rows = if let Some(top_k) = top_k {
        materialize_top_k(input, top_k, keys, compare, memory)?
    } else {
        materialize(input, memory)?
    };
    rows.sort_by(compare);
    Ok(MaterializedRows { rows, position: 0 })
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
            order_direction(compare_utf8_mysql(left, right), key.ascending)
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

fn reserve_hash_map_entries<K, V>(
    values: &mut HashMap<K, V>,
    additional: usize,
    entry_bytes: usize,
    transient_bytes: usize,
    memory: &MemoryTracker,
) -> Result<usize, ExecError>
where
    K: Eq + Hash,
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

fn reserve_hash_set_entries<T>(
    values: &mut HashSet<T>,
    additional: usize,
    entry_bytes: usize,
    transient_bytes: usize,
    memory: &MemoryTracker,
) -> Result<usize, ExecError>
where
    T: Eq + Hash,
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

fn estimated_normalized_batch_row_bytes(
    batch: &RecordBatch,
    row: usize,
) -> Result<usize, ExecError> {
    let heap_bytes = batch
        .columns()
        .iter()
        .try_fold(0_usize, |heap_bytes, column| {
            let value = column
                .value(row)
                .ok_or(ExecError::InvalidBatch("row is outside an input column"))?;
            let value_bytes = match value {
                Value::Utf8(value) => value.len().saturating_mul(12),
                value => value.heap_bytes(),
            };
            Ok::<_, ExecError>(heap_bytes.saturating_add(value_bytes))
        })?;
    Ok(size_of::<Vec<Value>>()
        .saturating_add(batch.columns().len().saturating_mul(size_of::<Value>()))
        .saturating_add(heap_bytes)
        .saturating_add(HASH_ENTRY_OVERHEAD)
        .saturating_add(2 * size_of::<usize>()))
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
    use std::{collections::VecDeque, mem::size_of, sync::Mutex};

    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, parse_statement};
    use pintail_types::{Column, DataType, TableSchema, Value};

    use crate::{
        BatchStream, ColumnVector, ExecError, Execution, LogicalPlanner, MemoryTracker, Optimizer,
        PhysicalPlanner, RecordBatch, Scan, ScanProvider,
    };

    use super::{compare_decimal_text, reserve_vec_elements};

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

        fn next_batch_memory_upper_bound(&self) -> usize {
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
                Value::float64(12.35),
                Value::float64(100.0),
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
        assert_eq!(
            batch.selection().selected_rows().collect::<Vec<_>>(),
            [0, 2]
        );
        assert!(execution.next_batch().expect("end").is_none());
        assert!(execution.memory().used() > 0);
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
            Some(&Value::float64(1.5))
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
