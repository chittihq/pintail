//! A hash index that answers a correlated `EXISTS` or scalar subquery
//! without re-running it.
//!
//! The dependent path answers `EXISTS (SELECT .. FROM t WHERE ..)` and
//! `(SELECT c FROM t WHERE ..)` once per outer row by cloning, planning and
//! executing the inner query. The memo shares answers between rows that
//! carry the same correlation tuple, but when the tuples are mostly
//! distinct every row still pays a full plan and execution - from most of a
//! millisecond to tens of milliseconds each.
//!
//! When the inner query is one base table under a conjunctive `WHERE`, the
//! question each row asks is always the same one with different constants:
//! "which rows of `t` carry these keys and also satisfy the rest?". So the
//! operator reads `t` once, filtered by the conjuncts that do not mention
//! the outer row, and indexes the surviving rows, in scan order, by the
//! columns the outer row is equated with. Each outer row then looks up its
//! key and evaluates only the remaining correlated conjuncts over the
//! candidates: `EXISTS` stops at the first that is true, a scalar subquery
//! takes the first under `LIMIT 1` and otherwise needs at most one.
//!
//! Three shapes are read as that one:
//!
//! - **A derived table that does not read the outer row** is a table: it is
//!   read once, like one.
//! - **A derived table that is a selection, some of whose conjuncts read
//!   the outer row** - what a row constructor's `IN (SELECT ..)` is
//!   rewritten to - is the same selection without those conjuncts, with
//!   the conjuncts applied to its rows afterwards.
//! - **A projection that is itself a subquery of the inner row** is
//!   resolved against the one qualifying row by the ordinary dependent
//!   path, whose own memo and index live as long as this index does.
//!
//! With no equality to key by, every inner row is a candidate for every
//! outer row; that is only taken for an inner side of a few rows.
//!
//! What it must never change, and how each is kept:
//!
//! - **Comparison semantics**: only `inner_column = outer_expression` with
//!   both sides integers, or both sides text, becomes a key. Integers key by
//!   value, signed against unsigned included. Text keys by its collation
//!   weight bytes - the normalization the hash join uses - under the
//!   collation the equality itself compiles to, so case and accent folding
//!   and PAD SPACE against NO PAD are exactly `=`'s. An equality whose
//!   collation would change when the outer value becomes a literal, a text
//!   against a number, an ENUM: none are keys. Everything that is not a key
//!   stays in the residual and is evaluated by the ordinary expression code.
//! - **NULL**: a NULL key on either side is never equal to anything, so an
//!   inner row with one is not indexed and an outer row with one finds
//!   nothing. A residual that evaluates to NULL is not a match, as in
//!   `WHERE`.
//! - **Row order**: candidates are held in the order the table was read,
//!   which is the order a per-row execution reads the same rows in, so
//!   `LIMIT 1` answers with the same row.
//! - **Anything unexpected** - a key value of the wrong kind, a refused
//!   memory charge, too many rows, a failure while reading the table - falls
//!   back to the per-row path, which answers as it always has. The index can
//!   only remove work, never fail a query that runs without it.
//! - **Snapshot**: the provider is pinned for the statement, so the table
//!   read once is the table every per-row execution would have read.

use std::collections::HashMap;
use std::mem::size_of;

use pintail_sql::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundProjection, BoundQuery, ScalarFunction,
};
use pintail_types::{DataType, Value};

use super::join::{JoinHashKey, normalized_join_key};
use super::memo::DependentMemo;
use super::{
    DependentRow, ExecError, Execution, JoinKeyMode, KeyForm, MemoryTracker,
    dependent_subquery_memory_limit, substitute_outer_expr,
};
use crate::collation::Collation;
use crate::expression::{CompiledExpr, predicate_truth};
use crate::{LogicalPlanner, Optimizer, PhysicalPlanner, RecordBatch};

/// Inner rows one index holds at most. A table larger than this is read by
/// the per-row path, whose key filter can prune what a full read cannot.
const MAX_INDEX_ROWS: usize = 4 << 20;

/// Inner rows an index with no key holds at most: every one of them is
/// evaluated for every outer row.
const MAX_KEYLESS_ROWS: usize = 256;

/// Inner rows per per-row execution the index waits out before building. A
/// build reads the whole filtered table once; an outer input of a handful
/// of rows is cheaper answered one execution at a time, so a large table
/// lets the per-row path answer the first rows and builds only once the
/// outer input has shown it is not tiny.
const ROWS_PER_WAITED_EXECUTION: u64 = 8_192;
const MAX_WAITED_EXECUTIONS: u64 = 32;

/// How often, in candidates evaluated, a probe checks for cancellation.
const INTERRUPTION_STRIDE: usize = 1_024;

