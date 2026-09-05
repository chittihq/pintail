//! Statement-local memoization of dependent subquery results.
//!
//! The dependent path answers a correlated subquery once per outer row:
//! clone the inner query, substitute that row's outer values, plan, and
//! execute. When many outer rows share their correlation values the same
//! inner question is answered again and again - measured flat in the
//! repetition, one inner execution per outer row whatever the number of
//! distinct keys (`benchmark/evidence/dependent-subquery-ratio.md`).
//!
//! A memo keyed on the substituted outer tuple turns that into one inner
//! execution per distinct tuple. It lives in one operator and dies with
//! it, so nothing crosses a statement: the provider is pinned for the
//! statement, so every inner execution within it sees the same snapshot,
//! and the pinned statement clock has already folded `NOW()` and its
//! family to literals before the query reaches here.
//!
//! What the memo must never change, and how each is kept:
//!
//! - **Volatile inner queries** (`RAND()`, `UUID()`) are answered fresh
//!   every time: a query whose body holds one is marked unmemoizable when
//!   the operator is built, and the memo is never consulted for it.
//! - **NULL** is its own key: the substituted literal is the same `NULL`
//!   for every such row and the inner answer for it is deterministic.
//! - **Collation**: keys compare bytewise, never under the session
//!   collation. `'a'` and `'A'` are two entries even where they compare
//!   equal - a miss, never a wrong hit.
//! - **Cardinality errors** and every other failure are not memoized; the
//!   error propagates from the row that raised it, as it does today.
//! - **Short-circuit**: the memo sits inside resolution, so an `IF` or
//!   `COALESCE` branch that is not taken is neither executed nor cached.
//! - **Memory**: every entry is charged to the query's tracker. A charge
//!   the ceiling refuses drops the whole memo and disables it for the rest
//!   of the operator, so memoization can only ever remove work - a query
//!   that would have run without it still runs.

use std::collections::HashMap;
use std::mem::size_of;

use pintail_sql::{BoundExpr, BoundExprKind, BoundQuery, WindowFunction};
use pintail_types::Value;

use super::MemoryTracker;

/// Entries one memo holds at most. Above this the key set is doing no
/// sharing worth its bookkeeping, and the bound keeps the pathological
/// case - every tuple distinct, every result wide - from growing the memo
/// to the ceiling before the charge does.
const MAX_ENTRIES: usize = 65_536;

/// The identity of one subquery inside an operator's expression: its
/// position in a pre-order walk of the expression tree. Stable across rows
/// because every row resolves a clone of the same expression.
pub(super) type SubquerySlot = usize;

/// Materialized inner results for one operator, by subquery and by the
/// outer tuple substituted into it.
pub(crate) struct DependentMemo {
    entries: HashMap<(SubquerySlot, Vec<Value>), Vec<Value>>,
    /// Bytes reserved with the tracker for `entries`, returned on drop.
    reserved: usize,
    /// Per subquery slot, whether its body is free of volatile functions.
    /// Consulted before any lookup; a volatile query is never cached.
    memoizable: Vec<bool>,
    /// Set once a charge was refused or the entry cap was hit: from then on
    /// every lookup misses and nothing is inserted, and the operator runs
    /// exactly as it did before the memo existed.
    disabled: bool,
    /// The next slot handed out while walking a row's expression.
    cursor: SubquerySlot,
    hits: u64,
    misses: u64,
}

