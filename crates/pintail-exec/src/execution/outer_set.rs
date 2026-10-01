//! Correlated scalar aggregate subqueries answered a batch of outer rows
//! at a time.
//!
//! The dependent path answers `(SELECT SUM(..) FROM t JOIN u .. WHERE t.k =
//! o.k ..)` by planning and executing it once per outer row. The memo
//! shares answers between rows carrying the same outer values, and the hash
//! index answers a single-table lookup, but an aggregate over a join whose
//! outer values are mostly distinct still pays one plan and one execution
//! per row - around ten milliseconds each over a few million inner rows,
//! and twice that when HAVING and the select list both hold the subquery.
//!
//! The binder gives such a subquery a second form
//! (`pintail_sql::OuterSetQuery`): the same query joined to relations that
//! carry the outer rows' values and grouped by which outer row each joined
//! row belongs to. This module runs it. On the first row of a batch whose
//! outer values it has not answered, it collects the batch's distinct outer
//! tuples, serves them to the form's virtual relations from memory,
//! executes the form once, and keeps the value of every tuple. Each row is
//! then a hash lookup.
//!
//! What it must never change, and how each is kept:
//!
//! - **The value over no rows.** A tuple the grouped result has no row for
//!   matched nothing. What the subquery answers then - 0 for `COUNT`, NULL
//!   for `SUM`, whatever its expression makes of those, NULL for an
//!   `ORDER BY .. LIMIT 1` lookup - is taken from the subquery itself,
//!   executed once over an input no row passes.
//! - **Tuple identity.** Tuples are distinct by value, bytewise, NULL
//!   included, exactly as the memo keys them. Two outer rows share an
//!   answer only when the per-row path would have substituted the same
//!   literals; what equals what inside the query is decided by the query.
//! - **Short-circuit and errors.** A batch is answered ahead of the rows
//!   that ask, so a row whose `IF` branch never evaluates the subquery has
//!   an answer nobody reads. If the set execution fails for any reason, the
//!   slot declines for the rest of the operator and the per-row path
//!   answers, raising whatever it raises on the row that raises it.
//! - **One answer per statement.** HAVING, ORDER BY and the select list
//!   each hold their own copy of a subquery they share, in operators that
//!   do not know of each other. The answers live with the statement, keyed
//!   by the subquery's text and the outer columns it reads, so the operator
//!   that asks second finds every tuple the first one answered.
//! - **Volatile subqueries** are never answered here: the slot's memo
//!   classification is consulted first.
//! - **Snapshot**: the provider is pinned for the statement, so the set
//!   execution reads the tables every per-row execution would have read.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::mem::size_of;
use std::sync::{Arc, Mutex, PoisonError};

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundJoinKind, BoundQuery, BoundTable,
    OuterSetQuery, ScalarFunction,
};
use pintail_types::{DataType, Value};

use super::membership::{self, MaterializedMembership};
use super::memo::{DependentMemo, SubquerySlot};
use super::{
    BatchStream, DEPENDENT_SET_EXECUTIONS, DependentRow, ExecError, Execution, MemoryTracker,
    PullOperator, RowsBatchStream, ScanProvider, dependent_subquery_memory_limit,
    materialize_subquery, substitute_outer_query,
};
use crate::collation::Collation;
use crate::logical::Scan;
use crate::{LogicalPlanner, Optimizer, PhysicalPlanner, RecordBatch};

/// The answers of every set-at-a-time subquery of one statement, by the
/// subquery's text and outer columns.
pub(super) type StatementSets = Arc<Mutex<HashMap<String, Arc<Mutex<SharedAnswers>>>>>;

/// One subquery's answers, shared by every operator that holds a copy of it.
#[derive(Debug)]
pub(super) struct SharedAnswers {
    /// The subquery's value for each outer tuple answered so far.
    answers: HashMap<Vec<Value>, Value>,
    /// The subquery's value over no rows.
    empty: Value,
}

/// What one subquery slot's set-at-a-time form is doing.
pub(super) enum SetState {
    /// Answering every tuple it has seen.
    Ready(Box<SetAnswers>),
    /// The outer columns were not all in the operator's input, or an
    /// execution failed. Never retried.
    Declined,
}