/// The share of the query's remaining memory one index may hold. An
/// operator that resets its memo under memory pressure would otherwise
/// drop and rebuild an index that fills half the ceiling on every row.
const MEMORY_SHARE_DIVISOR: usize = 4;

/// What one subquery slot's index is doing.
pub(super) enum IndexState {
    /// The shape qualifies; the per-row path answers `remaining` more rows
    /// before the index is built.
    Pending {
        plan: Box<IndexPlan>,
        remaining: u64,
    },
    /// Built and answering every row it can.
    Built(Box<SubqueryIndex>),
    /// The shape does not qualify, or the build gave up. Never retried.
    Declined,
}

/// Which subquery form a slot holds, and so what an answer is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubqueryForm {
    Exists,
    Scalar,
}

/// What the index answers with, once the candidates are filtered.
#[derive(Clone)]
enum Answer {
    /// Whether any candidate qualifies.
    Exists,
    /// The projection over the one qualifying row, NULL for none. With
    /// `first` (`LIMIT 1`) the first in scan order wins; without it a second
    /// qualifying row is the cardinality error.
    /// `nested` when the projection holds a subquery of the inner row.
    Scalar {
        projection: BoundExpr,
        first: bool,
        nested: bool,
    },
}

/// How one key's two sides are normalized to compare.
#[derive(Clone, Copy)]
enum PlannedForm {
    Integer,
    /// Text under the collation the equality compiles to: the named one,
    /// or the plan's when the operands name none.
    Text(Option<&'static str>),
}

/// One `inner_column = outer_expression` key.
struct PlannedKey {
    inner: usize,
    outer: BoundExpr,
    form: PlannedForm,
}

/// The parts of an inner query the index is built from, found once per
/// operator.
pub(super) struct IndexPlan {
    /// The inner table filtered by the uncorrelated conjuncts, projecting
    /// only `layout`.
    materialize: BoundQuery,
    /// Inner columns in the order `materialize` projects them.
    layout: Vec<BoundColumn>,
    keys: Vec<PlannedKey>,
    /// The correlated conjuncts that are not keys, joined by AND.
    residual: Option<BoundExpr>,
    answer: Answer,
}

enum BuiltAnswer {
    Exists,
    Scalar {
        projection: CompiledExpr,
        first: bool,
    },
    /// A projection holding subqueries of the inner row, resolved per
    /// qualifying row by the dependent path under a memo of its own.
    Nested {
        projection: BoundExpr,
        first: bool,
        memo: Option<Box<DependentMemo>>,
        /// Whether the projection has been seen to read nothing beyond the
        /// inner row.
        checked: bool,
    },
}

/// A built index: the filtered inner rows and their keys.
pub(super) struct SubqueryIndex {
    batches: Vec<RecordBatch>,
    /// Key tuple to `(batch, row)` of every inner row carrying it, in scan
    /// order.
    rows: HashMap<Vec<JoinHashKey>, Vec<(u32, u32)>>,
    layout: Vec<BoundColumn>,
    /// Outer key expressions compiled against the operator's input, with
    /// how each is normalized.
    outer_keys: Vec<(CompiledExpr, JoinKeyMode)>,
    residual: Option<BoundExpr>,
    answer: BuiltAnswer,
    /// Bytes charged to the query's tracker, returned by `release`.
    reserved: usize,
    /// Reused probe key.
    probe: Vec<JoinHashKey>,
}

impl SubqueryIndex {
    pub(super) fn release(&mut self, memory: &MemoryTracker) {
        if let BuiltAnswer::Nested { memo, .. } = &mut self.answer
            && let Some(memo) = memo.take()
        {
            super::record_dependent_memo(memo.finish(memory));
        }
        memory.release(self.reserved);
        self.reserved = 0;
        self.batches.clear();
        self.rows.clear();
    }
}

/// Counts the index moved, for the process counters.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IndexStats {
    pub(super) builds: u64,
    pub(super) probes: u64,
    pub(super) declines: u64,
}

/// Decides, once per slot, whether `query` has a shape the index answers,
/// and says why not when it has none.
pub(super) fn plan(query: &BoundQuery, form: SubqueryForm) -> Result<IndexState, &'static str> {
    analyse(query, form).map(|(plan, estimated_rows)| IndexState::Pending {
        plan: Box::new(plan),
        remaining: estimated_rows.map_or(MAX_WAITED_EXECUTIONS, |rows| {
            (rows / ROWS_PER_WAITED_EXECUTION).min(MAX_WAITED_EXECUTIONS)
        }),
    })
}

/// Inner rows a built index holds.
pub(super) fn indexed_rows(index: &SubqueryIndex) -> u64 {
    index
        .rows
        .values()
        .map(|rows| u64::try_from(rows.len()).unwrap_or(u64::MAX))
        .sum()
}