impl DependentMemo {
    /// A memo for one operator over `expression`, with each subquery in it
    /// classified by whether its body may be cached.
    pub(crate) fn for_expressions<'a>(expressions: impl Iterator<Item = &'a BoundExpr>) -> Self {
        let mut memoizable = Vec::new();
        for expression in expressions {
            classify_subqueries(expression, &mut memoizable);
        }
        Self {
            entries: HashMap::new(),
            reserved: 0,
            memoizable,
            disabled: false,
            cursor: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Starts a row: subquery slots are handed out again from the first.
    pub(crate) fn begin_row(&mut self) {
        self.cursor = 0;
    }

    /// Claims the next subquery slot in this row's walk.
    pub(super) fn next_slot(&mut self) -> SubquerySlot {
        let slot = self.cursor;
        self.cursor += 1;
        slot
    }

    /// Advances the slot cursor past every subquery in `expression`
    /// without touching any of them: a short-circuited branch is neither
    /// executed nor cached, but the slots after it must still line up.
    pub(super) fn skip_subqueries_in(&mut self, expression: &BoundExpr) {
        let mut skipped = Vec::new();
        classify_subqueries(expression, &mut skipped);
        self.cursor += skipped.len();
    }

    /// The cached result for `slot` under `key`, if there is one.
    pub(super) fn get(&mut self, slot: SubquerySlot, key: &[Value]) -> Option<Vec<Value>> {
        if self.disabled || !self.memoizable.get(slot).copied().unwrap_or(false) {
            return None;
        }
        // The lookup allocates its key only on the miss path below; a hit
        // is answered from a borrowed probe.
        let found = self.entries.get(&(slot, key.to_vec())).cloned();
        if found.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        found
    }

    /// Records `values` as the answer for `slot` under `key`, charging the
    /// tracker. A refused charge or a full memo disables it for the rest
    /// of the operator rather than failing the query.
    pub(super) fn insert(
        &mut self,
        memory: &MemoryTracker,
        slot: SubquerySlot,
        key: Vec<Value>,
        values: &[Value],
    ) {
        if self.disabled || !self.memoizable.get(slot).copied().unwrap_or(false) {
            return;
        }
        if self.entries.len() >= MAX_ENTRIES {
            self.disable(memory);
            return;
        }
        let bytes = entry_bytes(&key, values);
        if memory.reserve(bytes).is_err() {
            self.disable(memory);
            return;
        }
        self.reserved = self.reserved.saturating_add(bytes);
        self.entries.insert((slot, key), values.to_vec());
    }

    /// Drops every entry and returns its memory. The memo stays alive but
    /// answers nothing from here on.
    fn disable(&mut self, memory: &MemoryTracker) {
        self.entries.clear();
        memory.release(self.reserved);
        self.reserved = 0;
        self.disabled = true;
    }

    /// Returns the memo's memory to the tracker. Called by the operator
    /// that owns it once its rows are collected; not a `Drop` impl because
    /// the tracker is borrowed, not owned.
    pub(crate) fn finish(mut self, memory: &MemoryTracker) -> DependentMemoStats {
        memory.release(self.reserved);
        self.reserved = 0;
        DependentMemoStats {
            hits: self.hits,
            misses: self.misses,
            disabled: self.disabled,
        }
    }
}

/// What one operator's memo did, for the process counters.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DependentMemoStats {
    pub(super) hits: u64,
    pub(super) misses: u64,
    pub(super) disabled: bool,
}

/// Bytes one entry costs: both vectors' headers, every value's inline size
/// and heap, plus the hash-map slot they occupy.
fn entry_bytes(key: &[Value], values: &[Value]) -> usize {
    let value_bytes = |items: &[Value]| {
        items.iter().fold(size_of::<Vec<Value>>(), |total, value| {
            total
                .saturating_add(size_of::<Value>())
                .saturating_add(value.heap_bytes())
        })
    };
    value_bytes(key)
        .saturating_add(value_bytes(values))
        .saturating_add(size_of::<(SubquerySlot, Vec<Value>)>())
        .saturating_add(size_of::<u64>())
}

/// Walks `expression` in the same pre-order the resolver uses, pushing one
/// entry per subquery: whether its body is free of volatile functions.
///
/// The walk order here and in `resolve_dependent_expr_subqueries` must
/// agree, or a slot would name the wrong subquery; both visit subqueries
/// before recursing into their siblings and left before right.
fn classify_subqueries(expression: &BoundExpr, memoizable: &mut Vec<bool>) {
    match &expression.kind {
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            memoizable.push(!query_is_volatile(query));
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            // The resolver resolves the tested expression first, then the
            // membership query; slots follow that order.
            classify_subqueries(expr, memoizable);
            memoizable.push(!query_is_volatile(query));
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            classify_subqueries(expr, memoizable);
        }
        BoundExprKind::Binary { left, right, .. } => {
            classify_subqueries(left, memoizable);
            classify_subqueries(right, memoizable);
        }
        BoundExprKind::Scalar { args, .. } => {
            for argument in args {
                classify_subqueries(argument, memoizable);
            }
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => {}
    }
}

/// Whether any expression anywhere in the query can answer differently on
/// two evaluations of the same inputs. Errs toward "volatile" for any shape
/// it does not walk, so an unfamiliar construct is answered fresh.
fn query_is_volatile(query: &BoundQuery) -> bool {
    query
        .projection
        .iter()
        .any(|projection| expr_is_volatile(&projection.expr))
        || query.filter.as_ref().is_some_and(expr_is_volatile)
        || query.group_by.iter().any(expr_is_volatile)
        || query.having.as_ref().is_some_and(expr_is_volatile)
        || query.aggregates.iter().any(|aggregate| {
            aggregate.expr.as_ref().is_some_and(expr_is_volatile)
                || aggregate
                    .order_within
                    .iter()
                    .any(|(expression, _)| expr_is_volatile(expression))
        })
        || query.windows.iter().any(|window| {
            let function = match &window.function {
                WindowFunction::Aggregate(aggregate) => {
                    aggregate.expr.as_ref().is_some_and(expr_is_volatile)
                }
                WindowFunction::Offset { expr, default, .. } => {
                    expr_is_volatile(expr) || default.as_deref().is_some_and(expr_is_volatile)
                }
                WindowFunction::Extreme { expr, .. } => expr_is_volatile(expr),
                WindowFunction::RowNumber
                | WindowFunction::Rank
                | WindowFunction::DenseRank
                | WindowFunction::NTile(_) => false,
            };
            function
                || window.partition_by.iter().any(expr_is_volatile)
                || window.order_by.iter().any(|key| expr_is_volatile(&key.expr))
        })
        // ORDER BY keys index the projection, which is already walked.
        || query.from.iter().any(|source| {
            source
                .base
                .input
                .as_deref()
                .is_some_and(query_is_volatile)
                || source.joins.iter().any(|join| {
                    join.table.input.as_deref().is_some_and(query_is_volatile)
                        || join.condition.as_ref().is_some_and(expr_is_volatile)
                })
        })
        || query.union_all.iter().any(query_is_volatile)
        || query
            .set_ops
            .iter()
            .any(|(_, right)| query_is_volatile(right))
        || query
            .recursive
            .as_deref()
            .is_some_and(|recursive| query_is_volatile(&recursive.member))
}