/// Where one slot finds its outer values and its answers.
pub(super) struct SetAnswers {
    /// Position in the operator's input of each outer column, in the order
    /// the form's relations list them.
    positions: Vec<usize>,
    /// Per relation: its synthetic table, where its columns start within a
    /// tuple, and their types.
    relations: Vec<OuterRelation>,
    shared: Arc<Mutex<SharedAnswers>>,
}

/// A virtual relation's synthetic table, the offset of its first column
/// within a tuple, and its column types.
type OuterRelation = (TableId, usize, Vec<DataType>);

/// The subquery's value for the current row from its set-at-a-time form,
/// or `None` for the other paths to answer.
pub(super) fn answer(
    slot: SubquerySlot,
    query: &BoundQuery,
    form: &OuterSetQuery,
    context: &DependentRow<'_>,
    memo: &mut DependentMemo,
) -> Result<Option<Value>, ExecError> {
    if !memo.memoizable(slot) {
        return Ok(None);
    }
    let mut state = memo.outer_sets.remove(&slot).unwrap_or_else(|| {
        prepare(query, form, context).map_or(SetState::Declined, |answers| {
            SetState::Ready(Box::new(answers))
        })
    });
    let mut found = None;
    let mut failed = false;
    if let SetState::Ready(set) = &state
        && let Some(tuple) = tuple_at(context.batch, context.row, &set.positions)
    {
        let mut shared = set.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if !shared.answers.contains_key(&tuple) {
            failed = fill(set, &mut shared, form, context).is_err();
        }
        if !failed {
            found = shared.answers.get(&tuple).cloned();
        }
    }
    if failed {
        state = SetState::Declined;
    }
    memo.outer_sets.insert(slot, state);
    // A failure above may have been the statement being cancelled; say so
    // here rather than start the per-row path on a dead statement.
    context.memory.check_interruption()?;
    Ok(found)
}

/// Locates the outer columns and finds the statement's answers for this
/// subquery, taking its value over no rows if no operator has yet.
fn prepare(
    query: &BoundQuery,
    form: &OuterSetQuery,
    context: &DependentRow<'_>,
) -> Option<SetAnswers> {
    let mut positions = Vec::new();
    let mut relations = Vec::with_capacity(form.relations.len());
    let mut identity = form.text.clone();
    for relation in &form.relations {
        relations.push((
            relation.table_id,
            positions.len(),
            relation
                .columns
                .iter()
                .map(|column| column.data_type)
                .collect(),
        ));
        for column in &relation.columns {
            positions.push(context.columns.iter().position(|candidate| {
                candidate.database_id == column.database_id
                    && candidate.table_id == column.table_id
                    && candidate.column_id == column.column_id
                    && candidate
                        .relation_name
                        .eq_ignore_ascii_case(&column.relation_name)
            })?);
            // Writing to a String cannot fail.
            let _ = write!(
                identity,
                "\u{0}{}.{}.{}",
                column.table_id.get(),
                column.column_id,
                column.relation_name.to_ascii_lowercase()
            );
        }
    }
    let statement = context.memory.outer_sets();
    let existing = statement
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&identity)
        .cloned();
    let shared = if let Some(shared) = existing {
        shared
    } else {
        let shared = Arc::new(Mutex::new(SharedAnswers {
            answers: HashMap::new(),
            empty: value_over_nothing(query, context)?,
        }));
        statement
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(identity, Arc::clone(&shared));
        shared
    };
    Some(SetAnswers {
        positions,
        relations,
        shared,
    })
}

/// What the subquery answers for an outer row no inner row matches: the
/// subquery itself, over an input no row passes.
fn value_over_nothing(query: &BoundQuery, context: &DependentRow<'_>) -> Option<Value> {
    let mut over_nothing = query.clone();
    over_nothing.outer_set = None;
    substitute_outer_query(
        &mut over_nothing,
        context.batch,
        context.row,
        context.columns,
        &mut Vec::new(),
    )
    .ok()?;
    over_nothing.filter = Some(BoundExpr {
        kind: BoundExprKind::Literal(Value::Boolean(false)),
        data_type: Some(DataType::Boolean),
        nullable: false,
    });
    let values = materialize_subquery(
        over_nothing,
        context.provider,
        dependent_subquery_memory_limit(context.memory, context.batch).ok()?,
        context.memory.deadline,
        Some(2),
        context.collation,
    )
    .ok()?;
    match values.as_slice() {
        [empty] => Some(empty.clone()),
        // A lookup, not an aggregate: no row is NULL.
        [] if query.aggregates.is_empty() => Some(Value::Null),
        _ => None,
    }
}

