//! Counts over a join on an inequality, taken without pairing the rows.
//!
//! `SELECT a.id, COUNT(b.id) FROM t a JOIN t b ON b.id < a.id GROUP BY
//! a.id` pairs every row with half the table before it counts anything:
//! over a hundred thousand rows that is five billion pairs, held as rows.
//! The count needs none of them. How many right rows a left row pairs with
//! is a number read off the right side once it is in order - the rows its
//! bounds reach - and for `<>` it is every right row with a value, less
//! the ones that carry the left row's own.
//!
//! So where an aggregate of nothing but `COUNT`s sits on such a join and
//! groups by the left side alone, the join is replaced by its left input
//! with one more column per counted thing: how many right rows each left
//! row pairs with. `COUNT(*)` and `COUNT(right_expression)` become the sum
//! of those columns over the group, which is the same number.
//!
//! What it must never change, and how each is kept:
//!
//! - **Which pairs the join accepts.** Only a condition that is the
//!   inequalities the nested loop already narrows by, or one `<>`, with
//!   conjuncts that read a single side beside them. The inequalities are
//!   counted through the join's own sorted index (`RangeCounter`), so a
//!   value pairs here with exactly the rows it is searched against there;
//!   `<>` is counted with the keys an equality join hashes by. A conjunct
//!   on the right side alone removes right rows before they are counted;
//!   one on the left side alone leaves a left row with no pair.
//! - **NULL.** A NULL on either side of the comparison pairs with nothing.
//!   `COUNT(expression)` counts the paired right rows whose expression is
//!   not NULL: they are kept in a count of their own.
//! - **The join's kind.** An inner join drops a left row with no pair; a
//!   left join keeps it, as one row whose right side is NULL - one for
//!   `COUNT(*)`, none for a count of a right expression.
//! - **No rows.** With no GROUP BY the answer is one row whatever the
//!   input, zero included, so the counts are added here and handed on as
//!   one row.
//! - **Anything else** - another condition, a value outside the ordered
//!   domain, more rows than are worth holding - leaves the join as it was.

use std::collections::HashMap;

use pintail_catalog::{DatabaseId, TableId};
use pintail_sql::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundJoinKind,
};
use pintail_types::{DataType, Value};

use super::join::{JoinHashKey, RangeCounter, RangeKey, normalized_join_key};
use super::{
    ExecError, JoinKeyMode, MemoryTracker, PhysicalPlan, PullOperator, ScanProvider, batch_row,
    build_operator, estimated_row_payload_bytes, hash_join_key_mode, pair_key_collation,
};
use crate::RecordBatch;
use crate::collation::Collation;
use crate::expression::{CompiledExpr, predicate_truth};

/// Rows either side may hold before the join is left as it was.
const MAX_SIDE_ROWS: usize = 4 << 20;

/// Right rows of an equality join read to decide whether it is counted: a
/// join that keeps its own path reads them a second time.
const EQUAL_MAX_RIGHT_ROWS: u64 = 1 << 20;

/// Right rows per key from which an equality join is counted. Under it the
/// join's own paths, which fold a probe row's few pairs in parallel, are
/// the better answer.
const EQUAL_ROWS_PER_KEY: u64 = 256;

/// The aggregate's new input: its rows, their layout, and the aggregates
/// rewritten to add the counts up.
pub(super) struct Counted {
    pub(super) input: PullOperator,
    pub(super) columns: Vec<BoundColumn>,
    pub(super) aggregates: Vec<BoundAggregate>,
}

fn same_column(left: &BoundColumn, right: &BoundColumn) -> bool {
    left.database_id == right.database_id
        && left.table_id == right.table_id
        && left.column_id == right.column_id
        && left
            .relation_name
            .eq_ignore_ascii_case(&right.relation_name)
}

/// Whether `expression` is plain and every column it reads is one of
/// `columns`.
fn reads_only(expression: &BoundExpr, columns: &[BoundColumn]) -> bool {
    match &expression.kind {
        BoundExprKind::Column(column) => {
            !column.outer
                && columns
                    .iter()
                    .any(|candidate| same_column(candidate, column))
        }
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            reads_only(expr, columns)
        }
        BoundExprKind::Binary { left, right, .. } => {
            reads_only(left, columns) && reads_only(right, columns)
        }
        BoundExprKind::Scalar { args, .. } => {
            args.iter().all(|argument| reads_only(argument, columns))
        }
        _ => false,
    }
}