/// The optimizer's notion of volatility, extended through nested
/// subqueries: a subquery is as volatile as its body.
fn expr_is_volatile(expression: &BoundExpr) -> bool {
    match &expression.kind {
        BoundExprKind::ScalarSubquery(query) | BoundExprKind::ExistsSubquery { query, .. } => {
            query_is_volatile(query)
        }
        BoundExprKind::InSubquery { expr, query, .. } => {
            expr_is_volatile(expr) || query_is_volatile(query)
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            expr_is_volatile(expr)
        }
        BoundExprKind::Binary { left, right, .. } => {
            expr_is_volatile(left) || expr_is_volatile(right)
        }
        BoundExprKind::Scalar { args, .. } => {
            crate::optimizer::is_volatile(expression) || args.iter().any(expr_is_volatile)
        }
        BoundExprKind::Column(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{DependentMemo, entry_bytes};
    use crate::execution::MemoryTracker;
    use pintail_types::Value;

    fn tracker(limit: usize) -> MemoryTracker {
        MemoryTracker::new(limit)
    }

    #[test]
    fn a_refused_charge_drops_the_memo_rather_than_failing_the_query() {
        let memory = tracker(entry_bytes(&[Value::UInt64(1)], &[Value::UInt64(1)]) * 2);
        let mut memo = DependentMemo {
            entries: std::collections::HashMap::new(),
            reserved: 0,
            memoizable: vec![true],
            disabled: false,
            cursor: 0,
            hits: 0,
            misses: 0,
        };
        memo.insert(&memory, 0, vec![Value::UInt64(1)], &[Value::UInt64(1)]);
        memo.insert(&memory, 0, vec![Value::UInt64(2)], &[Value::UInt64(2)]);
        assert_eq!(memo.entries.len(), 2);
        let used_before = memory.used();
        assert!(used_before > 0, "entries are charged");

        // The third entry does not fit: everything is released and the
        // memo goes quiet, and the query is not told.
        memo.insert(&memory, 0, vec![Value::UInt64(3)], &[Value::UInt64(3)]);
        assert!(memo.disabled);
        assert!(memo.entries.is_empty());
        assert_eq!(memory.used(), 0, "a dropped memo owes nothing");
        assert!(memo.get(0, &[Value::UInt64(1)]).is_none());
        let stats = memo.finish(&memory);
        assert!(stats.disabled);
    }

    #[test]
    fn null_and_bytewise_distinct_text_are_separate_keys() {
        let memory = tracker(1 << 20);
        let mut memo = DependentMemo {
            entries: std::collections::HashMap::new(),
            reserved: 0,
            memoizable: vec![true],
            disabled: false,
            cursor: 0,
            hits: 0,
            misses: 0,
        };
        memo.insert(&memory, 0, vec![Value::Null], &[Value::UInt64(0)]);
        memo.insert(
            &memory,
            0,
            vec![Value::Utf8("a".into())],
            &[Value::UInt64(1)],
        );
        assert_eq!(memo.get(0, &[Value::Null]), Some(vec![Value::UInt64(0)]));
        assert_eq!(
            memo.get(0, &[Value::Utf8("a".into())]),
            Some(vec![Value::UInt64(1)])
        );
        // Case-insensitive collation would call these equal; the memo does
        // not, and answers with a miss.
        assert_eq!(memo.get(0, &[Value::Utf8("A".into())]), None);
        let stats = memo.finish(&memory);
        assert_eq!((stats.hits, stats.misses), (2, 1));
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn a_volatile_slot_is_never_consulted() {
        let memory = tracker(1 << 20);
        let mut memo = DependentMemo {
            entries: std::collections::HashMap::new(),
            reserved: 0,
            memoizable: vec![false],
            disabled: false,
            cursor: 0,
            hits: 0,
            misses: 0,
        };
        memo.insert(&memory, 0, vec![Value::UInt64(1)], &[Value::UInt64(9)]);
        assert!(memo.entries.is_empty());
        assert!(memo.get(0, &[Value::UInt64(1)]).is_none());
        let stats = memo.finish(&memory);
        assert_eq!((stats.hits, stats.misses), (0, 0), "not even counted");
    }
}