/// The answer a query's projection and LIMIT ask for, when the index can
/// give it.
fn answer_of(query: &BoundQuery, form: SubqueryForm) -> Option<Answer> {
    match form {
        SubqueryForm::Exists => {
            // EXISTS ignores what a row carries, its order, and any LIMIT
            // that keeps one row.
            let keeps_a_row = query
                .limit
                .is_none_or(|limit| limit.offset == 0 && limit.count >= 1);
            let bare = query.projection.iter().all(|projection| {
                matches!(
                    projection.expr.kind,
                    BoundExprKind::Literal(_) | BoundExprKind::Column(_)
                )
            });
            (keeps_a_row && bare).then_some(Answer::Exists)
        }
        SubqueryForm::Scalar => {
            let [projection] = query.projection.as_slice() else {
                return None;
            };
            let first = match query.limit {
                None => false,
                Some(limit) if limit.offset == 0 && limit.count >= 1 => limit.count == 1,
                Some(_) => return None,
            };
            let mut inner = Vec::new();
            let mut has_outer = false;
            let nested = !plain_expression(&projection.expr);
            if nested && !holds_only_subqueries(&projection.expr) {
                return None;
            }
            columns_of(&projection.expr, &mut inner, &mut has_outer);
            (!has_outer
                && !query.distinct
                && query.order_by.is_empty()
                && query.hidden_sort_columns == 0)
                .then(|| Answer::Scalar {
                    projection: projection.expr.clone(),
                    first,
                    nested,
                })
        }
    }
}

#[allow(clippy::too_many_lines)] // one shape check, read top to bottom
fn analyse(
    query: &BoundQuery,
    form: SubqueryForm,
) -> Result<(IndexPlan, Option<u64>), &'static str> {
    let [source] = query.from.as_slice() else {
        return Err("it reads more than one FROM item");
    };
    if !source.joins.is_empty() {
        return Err("it joins");
    }
    if !query.group_by.is_empty()
        || !query.aggregates.is_empty()
        || !query.windows.is_empty()
        || query.having.is_some()
    {
        return Err("it aggregates or has a window function");
    }
    if !query.union_all.is_empty() || !query.set_ops.is_empty() || query.recursive.is_some() {
        return Err("it is a set operation");
    }
    // A derived table that reads the outer row is not one table read once;
    // one that only renames a base table's columns is that table.
    let flattened;
    let query = match source.base.input.as_deref() {
        Some(input) if super::bound_query_has_outer_refs(input) => {
            flattened = lift_correlated(query, input)
                .ok_or("its derived table reads the outer row and is not a plain selection")?;
            &flattened
        }
        _ => query,
    };
    let [source] = query.from.as_slice() else {
        return Err("it reads more than one FROM item");
    };
    let answer =
        answer_of(query, form).ok_or("its select list, ORDER BY or LIMIT is not one it reads")?;
    let base = &source.base;
    let estimated_rows = base.estimated_rows.or(base.row_count);
    if estimated_rows.is_some_and(|rows| rows > MAX_INDEX_ROWS as u64) {
        return Err("its table is larger than an index holds");
    }
    let filter = query
        .filter
        .as_ref()
        .ok_or("it has no WHERE reading the outer row")?;
    let mut conjuncts = Vec::new();
    flatten_and(filter, &mut conjuncts);

    let mut uncorrelated = Vec::new();
    let mut keys = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if !plain_expression(conjunct) {
            return Err("a WHERE conjunct holds a subquery");
        }
        let mut inner_columns = Vec::new();
        let mut has_outer = false;
        columns_of(conjunct, &mut inner_columns, &mut has_outer);
        if inner_columns.iter().any(|column| !belongs_to(column, base)) {
            return Err("a WHERE conjunct reads a column of another relation");
        }
        if !has_outer {
            uncorrelated.push(conjunct.clone());
        } else if let Some(key) = equality_key(conjunct) {
            keys.push(key);
        } else {
            residual.push(conjunct.clone());
        }
    }
    // With no key every inner row is a candidate for every outer row.
    if keys.is_empty()
        && (residual.is_empty()
            || estimated_rows.is_some_and(|rows| rows > MAX_KEYLESS_ROWS as u64))
    {
        return Err("no equality keys its table by the outer row");
    }

    let mut layout: Vec<BoundColumn> = Vec::new();
    let mut add_column = |column: &BoundColumn| {
        if let Some(position) = layout.iter().position(|seen| same_column(seen, column)) {
            position
        } else {
            layout.push(BoundColumn {
                outer: false,
                ..column.clone()
            });
            layout.len() - 1
        }
    };
    let keys = keys
        .into_iter()
        .map(|(inner, outer, form)| PlannedKey {
            inner: add_column(inner),
            outer: outer.clone(),
            form,
        })
        .collect::<Vec<_>>();
    let mut read = residual.iter().collect::<Vec<_>>();
    match &answer {
        // Which of the inner row's columns a nested subquery reads is not
        // looked for: the row is held whole.
        Answer::Scalar { nested: true, .. } => {
            for column in &base.columns {
                add_column(column);
            }
        }
        Answer::Scalar { projection, .. } => read.push(projection),
        Answer::Exists => {}
    }
    for expression in read {
        let mut inner_columns = Vec::new();
        let mut has_outer = false;
        columns_of(expression, &mut inner_columns, &mut has_outer);
        for column in inner_columns {
            if !belongs_to(column, base) {
                return Err("its select list reads a column of another relation");
            }
            add_column(column);
        }
    }

    let materialize = materialize_query(query, &layout, uncorrelated);
    Ok((
        IndexPlan {
            materialize,
            layout,
            keys,
            residual: conjoin(residual),
            answer,
        },
        estimated_rows,
    ))
}