fn flatten_and<'a>(expression: &'a BoundExpr, conjuncts: &mut Vec<&'a BoundExpr>) {
    match &expression.kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            flatten_and(left, conjuncts);
            flatten_and(right, conjuncts);
        }
        _ => conjuncts.push(expression),
    }
}

/// How the right rows one left row pairs with are counted.
enum Pairing {
    /// The right rows the left row's bounds reach.
    Range(RangeCounter),
    /// Every right row with a value, less those that carry the left row's.
    Unequal {
        left: CompiledExpr,
        right: CompiledExpr,
        mode: JoinKeyMode,
        rows: u64,
        by_key: HashMap<JoinHashKey, u64>,
    },
    /// The right rows that carry the left row's keys.
    Equal {
        left: Vec<(CompiledExpr, JoinKeyMode)>,
        right: Vec<(CompiledExpr, JoinKeyMode)>,
        rows: u64,
        by_key: HashMap<JoinHashKey, u64>,
    },
}

/// One row's keys as one key, `None` when a key is NULL under an equality
/// NULL never satisfies.
fn equal_key(
    parts: &[(CompiledExpr, JoinKeyMode)],
    batch: &RecordBatch,
    row: usize,
) -> Result<Option<JoinHashKey>, ExecError> {
    let mut keys = Vec::with_capacity(parts.len());
    for (expression, mode) in parts {
        let Some(key) = normalized_join_key(expression.evaluate(batch, row)?, *mode)? else {
            return Ok(None);
        };
        keys.push(key);
    }
    Ok(Some(JoinHashKey::Composite(keys)))
}

impl Pairing {
    fn push(&mut self, batch: &RecordBatch, row: usize) -> Result<bool, ExecError> {
        match self {
            Self::Range(counter) => counter.push(batch, row),
            Self::Equal {
                right,
                rows,
                by_key,
                ..
            } => {
                if let Some(key) = equal_key(right, batch, row)? {
                    *rows += 1;
                    *by_key.entry(key).or_default() += 1;
                }
                Ok(true)
            }
            Self::Unequal {
                right,
                mode,
                rows,
                by_key,
                ..
            } => {
                if let Some(key) = normalized_join_key(right.evaluate(batch, row)?, *mode)? {
                    *rows += 1;
                    *by_key.entry(key).or_default() += 1;
                }
                Ok(true)
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Range(counter) => counter.len(),
            Self::Unequal { by_key, .. } | Self::Equal { by_key, .. } => by_key.len(),
        }
    }

    fn count(&self, batch: &RecordBatch, row: usize) -> Result<Option<u64>, ExecError> {
        match self {
            Self::Range(counter) => counter.count(batch, row),
            Self::Equal { left, by_key, .. } => Ok(Some(
                equal_key(left, batch, row)?
                    .and_then(|key| by_key.get(&key).copied())
                    .unwrap_or(0),
            )),
            Self::Unequal {
                left,
                mode,
                rows,
                by_key,
                ..
            } => Ok(Some(
                match normalized_join_key(left.evaluate(batch, row)?, *mode)? {
                    None => 0,
                    Some(key) => rows - by_key.get(&key).copied().unwrap_or(0),
                },
            )),
        }
    }
}

/// The pairing a join condition's two-sided conjuncts ask for, built
/// afresh for each thing counted.
struct PairingPlan<'a> {
    ranges: &'a [RangeKey],
    unequal: Option<(&'a BoundExpr, &'a BoundExpr)>,
    /// A hash join's keys, left then right, and whether each is `<=>`.
    equal: &'a [(&'a BoundExpr, &'a BoundExpr, bool)],
    left_columns: &'a [BoundColumn],
    right_columns: &'a [BoundColumn],
    collation: Collation,
}