/// The outer tuple of `row`. `None` when a value's display does not
/// identify it, which the memo refuses as a key for the same reason.
fn tuple_at(batch: &RecordBatch, row: usize, positions: &[usize]) -> Option<Vec<Value>> {
    positions
        .iter()
        .map(|&position| {
            batch
                .column(position)
                .and_then(|values| values.value(row))
                .filter(|value| !matches!(value, Value::DecimalAverage(_)))
                .cloned()
        })
        .collect()
}

/// The distinct outer tuples of the current batch, and of the batches the
/// operator holds beyond it, that have no answer yet.
fn unanswered(
    set: &SetAnswers,
    shared: &SharedAnswers,
    context: &DependentRow<'_>,
) -> Vec<Vec<Value>> {
    let mut seen: HashSet<Vec<Value>> = HashSet::new();
    let mut tuples = Vec::new();
    for batch in std::iter::once(context.batch).chain(context.ahead) {
        for row in batch.selection().selected_rows() {
            if let Some(tuple) = tuple_at(batch, row, &set.positions)
                && !shared.answers.contains_key(&tuple)
                && seen.insert(tuple.clone())
            {
                tuples.push(tuple);
            }
        }
    }
    tuples
}

/// Answers every unanswered tuple the operator holds in one execution of
/// the form. The answers' bytes stay charged to the statement.
fn fill(
    set: &SetAnswers,
    shared: &mut SharedAnswers,
    form: &OuterSetQuery,
    context: &DependentRow<'_>,
) -> Result<(), ExecError> {
    let tuples = unanswered(set, shared, context);
    let bytes = tuples.iter().fold(0_usize, |total, tuple| {
        tuple.iter().fold(
            total
                .saturating_add(size_of::<Vec<Value>>())
                .saturating_add(size_of::<Value>().saturating_mul(2))
                .saturating_add(shared.empty.heap_bytes()),
            |total, value| {
                total
                    .saturating_add(size_of::<Value>())
                    .saturating_add(value.heap_bytes())
            },
        )
    });
    context.memory.reserve(bytes)?;

    let mut query = form.query.clone();
    let count = u64::try_from(tuples.len()).unwrap_or(u64::MAX);
    let sized = |table: &mut BoundTable| {
        if set
            .relations
            .iter()
            .any(|(table_id, ..)| *table_id == table.table_id)
            && table.input.is_none()
        {
            table.row_count = Some(count);
            table.estimated_rows = Some(count);
        }
    };
    for source in &mut query.from {
        sized(&mut source.base);
        for join in &mut source.joins {
            sized(&mut join.table);
        }
    }
    query.tables.iter_mut().for_each(sized);
    let held = restrict_to_outer_values(&mut query, &set.relations, &tuples, context.collation);
    context.memory.reserve(held)?;
    let overlay = OuterRowsProvider {
        base: context.provider,
        relations: &set.relations,
        tuples: &tuples,
    };
    DEPENDENT_SET_EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let logical = Optimizer::optimize(LogicalPlanner::plan(query));
    let physical = PhysicalPlanner::plan(logical, context.collation)?;
    let mut execution = Execution::start_with_deadline(
        physical,
        &overlay,
        dependent_subquery_memory_limit(context.memory, context.batch)?,
        context.memory.deadline,
        context.collation,
    )?;
    let mut values: Vec<Option<Value>> = vec![None; tuples.len()];
    let mut grown = 0_usize;
    while let Some(batch) = execution.next_batch()? {
        for row in batch.selection().selected_rows() {
            let ordinal = match batch.column(0).and_then(|column| column.value(row)) {
                Some(Value::UInt64(ordinal)) => usize::try_from(*ordinal).ok(),
                Some(Value::Int64(ordinal)) => usize::try_from(*ordinal).ok(),
                _ => None,
            };
            let value = batch.column(1).and_then(|column| column.value(row));
            let (Some(ordinal), Some(value)) = (ordinal, value) else {
                return Err(ExecError::InvalidBatch(
                    "outer-set subquery result is missing its ordinal or value",
                ));
            };
            let Some(slot @ None) = values.get_mut(ordinal) else {
                return Err(ExecError::InvalidBatch(
                    "outer-set subquery answered an ordinal twice or out of range",
                ));
            };
            grown = grown.saturating_add(value.heap_bytes());
            *slot = Some(value.clone());
        }
    }
    drop(execution);
    context.memory.reserve(grown)?;
    for (tuple, value) in tuples.into_iter().zip(values) {
        let value = value.unwrap_or_else(|| shared.empty.clone());
        shared.answers.insert(tuple, value);
    }
    Ok(())
}