/// `query` with the conjuncts of its derived table - `input` - that read
/// the outer row lifted out of it: the derived table is left reading no
/// outer row, each inner column those conjuncts read is carried out as one
/// more column of it, and the conjuncts join the query's own, reading those
/// columns. A selection filters the same rows before or after it is
/// projected, so the rows are the ones the query saw. `None` for a derived
/// table that is more than a selection, or whose lifted columns would not
/// compare outside it as they do inside.
fn lift_correlated(query: &BoundQuery, input: &BoundQuery) -> Option<BoundQuery> {
    if !input.group_by.is_empty()
        || !input.aggregates.is_empty()
        || !input.windows.is_empty()
        || input.having.is_some()
        || input.distinct
        || !input.order_by.is_empty()
        || input.hidden_sort_columns != 0
        || !input.union_all.is_empty()
        || !input.set_ops.is_empty()
        || input.limit.is_some()
        || input.recursive.is_some()
    {
        return None;
    }
    let mut conjuncts = Vec::new();
    flatten_and(input.filter.as_ref()?, &mut conjuncts);
    let (lifted, kept): (Vec<&BoundExpr>, Vec<&BoundExpr>) = conjuncts
        .into_iter()
        .partition(|conjunct| super::bound_expr_has_outer_refs(conjunct));
    let mut flat = query.clone();
    let derived = &mut flat.from.first_mut()?.base;
    let mut inner = derived.input.take()?;
    inner.filter = conjoin(kept.into_iter().cloned().collect());
    // Only the conjuncts read the outer row: nothing else of the derived
    // table may.
    if super::bound_query_has_outer_refs(&inner) {
        return None;
    }
    let mut carried: Vec<BoundColumn> = Vec::new();
    let mut outside = Vec::with_capacity(lifted.len());
    for conjunct in lifted {
        if !plain_expression(conjunct) {
            return None;
        }
        let mut conjunct = conjunct.clone();
        if !carry_out(&mut conjunct, derived, &mut inner, &mut carried) {
            return None;
        }
        outside.push(conjunct);
    }
    derived.input = Some(inner);
    flat.filter = conjoin(flat.filter.take().into_iter().chain(outside).collect());
    Some(flat)
}

/// Reads every inner column of a lifted conjunct through a column the
/// derived table gains for it. `false` when a column's comparisons depend
/// on something a derived column does not carry.
fn carry_out(
    expression: &mut BoundExpr,
    derived: &mut pintail_sql::BoundTable,
    inner: &mut BoundQuery,
    carried: &mut Vec<BoundColumn>,
) -> bool {
    match &mut expression.kind {
        BoundExprKind::Column(column) if column.outer => true,
        BoundExprKind::Column(column) => {
            if column.enum_labels.is_some()
                || column.timestamp
                || column.geometry
                || column.bit_width.is_some()
            {
                return false;
            }
            let position = carried
                .iter()
                .position(|seen| same_column(seen, column))
                .unwrap_or_else(|| {
                    carried.push(column.clone());
                    inner.projection.push(BoundProjection {
                        name: format!("<lifted-{}>", carried.len()),
                        expr: BoundExpr {
                            data_type: Some(column.data_type),
                            nullable: column.nullable,
                            kind: BoundExprKind::Column(column.clone()),
                        },
                    });
                    derived.columns.push(BoundColumn {
                        database_id: derived.database_id,
                        table_id: derived.table_id,
                        column_id: u32::try_from(inner.projection.len()).unwrap_or(u32::MAX),
                        relation_name: derived.relation_name.clone(),
                        name: format!("<lifted-{}>", carried.len()),
                        ..column.clone()
                    });
                    carried.len() - 1
                });
            let first = derived.columns.len() - carried.len();
            let Some(outside) = derived.columns.get(first + position) else {
                return false;
            };
            *column = outside.clone();
            true
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            carry_out(expr, derived, inner, carried)
        }
        BoundExprKind::Binary { left, right, .. } => {
            carry_out(left, derived, inner, carried) && carry_out(right, derived, inner, carried)
        }
        BoundExprKind::Scalar { args, .. } => args
            .iter_mut()
            .all(|argument| carry_out(argument, derived, inner, carried)),
        _ => false,
    }
}

