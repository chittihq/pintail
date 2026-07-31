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
    BoundJoinKind, BoundOrderKey, BoundProjection,
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
        | BoundExprKind::Aggregate(_) => {}
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryTracker {
    limit: usize,
    used: usize,
}

impl MemoryTracker {
    /// Constructs a tracker with a hard byte limit.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Returns the hard byte limit.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns bytes currently reserved by stateful operators.
    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    /// Returns bytes still available to persistent query state.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// Reserves persistent operator memory.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryLimitExceeded`] before exceeding the query
    /// limit.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), ExecError> {
        let requested = self.used.saturating_add(bytes);
        if requested > self.limit {
            return Err(ExecError::MemoryLimitExceeded {
                used: self.used,
                requested: bytes,
                limit: self.limit,
            });
        }
        self.used = requested;
        Ok(())
    }

    /// Releases persistent operator memory.
    pub fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    fn ensure_transient(&self, bytes: usize) -> Result<(), ExecError> {
        if self.used.saturating_add(bytes) > self.limit {
            return Err(ExecError::MemoryLimitExceeded {
                used: self.used,
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
        let mut memory = MemoryTracker::new(memory_limit);
        memory.reserve(subquery_bytes)?;
        let (root, _) = build_operator(plan, provider, &mut memory)?;
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
        self.root.next_batch(&mut self.memory)
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
    Limit {
        input: Box<Self>,
        skip: u64,
        take: u64,
    },
}

impl PullOperator {
    #[allow(clippy::too_many_lines)]
    fn next_batch(&mut self, memory: &mut MemoryTracker) -> Result<Option<RecordBatch>, ExecError> {
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
                    *state = Some(Box::new(build_hash_join_state(
                        right, right_key, *key_mode, memory,
                    )?));
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
    memory: &mut MemoryTracker,
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

struct HashJoinState {
    build: HashMap<JoinHashKey, Vec<Vec<Value>>>,
    batch: Option<RecordBatch>,
    batch_reserved: usize,
    row: usize,
    match_index: usize,
    left_values: Option<Vec<Value>>,
    left_key: Option<JoinHashKey>,
    left_reserved: usize,
}

impl HashJoinState {
    fn clear_left(&mut self, memory: &mut MemoryTracker) {
        self.left_values = None;
        self.left_key = None;
        self.match_index = 0;
        memory.release(self.left_reserved);
        self.left_reserved = 0;
    }

    fn clear_batch(&mut self, memory: &mut MemoryTracker) {
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
    memory: &mut MemoryTracker,
) -> Result<HashJoinState, ExecError> {
    let mut build: HashMap<JoinHashKey, Vec<Vec<Value>>> = HashMap::new();
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
            let Some(key) = normalized_join_key(right_key.evaluate(&batch, row)?, key_mode)? else {
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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

struct AggregateState {
    value: AggregateValue,
    seen: Option<HashSet<Value>>,
    /// f64 of the current Minimum/Maximum extreme when known (typed path).
    /// Guides comparisons: strict f64 inequality between correctly-rounded
    /// values transfers to the exact ordering (rounding is monotone), so only
    /// f64 ties pay the full text/value comparison. Invalidated on merge.
    extreme_number: Option<f64>,
}

enum AggregateValue {
    Count(u64),
    Sum(Option<Value>),
    Average { sum: f64, count: u64 },
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
        }
    }

    fn update(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        memory: &mut MemoryTracker,
    ) -> Result<(), ExecError> {
        self.update_with_number(aggregate, value, None, memory)
    }

    fn update_with_number(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        number: Option<f64>,
        memory: &mut MemoryTracker,
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
        memory: &mut MemoryTracker,
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

    #[allow(clippy::cast_precision_loss)]
    fn finish(self, memory: &mut MemoryTracker) -> Result<Value, ExecError> {
        Ok(match self.value {
            AggregateValue::Count(count) => Value::UInt64(count),
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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

    drop(join);
    memory.release(build_reserved);
    Ok(Some(finish_aggregate_groups(groups.into_values(), memory)?))
}

#[allow(clippy::too_many_arguments)]
fn build_local_fused_join_groups(
    batch: &RecordBatch,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    left_width: usize,
    right_group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    build: &HashMap<JoinHashKey, Vec<Vec<Value>>>,
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    let mut groups = Vec::<AggregateGroup>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let mut memory = MemoryTracker::new(usize::MAX);
    for row in batch.selection().selected_rows() {
        let Some(key) = normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? else {
            continue;
        };
        let Some(matches) = build.get(&key) else {
            continue;
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
                state.update(aggregate, value, &mut memory)?;
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

fn build_local_direct_groups(
    batch: &RecordBatch,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    let mut groups = Vec::<AggregateGroup>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let mut memory = MemoryTracker::new(usize::MAX);
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
            &mut memory,
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
    let mut memory = MemoryTracker::new(usize::MAX);
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
            &mut memory,
        )?;
    }
    Ok(groups)
}

fn finish_aggregate_groups(
    groups: impl ExactSizeIterator<Item = AggregateGroup>,
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let mut groups = Vec::<AggregateGroup>::new();
    let mut scalar_index = HashMap::<Value, usize>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let mut index_reserved = 0_usize;

    loop {
        let batch = if let Some(batch) = first_batch.take() {
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

fn update_aggregate_states(
    batch: &RecordBatch,
    row: usize,
    batch_bytes: usize,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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

fn build_sort(
    input: &mut PullOperator,
    keys: &[BoundOrderKey],
    top_k: Option<usize>,
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
    memory: &mut MemoryTracker,
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
        let mut memory = MemoryTracker::new(16 * 1024);
        let mut values = Vec::<String>::new();

        let reserved =
            reserve_vec_elements(&mut values, 1, 64, &mut memory).expect("reserve capacity");
        assert_eq!(reserved, values.capacity() * size_of::<String>(),);
        assert_eq!(memory.used(), reserved);

        values.push("x".to_owned());
        assert_eq!(
            reserve_vec_elements(&mut values, 1, 64, &mut memory).expect("reuse spare capacity"),
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