/// Input rows an operator reads ahead of the row it is answering when one
/// of its expressions holds a set-at-a-time subquery, so one execution
/// answers them all; zero when none does, and the operator streams as it
/// always has.
pub(super) fn window_rows<'a>(expressions: impl Iterator<Item = &'a BoundExpr>) -> usize {
    const WINDOW_ROWS: usize = 1 << 18;
    let mut holds = false;
    for expression in expressions {
        holds = holds || holds_outer_set(expression);
    }
    if holds { WINDOW_ROWS } else { 0 }
}

fn holds_outer_set(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::ScalarSubquery(query) => query.outer_set.is_some(),
        BoundExprKind::InSubquery { expr, .. }
        | BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => holds_outer_set(expr),
        BoundExprKind::Binary { left, right, .. } => {
            holds_outer_set(left) || holds_outer_set(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(holds_outer_set),
        BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

/// The operator's next input batch, with up to `window` rows of the
/// batches after it read into `held` first. The batches come back in input
/// order; `held` is what a set execution may answer ahead. Reading ahead
/// stops at the first batch the query's memory cannot also hold.
pub(super) fn next_held(
    input: &mut PullOperator,
    memory: &MemoryTracker,
    held: &mut VecDeque<RecordBatch>,
    window: usize,
) -> Result<Option<RecordBatch>, ExecError> {
    if held.is_empty() {
        let mut rows = 0_usize;
        while let Some(batch) = input.next_batch(memory)? {
            rows = rows.saturating_add(batch.visible_row_count());
            let fits = memory.ensure_transient(
                held.iter()
                    .map(RecordBatch::estimated_bytes)
                    .fold(batch.estimated_bytes(), usize::saturating_add),
            );
            held.push_back(batch);
            if rows >= window || fits.is_err() {
                break;
            }
        }
        held.make_contiguous();
    }
    Ok(held.pop_front())
}

/// Serves the form's virtual relations from the collected outer tuples and
/// delegates every other scan to the statement's provider.
struct OuterRowsProvider<'a> {
    base: &'a dyn ScanProvider,
    relations: &'a [OuterRelation],
    tuples: &'a [Vec<Value>],
}

impl ScanProvider for OuterRowsProvider<'_> {
    fn open_scan(
        &self,
        scan: &Scan,
        memory_limit: usize,
    ) -> Result<Box<dyn BatchStream>, ExecError> {
        let relation = (scan.table.database_id == DatabaseId::new(u64::MAX)
            && scan.table.input.is_none())
        .then(|| {
            self.relations
                .iter()
                .find(|(table_id, ..)| *table_id == scan.table.table_id)
        })
        .flatten();
        let Some((_, start, types)) = relation else {
            return self.base.open_scan(scan, memory_limit);
        };
        // Column 1 is the ordinal; column n is the relation's (n - 2)th.
        let mut column_types = Vec::with_capacity(scan.projected_column_ids.len());
        let mut offsets = Vec::with_capacity(scan.projected_column_ids.len());
        for id in &scan.projected_column_ids {
            if *id == 1 {
                column_types.push(DataType::UInt64);
                offsets.push(None);
                continue;
            }
            let offset = usize::try_from(id.saturating_sub(2)).unwrap_or(usize::MAX);
            let data_type = types
                .get(offset)
                .copied()
                .ok_or(ExecError::InvalidPhysicalPlan(
                    "outer-rows scan projects an unknown column",
                ))?;
            column_types.push(data_type);
            offsets.push(Some(start + offset));
        }
        let rows = self
            .tuples
            .iter()
            .enumerate()
            .map(|(ordinal, tuple)| {
                offsets
                    .iter()
                    .map(|offset| match offset {
                        None => Value::UInt64(u64::try_from(ordinal).unwrap_or(u64::MAX)),
                        Some(offset) => tuple[*offset].clone(),
                    })
                    .collect()
            })
            .collect();
        Ok(Box::new(RowsBatchStream {
            rows,
            cursor: 0,
            column_types,
        }))
    }

    fn table_provider(
        &self,
        database_id: DatabaseId,
        table_id: TableId,
    ) -> Option<Box<dyn ScanProvider + Send + Sync>> {
        self.base.table_provider(database_id, table_id)
    }
}

/// A column of the form's FROM clause, by what identifies it there.
type ColumnKey = (TableId, u32, String);

/// The outer values a column is restricted to: their offset within a
/// tuple, and the type and collation they carry.
type OuterValues = (usize, DataType, Option<String>);

fn column_key(column: &BoundColumn) -> ColumnKey {
    (
        column.table_id,
        column.column_id,
        column.relation_name.to_ascii_lowercase(),
    )
}

/// Tells the scans which rows can matter: a column a join condition
/// equates with an outer value can only match while it holds one of the
/// outer values collected, and so can a column equated with that column in
/// a later join.
///
/// `t JOIN u ON u.k = t.k` under `t.k = outer.k` reads `u` whole unless
/// `u.k` is restricted too, and the join that would discover the
/// restriction at run time has `t JOIN ..`, not the handful of outer rows,
/// on its other side. So every such column gains `column IN (outer
/// values)` in the join condition that introduces it. The addition only
/// repeats what the equalities already imply - a row it removes could not
/// have joined - and it is made only where the column and the outer value
/// have one type and, for text, one collation, so `IN` and `=` agree on
/// what equals what. An INNER join takes it for either side's column; a
/// LEFT join only for a column of its right table, whose unmatched rows
/// the join drops anyway.
///
/// Returns the bytes the membership indexes hold.
fn restrict_to_outer_values(
    query: &mut BoundQuery,
    relations: &[OuterRelation],
    tuples: &[Vec<Value>],
    collation: Collation,
) -> usize {
    let virtual_database = DatabaseId::new(u64::MAX);
    let relation_of = |column: &BoundColumn| {
        (column.database_id == virtual_database)
            .then(|| {
                relations
                    .iter()
                    .find(|(table_id, ..)| *table_id == column.table_id)
            })
            .flatten()
    };
    let outer_values = |column: &BoundColumn| -> Option<OuterValues> {
        let (_, start, types) = relation_of(column)?;
        let offset = usize::try_from(column.column_id.checked_sub(2)?).ok()?;
        (offset < types.len()).then(|| (start + offset, column.data_type, column.collation.clone()))
    };
    let mut restricted: HashMap<ColumnKey, OuterValues> = HashMap::new();
    let mut held = 0_usize;
    for source in &mut query.from {
        // Twice: an equality may name its restricted side in a later join.
        for _ in 0..2 {
            for join in &mut source.joins {
                if !matches!(join.kind, BoundJoinKind::Inner | BoundJoinKind::Left) {
                    continue;
                }
                let Some(mut condition) = join.condition.take() else {
                    continue;
                };
                let mut found: Vec<(BoundExpr, OuterValues)> = Vec::new();
                let mut conjuncts = Vec::new();
                and_conjuncts(&condition, &mut conjuncts);
                for (known, fresh, expression) in conjuncts.into_iter().flat_map(column_equality) {
                    let Some(values) =
                        outer_values(known).or_else(|| restricted.get(&column_key(known)).cloned())
                    else {
                        continue;
                    };
                    let of_right_table = fresh.table_id == join.table.table_id
                        && fresh
                            .relation_name
                            .eq_ignore_ascii_case(&join.table.relation_name);
                    if fresh.outer
                        || relation_of(fresh).is_some()
                        || restricted.contains_key(&column_key(fresh))
                        || fresh.data_type != values.1
                        || fresh.collation != values.2
                        || fresh.enum_labels.is_some()
                        || !matches!(
                            fresh.data_type.storage_type(),
                            DataType::Int64 | DataType::UInt64 | DataType::Utf8
                        )
                        || (join.kind == BoundJoinKind::Left && !of_right_table)
                    {
                        continue;
                    }
                    restricted.insert(column_key(fresh), values.clone());
                    found.push((expression.clone(), values));
                }
                for (column, values) in found {
                    if let Some(member) =
                        membership_of(column, &values, tuples, collation, &mut held)
                    {
                        condition = BoundExpr {
                            data_type: Some(DataType::Boolean),
                            nullable: true,
                            kind: BoundExprKind::Binary {
                                op: BinaryOp::And,
                                left: Box::new(condition),
                                right: Box::new(member),
                            },
                        };
                    }
                }
                join.condition = Some(condition);
            }
        }
    }
    held
}

/// Both readings of `a = b` between two columns: each side as the one
/// whose values are known, with the other and its expression.
fn column_equality(conjunct: &BoundExpr) -> Vec<(&BoundColumn, &BoundColumn, &BoundExpr)> {
    if let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left,
        right,
    } = &conjunct.kind
        && let (BoundExprKind::Column(first), BoundExprKind::Column(second)) =
            (&left.kind, &right.kind)
    {
        vec![(first, second, &**right), (second, first, &**left)]
    } else {
        Vec::new()
    }
}