/// The inner table filtered by `uncorrelated`, projecting `layout`.
fn materialize_query(
    query: &BoundQuery,
    layout: &[BoundColumn],
    uncorrelated: Vec<BoundExpr>,
) -> BoundQuery {
    BoundQuery {
        from: query.from.clone(),
        tables: query.tables.clone(),
        projection: layout
            .iter()
            .map(|column| BoundProjection {
                name: column.name.clone(),
                expr: BoundExpr {
                    data_type: Some(column.data_type),
                    nullable: column.nullable,
                    kind: BoundExprKind::Column(column.clone()),
                },
            })
            .collect(),
        filter: conjoin(uncorrelated),
        group_by: Vec::new(),
        aggregates: Vec::new(),
        windows: Vec::new(),
        having: None,
        distinct: false,
        order_by: Vec::new(),
        hidden_sort_columns: 0,
        union_all: Vec::new(),
        union_distinct: false,
        set_ops: Vec::new(),
        limit: None,
        recursive: None,
        outer_set: None,
        outer_set_refusal: None,
        text_collation: query.text_collation,
    }
}

fn key_mode(form: PlannedForm, fallback: Collation) -> JoinKeyMode {
    let form = match form {
        PlannedForm::Integer => KeyForm::Integer,
        PlannedForm::Text(named) => KeyForm::CollatedText(match named {
            Some(name) => match Collation::from_mysql_name(name) {
                Some(collation) => collation,
                None => fallback,
            },
            None => fallback,
        }),
    };
    JoinKeyMode {
        form,
        null_safe: false,
    }
}

/// The outer key expressions and the answer, compiled for this operator's
/// input - `None` when an outer column belongs to a scope further out.
fn compile_for(
    plan: &IndexPlan,
    context: &DependentRow<'_>,
) -> Option<(Vec<(CompiledExpr, JoinKeyMode)>, BuiltAnswer)> {
    let outer_keys = plan
        .keys
        .iter()
        .map(|key| {
            outer_columns_resolve(&key.outer, context.columns)
                .then(|| CompiledExpr::compile(&key.outer, context.columns, context.collation).ok())
                .flatten()
                .map(|compiled| (compiled, key_mode(key.form, context.collation)))
        })
        .collect::<Option<Vec<_>>>()?;
    if plan
        .residual
        .as_ref()
        .is_some_and(|residual| !outer_columns_resolve(residual, context.columns))
    {
        return None;
    }
    let answer = match &plan.answer {
        Answer::Exists => BuiltAnswer::Exists,
        Answer::Scalar {
            projection,
            first,
            nested: false,
        } => BuiltAnswer::Scalar {
            projection: CompiledExpr::compile(projection, &plan.layout, context.collation).ok()?,
            first: *first,
        },
        Answer::Scalar {
            projection,
            first,
            nested: true,
        } => {
            // A nested subquery names the inner row's columns as outer
            // ones. Were the operator's own input to carry a column of the
            // same identity, which of the two a name means could not be
            // told apart here.
            if plan.layout.iter().any(|inner| {
                context
                    .columns
                    .iter()
                    .any(|outer| same_column(outer, inner))
            }) {
                return None;
            }
            BuiltAnswer::Nested {
                projection: projection.clone(),
                first: *first,
                memo: Some(Box::new(DependentMemo::for_expressions(std::iter::once(
                    projection,
                )))),
                checked: false,
            }
        }
    };
    Some((outer_keys, answer))
}

