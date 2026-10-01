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
    Scalar { projection: BoundExpr, first: bool },
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

/// Decides, once per slot, whether `query` has a shape the index answers.
pub(super) fn plan(query: &BoundQuery, form: SubqueryForm) -> IndexState {
    analyse(query, form).map_or(IndexState::Declined, |(plan, estimated_rows)| {
        IndexState::Pending {
            plan: Box::new(plan),
            remaining: estimated_rows.map_or(MAX_WAITED_EXECUTIONS, |rows| {
                (rows / ROWS_PER_WAITED_EXECUTION).min(MAX_WAITED_EXECUTIONS)
            }),
        }
    })
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
            if !plain_expression(&projection.expr) {
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
                })
        }
    }
}

fn analyse(query: &BoundQuery, form: SubqueryForm) -> Option<(IndexPlan, Option<u64>)> {
    let [source] = query.from.as_slice() else {
        return None;
    };
    if !source.joins.is_empty()
        || source.base.input.is_some()
        || !query.group_by.is_empty()
        || !query.aggregates.is_empty()
        || !query.windows.is_empty()
        || query.having.is_some()
        || !query.union_all.is_empty()
        || !query.set_ops.is_empty()
        || query.recursive.is_some()
    {
        return None;
    }
    let answer = answer_of(query, form)?;
    let base = &source.base;
    if base
        .estimated_rows
        .or(base.row_count)
        .is_some_and(|rows| rows > MAX_INDEX_ROWS as u64)
    {
        return None;
    }
    let filter = query.filter.as_ref()?;
    let mut conjuncts = Vec::new();
    flatten_and(filter, &mut conjuncts);

    let mut uncorrelated = Vec::new();
    let mut keys = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if !plain_expression(conjunct) {
            return None;
        }
        let mut inner_columns = Vec::new();
        let mut has_outer = false;
        columns_of(conjunct, &mut inner_columns, &mut has_outer);
        if inner_columns.iter().any(|column| !belongs_to(column, base)) {
            return None;
        }
        if !has_outer {
            uncorrelated.push(conjunct.clone());
        } else if let Some(key) = equality_key(conjunct) {
            keys.push(key);
        } else {
            residual.push(conjunct.clone());
        }
    }
    if keys.is_empty() {
        return None;
    }

    let mut layout: Vec<BoundColumn> = Vec::new();
    let mut add_column = |column: &BoundColumn| {
        if let Some(position) = layout.iter().position(|seen| same_column(seen, column)) {
            position
        } else {
            layout.push(column.clone());
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
    if let Answer::Scalar { projection, .. } = &answer {
        read.push(projection);
    }
    for expression in read {
        let mut inner_columns = Vec::new();
        let mut has_outer = false;
        columns_of(expression, &mut inner_columns, &mut has_outer);
        for column in inner_columns {
            if !belongs_to(column, base) {
                return None;
            }
            add_column(column);
        }
    }

    let materialize = materialize_query(query, &layout, uncorrelated);
    Some((
        IndexPlan {
            materialize,
            layout,
            keys,
            residual: conjoin(residual),
            answer,
        },
        base.estimated_rows.or(base.row_count),
    ))
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
        Answer::Scalar { projection, first } => BuiltAnswer::Scalar {
            projection: CompiledExpr::compile(projection, &plan.layout, context.collation).ok()?,
            first: *first,
        },
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
                if indexed > MAX_INDEX_ROWS {
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
        BuiltAnswer::Exists | BuiltAnswer::Scalar { first: true, .. } => 1,
        BuiltAnswer::Scalar { first: false, .. } => 2,
    };
    let Some(found) = qualifying_rows(index, context, wanted)? else {
        return Ok(None);
    };
    Ok(Some(match &index.answer {
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
