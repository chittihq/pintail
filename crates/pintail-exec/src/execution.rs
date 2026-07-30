use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    mem::{size_of, size_of_val},
};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundJoinKind, BoundOrderKey, BoundProjection,
};
use pintail_types::{DataType, Value};

use crate::{
    BatchError, ColumnVector, DEFAULT_BATCH_ROWS, LogicalPlan, RecordBatch, Scan,
    expression::{CompiledExpr, mysql_f64, mysql_i64, mysql_u64, predicate_truth},
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
    if first.data_type != second.data_type {
        return None;
    }
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
        BoundExprKind::Literal(_) | BoundExprKind::GroupKey(_) | BoundExprKind::Aggregate(_) => {}
    }
}

/// Pull-based batch source opened for one physical scan.
pub trait BatchStream: Send {
    /// Produces the next batch, or `None` at end of stream.
    ///
    /// # Errors
    ///
    /// Returns a source-specific execution error.
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError>;
}

/// Opens storage scans for physical execution.
pub trait ScanProvider {
    /// Opens one scan whose batches contain exactly the scan's projected
    /// columns in the requested order.
    ///
    /// # Errors
    ///
    /// Returns a source-specific execution error.
    fn open_scan(&self, scan: &Scan) -> Result<Box<dyn BatchStream>, ExecError>;
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
        plan: PhysicalPlan,
        provider: &dyn ScanProvider,
        memory_limit: usize,
    ) -> Result<Self, ExecError> {
        let output_fields = plan.output_fields();
        let (root, _) = build_operator(plan, provider)?;
        Ok(Self {
            root,
            memory: MemoryTracker::new(memory_limit),
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
        column_types: Vec<DataType>,
        right_width: usize,
        state: Option<MaterializedRows>,
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
                let Some(batch) = stream.next_batch()? else {
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
                column_types,
                right_width,
                state,
            } => {
                if state.is_none() {
                    *state = Some(build_hash_join(
                        left,
                        right,
                        *kind,
                        left_key,
                        right_key,
                        *right_width,
                        memory,
                    )?);
                }
                next_materialized_batch(
                    state.as_mut().expect("initialized above"),
                    column_types,
                    memory,
                )
            }
            Self::Filter { input, predicate } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                let selected = batch.selection().selected_rows().collect::<Vec<_>>();
                for row in selected {
                    let keep = predicate_truth(&predicate.evaluate(&batch, row)?)?;
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
                memory.ensure_transient(
                    batch
                        .estimated_bytes()
                        .saturating_add(output.estimated_bytes()),
                )?;
                Ok(Some(output))
            }
            Self::Distinct { input, seen } => loop {
                let Some(mut batch) = input.next_batch(memory)? else {
                    return Ok(None);
                };
                let selected = batch.selection().selected_rows().collect::<Vec<_>>();
                for row in selected {
                    let key = batch
                        .columns()
                        .iter()
                        .map(|column| {
                            column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                                "distinct row is outside an input column",
                            ))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if seen.contains(&key) {
                        batch.selection_mut().set(row, false)?;
                    } else {
                        let row_bytes = estimated_row_bytes(&key);
                        memory
                            .ensure_transient(batch.estimated_bytes().saturating_add(row_bytes))?;
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
            let stream = provider.open_scan(&scan)?;
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
        PhysicalPlan::CrossJoin {
            inputs,
            estimated_rows: _,
        } => {
            let built = inputs
                .into_iter()
                .map(|input| build_operator(input, provider))
                .collect::<Result<Vec<_>, _>>()?;
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
            let built = inputs
                .into_iter()
                .map(|input| build_operator(input, provider))
                .collect::<Result<Vec<_>, _>>()?;
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
            let (left, left_columns) = build_operator(*left, provider)?;
            let (right, right_columns) = build_operator(*right, provider)?;
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
                    column_types,
                    right_width,
                    state: None,
                },
                output_columns,
            ))
        }
        PhysicalPlan::Filter { input, predicate } => {
            let (input, columns) = build_operator(*input, provider)?;
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
            let (input, columns) = build_operator(*input, provider)?;
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
            let (input, columns) = build_operator(*input, provider)?;
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
            let (input, columns) = build_operator(*input, provider)?;
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
            let (input, columns) = build_operator(*input, provider)?;
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
            let (input, columns) = build_operator(*input, provider)?;
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

fn build_hash_join(
    left: &mut PullOperator,
    right: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    right_key: &CompiledExpr,
    right_width: usize,
    memory: &mut MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let mut build: HashMap<Value, Vec<Vec<Value>>> = HashMap::new();
    while let Some(batch) = right.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            let Some(key) = normalized_hash_key(right_key.evaluate(&batch, row)?) else {
                continue;
            };
            let values = batch_row(&batch, row)?;
            let bytes = estimated_row_bytes(&values).saturating_add(key.heap_bytes());
            memory.ensure_transient(batch.estimated_bytes().saturating_add(bytes))?;
            memory.reserve(bytes)?;
            build.entry(key).or_default().push(values);
        }
    }

    let mut rows = Vec::new();
    while let Some(batch) = left.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            let left_values = batch_row(&batch, row)?;
            let key = normalized_hash_key(left_key.evaluate(&batch, row)?);
            let matches = key.as_ref().and_then(|key| build.get(key));
            match kind {
                BoundJoinKind::Inner => {
                    if let Some(matches) = matches {
                        for right_values in matches {
                            let mut output = left_values.clone();
                            output.extend(right_values.iter().cloned());
                            reserve_output_row(&output, &batch, memory)?;
                            rows.push(output);
                        }
                    }
                }
                BoundJoinKind::Left => {
                    if let Some(matches) = matches {
                        for right_values in matches {
                            let mut output = left_values.clone();
                            output.extend(right_values.iter().cloned());
                            reserve_output_row(&output, &batch, memory)?;
                            rows.push(output);
                        }
                    } else {
                        let mut output = left_values;
                        output.extend(std::iter::repeat_n(Value::Null, right_width));
                        reserve_output_row(&output, &batch, memory)?;
                        rows.push(output);
                    }
                }
                BoundJoinKind::Semi if matches.is_some() => {
                    reserve_output_row(&left_values, &batch, memory)?;
                    rows.push(left_values);
                }
                BoundJoinKind::Anti if matches.is_none() => {
                    reserve_output_row(&left_values, &batch, memory)?;
                    rows.push(left_values);
                }
                BoundJoinKind::Semi | BoundJoinKind::Anti => {}
                BoundJoinKind::Cross => {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "cross semantics reached hash join",
                    ));
                }
            }
        }
    }
    Ok(MaterializedRows { rows, position: 0 })
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
        }
    }

    fn update(
        &mut self,
        aggregate: &CompiledAggregate,
        value: Value,
        memory: &mut MemoryTracker,
    ) -> Result<(), ExecError> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            let key = normalized_hash_key(value.clone()).unwrap_or(Value::Null);
            if !seen.insert(key.clone()) {
                return Ok(());
            }
            memory.reserve(size_of::<Value>().saturating_add(key.heap_bytes()))?;
        }
        match &mut self.value {
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Sum(sum) => {
                *sum = Some(add_aggregate_value(
                    sum.take(),
                    &value,
                    aggregate.data_type,
                )?);
            }
            AggregateValue::Average { sum, count } => {
                *sum += mysql_f64(&value)?;
                if !sum.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Minimum(minimum) => {
                if minimum.as_ref().is_none_or(|current| value < *current) {
                    *minimum = Some(value);
                }
            }
            AggregateValue::Maximum(maximum) => {
                if maximum.as_ref().is_none_or(|current| value > *current) {
                    *maximum = Some(value);
                }
            }
            AggregateValue::GroupConcat(values) => {
                let value = aggregate_string(&value)?;
                memory.reserve(value.len())?;
                values.push(value);
            }
        }
        Ok(())
    }

    #[allow(clippy::cast_precision_loss)]
    fn finish(self) -> Value {
        match self.value {
            AggregateValue::Count(count) => Value::UInt64(count),
            AggregateValue::Sum(value)
            | AggregateValue::Minimum(value)
            | AggregateValue::Maximum(value) => value.unwrap_or(Value::Null),
            AggregateValue::Average { sum: _, count: 0 } => Value::Null,
            AggregateValue::Average { sum, count } => Value::float64(sum / count as f64),
            AggregateValue::GroupConcat(values) if values.is_empty() => Value::Null,
            AggregateValue::GroupConcat(values) => Value::Utf8(values.join(",")),
        }
    }
}