/// Reads the filtered inner table and indexes it. `None` when anything
/// about it says the per-row path should keep answering.
pub(super) fn build(plan: &IndexPlan, context: &DependentRow<'_>) -> Option<SubqueryIndex> {
    let (outer_keys, answer) = compile_for(plan, context)?;
    let modes = outer_keys.iter().map(|(_, mode)| *mode).collect::<Vec<_>>();
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(plan.materialize.clone())),
        context.collation,
    )
    .ok()?;
    let limit = dependent_subquery_memory_limit(context.memory, context.batch).ok()?;
    let mut execution = Execution::start_with_deadline(
        physical,
        context.provider,
        limit,
        context.memory.deadline,
        context.collation,
    )
    .ok()?;

    let mut index = SubqueryIndex {
        batches: Vec::new(),
        rows: HashMap::new(),
        layout: plan.layout.clone(),
        outer_keys,
        residual: plan.residual.clone(),
        answer,
        reserved: 0,
        probe: Vec::with_capacity(plan.keys.len()),
    };
    let key_bytes = size_of::<Vec<JoinHashKey>>()
        + size_of::<Vec<(u32, u32)>>()
        + plan.keys.len() * size_of::<JoinHashKey>()
        + super::HASH_ENTRY_OVERHEAD;
    let mut indexed = 0_usize;
    let budget = context.memory.remaining() / MEMORY_SHARE_DIVISOR;
    let outcome = (|| -> Option<()> {
        while let Some(batch) = execution.next_batch().ok()? {
            let batch_number = u32::try_from(index.batches.len()).ok()?;
            let mut charge = batch.estimated_bytes();
            let mut key = Vec::with_capacity(plan.keys.len());
            'rows: for row in batch.selection().selected_rows() {
                key.clear();
                for (planned, mode) in plan.keys.iter().zip(&modes) {
                    let value = batch.column(planned.inner)?.value(row)?.clone();
                    match key_part(value, *mode) {
                        KeyValue::Key(part) => key.push(part),
                        KeyValue::Null => continue 'rows,
                        KeyValue::Other => return None,
                    }
                }
                indexed += 1;
                if indexed > MAX_INDEX_ROWS || (plan.keys.is_empty() && indexed > MAX_KEYLESS_ROWS)
                {
                    return None;
                }
                let row = u32::try_from(row).ok()?;
                charge += size_of::<(u32, u32)>();
                if let Some(rows) = index.rows.get_mut(key.as_slice()) {
                    rows.push((batch_number, row));
                } else {
                    charge += key_bytes + key.iter().map(key_heap_bytes).sum::<usize>();
                    index.rows.insert(key.clone(), vec![(batch_number, row)]);
                }
            }
            if index.reserved.saturating_add(charge) > budget {
                return None;
            }
            context.memory.reserve(charge).ok()?;
            index.reserved += charge;
            index.batches.push(batch);
        }
        Some(())
    })();
    if outcome.is_none() {
        index.release(context.memory);
        return None;
    }
    Some(index)
}

/// Answers the subquery for the current outer row - `EXISTS` as a boolean,
/// a scalar subquery as its value - or `None` when this row's key is not of
/// the kind the index holds and the per-row path must answer it.
pub(super) fn answer(
    index: &mut SubqueryIndex,
    context: &DependentRow<'_>,
) -> Result<Option<Value>, ExecError> {
    let wanted = match index.answer {
        BuiltAnswer::Exists
        | BuiltAnswer::Scalar { first: true, .. }
        | BuiltAnswer::Nested { first: true, .. } => 1,
        BuiltAnswer::Scalar { first: false, .. } | BuiltAnswer::Nested { first: false, .. } => 2,
    };
    let Some(found) = qualifying_rows(index, context, wanted)? else {
        return Ok(None);
    };
    Ok(Some(match &mut index.answer {
        BuiltAnswer::Nested {
            projection,
            memo,
            checked,
            ..
        } => match found.as_slice() {
            [] => Value::Null,
            [(batch, row)] => {
                let rows = &index.batches[*batch as usize];
                let row = *row as usize;
                if !*checked {
                    // Every outer column the projection names must be one
                    // of the inner row's; one further out is only the
                    // per-row path's to substitute.
                    let mut probe = projection.clone();
                    substitute_outer_expr(&mut probe, rows, row, &index.layout, &mut Vec::new())?;
                    if super::bound_expr_has_outer_refs(&probe) {
                        *memo = None;
                        return Ok(None);
                    }
                    *checked = true;
                }
                let Some(memo) = memo.as_deref_mut() else {
                    return Ok(None);
                };
                let inner = DependentRow {
                    batch: rows,
                    row,
                    columns: &index.layout,
                    provider: context.provider,
                    memory: context.memory,
                    collation: context.collation,
                    ahead: &[],
                };
                let mut resolved = projection.clone();
                memo.begin_row();
                super::resolve_dependent_expr_subqueries(&mut resolved, &inner, memo)?;
                CompiledExpr::compile(&resolved, &index.layout, context.collation)?
                    .evaluate(rows, row)?
            }
            _ => return Err(ExecError::ScalarSubqueryRows { rows: found.len() }),
        },
        BuiltAnswer::Exists => Value::Boolean(!found.is_empty()),
        BuiltAnswer::Scalar { projection, .. } => match found.as_slice() {
            [] => Value::Null,
            [(batch, row)] => {
                projection.evaluate(&index.batches[*batch as usize], *row as usize)?
            }
            _ => return Err(ExecError::ScalarSubqueryRows { rows: found.len() }),
        },
    }))
}