impl PairingPlan<'_> {
    fn build(&self) -> Result<Option<Pairing>, ExecError> {
        if !self.equal.is_empty() {
            let mut left_keys = Vec::with_capacity(self.equal.len());
            let mut right_keys = Vec::with_capacity(self.equal.len());
            for (left, right, null_safe) in self.equal {
                let Some(mode) = hash_join_key_mode(
                    left.data_type,
                    right.data_type,
                    pair_key_collation(left, right, self.collation),
                ) else {
                    return Ok(None);
                };
                let mode = JoinKeyMode {
                    null_safe: *null_safe,
                    ..mode
                };
                left_keys.push((
                    CompiledExpr::compile(left, self.left_columns, self.collation)?,
                    mode,
                ));
                right_keys.push((
                    CompiledExpr::compile(right, self.right_columns, self.collation)?,
                    mode,
                ));
            }
            return Ok(Some(Pairing::Equal {
                left: left_keys,
                right: right_keys,
                rows: 0,
                by_key: HashMap::new(),
            }));
        }
        if let Some((left, right)) = self.unequal {
            let Some(mode) = hash_join_key_mode(
                left.data_type,
                right.data_type,
                pair_key_collation(left, right, self.collation),
            ) else {
                return Ok(None);
            };
            return Ok(Some(Pairing::Unequal {
                left: CompiledExpr::compile(left, self.left_columns, self.collation)?,
                right: CompiledExpr::compile(right, self.right_columns, self.collation)?,
                mode,
                rows: 0,
                by_key: HashMap::new(),
            }));
        }
        Ok(RangeCounter::compile(
            self.ranges,
            self.left_columns,
            self.right_columns,
            self.collation,
        )?
        .map(Pairing::Range))
    }
}

fn count_column(index: usize) -> BoundColumn {
    BoundColumn {
        database_id: DatabaseId::new(u64::MAX),
        table_id: TableId::new(u64::MAX - 2),
        column_id: u32::try_from(index).unwrap_or(u32::MAX),
        relation_name: "<paired>".to_owned(),
        name: format!("<paired-{index}>"),
        data_type: DataType::UInt64,
        nullable: false,
        collation: None,
        enum_labels: None,
        geometry: false,
        timestamp: false,
        binary_width: None,
        bit_width: None,
        float_decimals: None,
        outer: false,
        using_shadowed: false,
    }
}