/// `column IN (the distinct non-NULL outer values at this offset)`, hashed
/// when there are enough of them; `None` when there are none.
fn membership_of(
    column: BoundExpr,
    (offset, data_type, column_collation): &OuterValues,
    tuples: &[Vec<Value>],
    collation: Collation,
    held: &mut usize,
) -> Option<BoundExpr> {
    let mut distinct = HashSet::new();
    let members = tuples
        .iter()
        .map(|tuple| &tuple[*offset])
        .filter(|value| !matches!(value, Value::Null) && distinct.insert(*value))
        .cloned()
        .collect::<Vec<_>>();
    if members.is_empty() {
        return None;
    }
    let member_collation = column_collation
        .as_deref()
        .and_then(Collation::from_mysql_name)
        .unwrap_or(collation);
    // A literal list is what a scan can hand the table's side index, which
    // reads the matching rows instead of testing every row; past the most
    // the index takes, a hashed set tests each row in constant time.
    let indexed = if members.len() <= crate::storage::INDEX_LOOKUP_VALUES {
        MaterializedMembership::Memory(members)
    } else {
        membership::index_members(
            members,
            member_collation,
            Some(*data_type),
            Some(*data_type),
        )
    };
    let kind = match indexed {
        MaterializedMembership::Memory(members) => {
            let mut args = Vec::with_capacity(members.len() + 1);
            args.push(column);
            args.extend(members.into_iter().map(|value| BoundExpr {
                data_type: Some(*data_type),
                nullable: false,
                kind: BoundExprKind::Literal(value),
            }));
            BoundExprKind::Scalar {
                function: ScalarFunction::InList { negated: false },
                args,
            }
        }
        MaterializedMembership::Prepared(membership, bytes) => {
            *held = held.saturating_add(bytes);
            BoundExprKind::PreparedIn {
                expr: Box::new(column),
                membership,
                negated: false,
            }
        }
    };
    Some(BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: true,
        kind,
    })
}

/// The top-level AND conjuncts of `expression`.
fn and_conjuncts<'a>(expression: &'a BoundExpr, conjuncts: &mut Vec<&'a BoundExpr>) {
    if let BoundExprKind::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = &expression.kind
    {
        and_conjuncts(left, conjuncts);
        and_conjuncts(right, conjuncts);
    } else {
        conjuncts.push(expression);
    }
}