/// Up to `wanted` qualifying candidates for the current outer row, in scan
/// order, or `None` for the per-row path.
fn qualifying_rows(
    index: &mut SubqueryIndex,
    context: &DependentRow<'_>,
    wanted: usize,
) -> Result<Option<Vec<(u32, u32)>>, ExecError> {
    index.probe.clear();
    for (key, mode) in &index.outer_keys {
        match key_part(key.evaluate(context.batch, context.row)?, *mode) {
            KeyValue::Key(part) => index.probe.push(part),
            KeyValue::Null => return Ok(Some(Vec::new())),
            KeyValue::Other => return Ok(None),
        }
    }
    let Some(candidates) = index.rows.get(index.probe.as_slice()) else {
        return Ok(Some(Vec::new()));
    };
    let Some(residual) = &index.residual else {
        return Ok(Some(candidates.iter().take(wanted).copied().collect()));
    };
    let mut residual = residual.clone();
    let mut substituted = Vec::new();
    substitute_outer_expr(
        &mut residual,
        context.batch,
        context.row,
        context.columns,
        &mut substituted,
    )?;
    let residual = CompiledExpr::compile(&residual, &index.layout, context.collation)?;
    let mut found = Vec::new();
    for (evaluated, &(batch, row)) in candidates.iter().enumerate() {
        if evaluated % INTERRUPTION_STRIDE == INTERRUPTION_STRIDE - 1 {
            context.memory.check_interruption()?;
        }
        let rows = &index.batches[batch as usize];
        if predicate_truth(&residual.evaluate(rows, row as usize)?)? {
            found.push((batch, row));
            if found.len() == wanted {
                break;
            }
        }
    }
    Ok(Some(found))
}

enum KeyValue {
    Key(JoinHashKey),
    Null,
    Other,
}

/// One side's value as the key the other side's is compared with. Only the
/// shapes the plan promised become keys: an integer under an integer key,
/// text under a text key. Anything else is the per-row path's to answer.
fn key_part(value: Value, mode: JoinKeyMode) -> KeyValue {
    let expected = match (&value, mode.form) {
        (Value::Null, _) => return KeyValue::Null,
        (Value::Int64(_) | Value::UInt64(_), KeyForm::Integer)
        | (Value::Utf8(_), KeyForm::CollatedText(_)) => true,
        _ => false,
    };
    if !expected {
        return KeyValue::Other;
    }
    match normalized_join_key(value, mode) {
        Ok(Some(
            key @ (JoinHashKey::CollatedText(_)
            | JoinHashKey::NegativeInteger(_)
            | JoinHashKey::NonNegativeInteger(_)),
        )) => KeyValue::Key(key),
        Ok(None) => KeyValue::Null,
        _ => KeyValue::Other,
    }
}

fn key_heap_bytes(key: &JoinHashKey) -> usize {
    match key {
        JoinHashKey::CollatedText(bytes) => bytes.len(),
        _ => 0,
    }
}

const fn is_integer(data_type: DataType) -> bool {
    matches!(
        data_type,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    )
}

/// The inner column under an optional explicit `COLLATE`.
fn inner_column(expression: &BoundExpr) -> Option<&BoundColumn> {
    match &expression.kind {
        BoundExprKind::Column(column) if !column.outer => Some(column),
        BoundExprKind::Scalar {
            function: ScalarFunction::Collate { .. },
            args,
        } if args.len() == 1 => inner_column(&args[0]),
        _ => None,
    }
}

/// `inner_column = outer_expression`, either way round, with the key form
/// its two sides compare under - or `None` when the equality is not one the
/// index can key exactly.
fn equality_key(conjunct: &BoundExpr) -> Option<(&BoundColumn, &BoundExpr, PlannedForm)> {
    let BoundExprKind::Binary {
        op: BinaryOp::Equal,
        left,
        right,
    } = &conjunct.kind
    else {
        return None;
    };
    let (inner, inner_side, outer) = if let Some(inner) = inner_column(left) {
        (inner, &**left, &**right)
    } else {
        (inner_column(right)?, &**right, &**left)
    };
    let mut inner_columns = Vec::new();
    let mut has_outer = false;
    columns_of(outer, &mut inner_columns, &mut has_outer);
    if !inner_columns.is_empty() || !has_outer {
        return None;
    }
    let (inner_type, outer_type) = (inner_side.data_type?, outer.data_type?);
    if is_integer(inner.data_type) && is_integer(inner_type) && is_integer(outer_type) {
        return Some((inner, outer, PlannedForm::Integer));
    }
    let text = inner.data_type == DataType::Utf8
        && inner_type == DataType::Utf8
        && outer_type == DataType::Utf8
        && inner.enum_labels.is_none()
        && !reads_enum(outer);
    if !text {
        return None;
    }
    // The per-row path compiles this equality after the outer value became
    // a literal, which carries no collation; the index keys it under the
    // same one. Where the two would differ the equality stays a residual.
    let collation = conjunct.text_collation()?;
    let mut substituted = conjunct.clone();
    literalize_outer(&mut substituted);
    (substituted.text_collation() == Some(collation)).then_some((
        inner,
        outer,
        PlannedForm::Text(Some(collation)),
    ))
}