/// The aggregate's input with the join counted rather than run, when
/// `input` is a join this module counts and the aggregate only counts.
/// `None` leaves the plan to be built as it stands.
#[allow(clippy::too_many_lines)] // one shape check and one pass over each side
pub(super) fn plan(
    input: &PhysicalPlan,
    group_by: &[BoundExpr],
    aggregates: &[BoundAggregate],
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Option<Counted>, ExecError> {
    let (left, right, kind, ranges, condition, equal, rows_per_key) = match input {
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            kind: kind @ (BoundJoinKind::Inner | BoundJoinKind::Left),
            keys,
            ranges,
            condition,
        } if keys.is_empty() => (
            left,
            right,
            *kind,
            ranges.as_slice(),
            Some(condition),
            Vec::new(),
            0,
        ),
        // An equality join grouped by its left side: counted only where
        // each key holds so many right rows that pairing them is the cost,
        // and only over a right side small enough to read again when the
        // join keeps its own path.
        PhysicalPlan::HashJoin {
            left,
            right,
            kind: kind @ (BoundJoinKind::Inner | BoundJoinKind::Left),
            left_key,
            extra_keys,
            null_safe,
            right_key,
            build_estimate: Some(build_rows),
            residual: None,
            ..
        } if !group_by.is_empty() && *build_rows <= EQUAL_MAX_RIGHT_ROWS => {
            let null_safe_at = |key: usize| null_safe.get(key).copied().unwrap_or(false);
            let mut equal = vec![(left_key, right_key, null_safe_at(0))];
            equal.extend(
                extra_keys
                    .iter()
                    .enumerate()
                    .map(|(index, (left, right))| (left, right, null_safe_at(index + 1))),
            );
            (left, right, *kind, &[][..], None, equal, EQUAL_ROWS_PER_KEY)
        }
        _ => return Ok(None),
    };
    if aggregates.is_empty()
        || aggregates.iter().any(|aggregate| {
            aggregate.function != AggregateFunction::Count
                || aggregate.distinct
                || !aggregate.order_within.is_empty()
        })
    {
        return Ok(None);
    }
    let (mut left, left_columns) = build_operator((**left).clone(), provider, memory, collation)?;
    let (mut right, right_columns) =
        build_operator((**right).clone(), provider, memory, collation)?;
    // A column both sides carry would name either: the sides of a
    // self-join differ by alias, and anything else is not counted here.
    if left_columns
        .iter()
        .any(|column| right_columns.iter().any(|other| same_column(column, other)))
    {
        return Ok(None);
    }
    if !group_by
        .iter()
        .all(|expression| reads_only(expression, &left_columns))
    {
        return Ok(None);
    }
    // What each aggregate counts: every paired right row, or those whose
    // expression is not NULL. Every paired row is counted first whether an
    // aggregate asks or not: it says which left rows pair at all.
    let mut counted: Vec<Option<&BoundExpr>> = vec![None];
    let mut slots = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        let argument = aggregate.expr.as_ref();
        if argument.is_some_and(|expression| {
            !reads_only(expression, &right_columns) || reads_only(expression, &[])
        }) {
            return Ok(None);
        }
        let slot = counted
            .iter()
            .position(|seen| *seen == argument)
            .unwrap_or_else(|| {
                counted.push(argument);
                counted.len() - 1
            });
        slots.push(slot);
    }
    let mut conjuncts = Vec::new();
    if let Some(condition) = condition {
        flatten_and(condition, &mut conjuncts);
    }
    let mut left_only = Vec::new();
    let mut right_only = Vec::new();
    let mut two_sided = Vec::new();
    for conjunct in conjuncts {
        if reads_only(conjunct, &left_columns) {
            left_only.push(CompiledExpr::compile(conjunct, &left_columns, collation)?);
        } else if reads_only(conjunct, &right_columns) {
            right_only.push(CompiledExpr::compile(conjunct, &right_columns, collation)?);
        } else {
            two_sided.push(conjunct);
        }
    }
    let unequal = match (ranges.is_empty(), two_sided.as_slice()) {
        (true, []) if !equal.is_empty() => None,
        (true, [conjunct]) => {
            let BoundExprKind::Binary {
                op: BinaryOp::NotEqual,
                left: first,
                right: second,
            } = &conjunct.kind
            else {
                return Ok(None);
            };
            if reads_only(first, &left_columns) && reads_only(second, &right_columns) {
                Some((&**first, &**second))
            } else if reads_only(second, &left_columns) && reads_only(first, &right_columns) {
                Some((&**second, &**first))
            } else {
                return Ok(None);
            }
        }
        // Each two-sided conjunct is one of the inequalities the join
        // narrows by, written either way round, and there is no other.
        (false, conjuncts)
            if conjuncts.len() == ranges.len()
                && conjuncts.iter().all(|conjunct| {
                    let BoundExprKind::Binary { op, left, right } = &conjunct.kind else {
                        return false;
                    };
                    let turned = match op {
                        BinaryOp::Less => BinaryOp::Greater,
                        BinaryOp::LessOrEqual => BinaryOp::GreaterOrEqual,
                        BinaryOp::Greater => BinaryOp::Less,
                        BinaryOp::GreaterOrEqual => BinaryOp::LessOrEqual,
                        _ => return false,
                    };
                    ranges.iter().any(|range| {
                        (range.op == *op && range.left == **left && range.right == **right)
                            || (range.op == turned
                                && range.left == **right
                                && range.right == **left)
                    })
                }) =>
        {
            None
        }
        _ => return Ok(None),
    };
    let pairing = PairingPlan {
        ranges,
        unequal,
        equal: &equal,
        left_columns: &left_columns,
        right_columns: &right_columns,
        collation,
    };
    let mut pairings = Vec::with_capacity(counted.len());
    let mut arguments = Vec::with_capacity(counted.len());
    for argument in &counted {
        let Some(built) = pairing.build()? else {
            return Ok(None);
        };
        pairings.push(built);
        arguments.push(
            argument
                .map(|expression| CompiledExpr::compile(expression, &right_columns, collation))
                .transpose()?,
        );
    }
    while let Some(batch) = right.next_batch(memory)? {
        memory.check_interruption()?;
        'rows: for row in batch.selection().selected_rows() {
            for conjunct in &right_only {
                if !predicate_truth(&conjunct.evaluate(&batch, row)?)? {
                    continue 'rows;
                }
            }
            for (pairing, argument) in pairings.iter_mut().zip(&arguments) {
                if let Some(argument) = argument
                    && matches!(argument.evaluate(&batch, row)?, Value::Null)
                {
                    continue;
                }
                if !pairing.push(&batch, row)? {
                    return Ok(None);
                }
            }
        }
        if pairings.iter().any(|pairing| pairing.len() > MAX_SIDE_ROWS) {
            return Ok(None);
        }
    }
    if let Some(Pairing::Equal { rows, by_key, .. }) = pairings.first()
        && *rows < rows_per_key.saturating_mul(by_key.len() as u64)
    {
        return Ok(None);
    }
    for pairing in &mut pairings {
        if let Pairing::Range(counter) = pairing {
            counter.seal();
        }
    }
    let grouped = !group_by.is_empty();
    let mut rows = Vec::new();
    let mut totals = vec![0_u64; pairings.len()];
    let mut reserved = 0_usize;
    let mut pairs = 0_u64;
    while let Some(batch) = left.next_batch(memory)? {
        memory.check_interruption()?;
        'rows: for row in batch.selection().selected_rows() {
            let mut reaches = true;
            for conjunct in &left_only {
                if !predicate_truth(&conjunct.evaluate(&batch, row)?)? {
                    reaches = false;
                    break;
                }
            }
            let mut counts = Vec::with_capacity(pairings.len());
            for pairing in &pairings {
                counts.push(if reaches {
                    let Some(count) = pairing.count(&batch, row)? else {
                        memory.release(reserved);
                        return Ok(None);
                    };
                    count
                } else {
                    0
                });
            }
            pairs = pairs.saturating_add(counts[0]);
            if counts[0] == 0 {
                if kind == BoundJoinKind::Inner {
                    continue 'rows;
                }
                // A left join's unpaired row is one row with a NULL right
                // side: one for COUNT(*), none for any other count.
                counts[0] = 1;
            }
            if grouped {
                let mut values = batch_row(&batch, row)?;
                values.extend(counts.into_iter().map(Value::UInt64));
                let bytes = estimated_row_payload_bytes(&values);
                memory.reserve(bytes)?;
                reserved = reserved.saturating_add(bytes);
                rows.push(values);
                if rows.len() > MAX_SIDE_ROWS {
                    memory.release(reserved);
                    return Ok(None);
                }
            } else {
                for (total, count) in totals.iter_mut().zip(counts) {
                    *total = total.checked_add(count).ok_or(ExecError::NumericOverflow)?;
                }
            }
        }
    }
    crate::counters::count(|counters| {
        counters.join_pairs_counted = counters.join_pairs_counted.saturating_add(pairs);
    });
    let mut columns = if grouped {
        left_columns
    } else {
        rows.push(totals.into_iter().map(Value::UInt64).collect());
        Vec::new()
    };
    let first_count = columns.len();
    columns.extend((0..pairings.len()).map(count_column));
    let aggregates = aggregates
        .iter()
        .zip(slots)
        .map(|(aggregate, slot)| BoundAggregate {
            function: AggregateFunction::Sum,
            expr: Some(BoundExpr {
                data_type: Some(DataType::UInt64),
                nullable: false,
                kind: BoundExprKind::Column(columns[first_count + slot].clone()),
            }),
            ..aggregate.clone()
        })
        .collect();
    Ok(Some(Counted {
        input: PullOperator::Rows {
            rows,
            cursor: 0,
            column_types: columns.iter().map(|column| column.data_type).collect(),
        },
        columns,
        aggregates,
    }))
}