fn build_hash_aggregate(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    memory: &mut MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    let mut groups = BTreeMap::<Vec<Value>, AggregateGroup>::new();
    if group_by.is_empty() {
        groups.insert(
            Vec::new(),
            AggregateGroup {
                values: Vec::new(),
                states: aggregates.iter().map(AggregateState::new).collect(),
            },
        );
        memory.reserve(size_of::<AggregateGroup>())?;
    }

    while let Some(batch) = input.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            let values = group_by
                .iter()
                .map(|expression| expression.evaluate(&batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            let key = values
                .iter()
                .cloned()
                .map(|value| normalized_hash_key(value).unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            if !groups.contains_key(&key) {
                let bytes = estimated_row_bytes(&values).saturating_add(estimated_row_bytes(&key));
                memory.ensure_transient(batch.estimated_bytes().saturating_add(bytes))?;
                memory.reserve(bytes.saturating_add(size_of::<AggregateGroup>()))?;
                groups.insert(
                    key.clone(),
                    AggregateGroup {
                        values,
                        states: aggregates.iter().map(AggregateState::new).collect(),
                    },
                );
            }
            let group = groups.get_mut(&key).expect("inserted above");
            for (aggregate, state) in aggregates.iter().zip(&mut group.states) {
                let value = aggregate
                    .expr
                    .as_ref()
                    .map_or(Ok(Value::Boolean(true)), |expression| {
                        expression.evaluate(&batch, row)
                    })?;
                state.update(aggregate, value, memory)?;
            }
        }
    }

    let mut rows = Vec::with_capacity(groups.len());
    for (_, group) in groups {
        let mut row = group.values;
        row.extend(group.states.into_iter().map(AggregateState::finish));
        memory.reserve(estimated_row_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
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
    match value {
        Value::Null => None,
        Value::Utf8(value) => Some(Value::Utf8(value.to_lowercase())),
        value => Some(value),
    }
}

fn reserve_output_row(
    row: &[Value],
    batch: &RecordBatch,
    memory: &mut MemoryTracker,
) -> Result<(), ExecError> {
    let bytes = estimated_row_bytes(row);
    memory.ensure_transient(batch.estimated_bytes().saturating_add(bytes))?;
    memory.reserve(bytes)
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
    state.position = end;
    let columns = rows_to_columns(rows, column_types)?;
    let batch = RecordBatch::new(rows.len(), columns)?;
    memory.ensure_transient(batch.estimated_bytes())?;
    Ok(Some(batch))
}

fn materialize(
    input: &mut PullOperator,
    memory: &mut MemoryTracker,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut rows = Vec::new();
    while let Some(batch) = input.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or(ExecError::InvalidBatch(
                        "cross-join row is outside an input column",
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let row_bytes = estimated_row_bytes(&values);
            memory.ensure_transient(batch.estimated_bytes().saturating_add(row_bytes))?;
            memory.reserve(row_bytes)?;
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
    let mut rows = materialize(input, memory)?;
    let compare = |left: &Vec<Value>, right: &Vec<Value>| compare_sort_rows(left, right, keys);
    if let Some(top_k) = top_k {
        if top_k == 0 {
            rows.clear();
        } else if top_k < rows.len() {
            rows.select_nth_unstable_by(top_k, compare);
            rows.truncate(top_k);
        }
    }
    rows.sort_by(compare);
    Ok(MaterializedRows { rows, position: 0 })
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
        (Value::Utf8(left), Value::Utf8(right)) => order_direction(
            left.to_lowercase().cmp(&right.to_lowercase()),
            key.ascending,
        ),
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

fn estimated_row_bytes(row: &[Value]) -> usize {
    size_of::<Vec<Value>>()
        + size_of_val(row)
        + row.iter().map(Value::heap_bytes).sum::<usize>()
        + 2 * size_of::<usize>()
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
        let mut rows = Vec::with_capacity(maximum);
        while !self.done && rows.len() < maximum {
            let mut row = Vec::new();
            for (input, index) in self.inputs.iter().zip(&self.indexes) {
                row.extend(input[*index].iter().cloned());
            }
            rows.push(row);
            self.advance();
        }
        rows
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
    use std::{collections::VecDeque, sync::Mutex};

    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, parse_statement};
    use pintail_types::{Column, DataType, TableSchema, Value};

    use crate::{
        BatchStream, ColumnVector, ExecError, Execution, LogicalPlanner, Optimizer,
        PhysicalPlanner, RecordBatch, Scan, ScanProvider,
    };

    struct StaticProvider {
        batches: Mutex<Vec<RecordBatch>>,
    }

    impl ScanProvider for StaticProvider {
        fn open_scan(&self, _scan: &Scan) -> Result<Box<dyn BatchStream>, ExecError> {
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
        fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
            Ok(self.batches.pop_front())
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
             CAST('12x' AS SIGNED)",
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
                Value::Utf8("alpha".to_owned()),
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
            "SELECT name, COUNT(*) AS rows, SUM(DISTINCT id) AS total \
             FROM events GROUP BY name HAVING COUNT(*) > 1",
        );
        let mut execution = Execution::start(plan, &provider, 64 * 1024).expect("execution");
        let batch = execution.next_batch().expect("pull").expect("result batch");

        assert_eq!(batch.visible_row_count(), 1);
        assert_eq!(
            batch.column(0).and_then(|column| column.value(0)),
            Some(&Value::Utf8("alpha".to_owned()))
        );
        assert_eq!(
            batch.column(1).and_then(|column| column.value(0)),
            Some(&Value::UInt64(2))
        );
        assert_eq!(
            batch.column(2).and_then(|column| column.value(0)),
            Some(&Value::UInt64(3))
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