/// Replaces every outer column with a text literal, as substitution would.
fn literalize_outer(expression: &mut BoundExpr) {
    match &mut expression.kind {
        BoundExprKind::Column(column) if column.outer => {
            expression.kind = BoundExprKind::Literal(Value::Utf8(String::new()));
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            literalize_outer(expr);
        }
        BoundExprKind::Binary { left, right, .. } => {
            literalize_outer(left);
            literalize_outer(right);
        }
        BoundExprKind::Scalar { args, .. } => args.iter_mut().for_each(literalize_outer),
        _ => {}
    }
}

fn reads_enum(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Column(column) => column.enum_labels.is_some(),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => reads_enum(expr),
        BoundExprKind::Binary { left, right, .. } => reads_enum(left) || reads_enum(right),
        BoundExprKind::Scalar { args, .. } => args.iter().any(reads_enum),
        _ => false,
    }
}

fn flatten_and<'a>(expression: &'a BoundExpr, conjuncts: &mut Vec<&'a BoundExpr>) {
    if let BoundExprKind::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = &expression.kind
    {
        flatten_and(left, conjuncts);
        flatten_and(right, conjuncts);
    } else {
        conjuncts.push(expression);
    }
}

/// Whether everything in `expression` that is not a plain node is a
/// subquery: no aggregate or window slot of the inner query itself.
fn holds_only_subqueries(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Column(_)
        | BoundExprKind::Literal(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. } => true,
        BoundExprKind::InSubquery { expr, .. } => holds_only_subqueries(expr),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            holds_only_subqueries(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            holds_only_subqueries(left) && holds_only_subqueries(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(holds_only_subqueries),
        BoundExprKind::PreparedIn { .. }
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => false,
    }
}

fn conjoin(conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    conjuncts.into_iter().reduce(|left, right| BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: left.nullable || right.nullable,
        kind: BoundExprKind::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
    })
}

/// Whether an expression is built only from the node kinds the index
/// evaluates row by row: no subqueries, no aggregate or window slots.
fn plain_expression(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::Column(_) | BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            plain_expression(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            plain_expression(left) && plain_expression(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().all(plain_expression),
        BoundExprKind::PreparedIn { .. }
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. }
        | BoundExprKind::InSubquery { .. }
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_) => false,
    }
}

/// Collects the inner columns an expression reads and whether it reads any
/// outer one. Only called on `plain_expression`s.
fn columns_of<'a>(
    expression: &'a BoundExpr,
    inner: &mut Vec<&'a BoundColumn>,
    has_outer: &mut bool,
) {
    match &expression.kind {
        BoundExprKind::Column(column) if column.outer => *has_outer = true,
        BoundExprKind::Column(column) => inner.push(column),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            columns_of(expr, inner, has_outer);
        }
        BoundExprKind::Binary { left, right, .. } => {
            columns_of(left, inner, has_outer);
            columns_of(right, inner, has_outer);
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                columns_of(argument, inner, has_outer);
            }
        }
        _ => {}
    }
}

/// Whether every outer column `expression` reads is one of the operator's
/// input columns. One that is not belongs to a scope further out, which
/// only the per-row path substitutes.
fn outer_columns_resolve(expression: &BoundExpr, columns: &[BoundColumn]) -> bool {
    match &expression.kind {
        BoundExprKind::Column(column) if column.outer => columns
            .iter()
            .any(|candidate| same_column(candidate, column)),
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            outer_columns_resolve(expr, columns)
        }
        BoundExprKind::Binary { left, right, .. } => {
            outer_columns_resolve(left, columns) && outer_columns_resolve(right, columns)
        }
        BoundExprKind::Scalar { args, .. } => args
            .iter()
            .all(|argument| outer_columns_resolve(argument, columns)),
        _ => true,
    }
}

fn same_column(left: &BoundColumn, right: &BoundColumn) -> bool {
    left.database_id == right.database_id
        && left.table_id == right.table_id
        && left.column_id == right.column_id
        && left
            .relation_name
            .eq_ignore_ascii_case(&right.relation_name)
}

fn belongs_to(column: &BoundColumn, table: &pintail_sql::BoundTable) -> bool {
    column.database_id == table.database_id
        && column.table_id == table.table_id
        && column
            .relation_name
            .eq_ignore_ascii_case(&table.relation_name)
}
