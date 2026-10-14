//! A correlated `IN (subquery)` whose members differ for every outer row,
//! answered by asking the subquery's table yes-or-no questions.
//!
//! `x NOT IN (SELECT t.m FROM t WHERE t.g = o.g AND t.id <> o.id)` has a
//! member set of its own for every outer row: the set-at-a-time form joins
//! every outer row to all the members of its group, and the per-row path
//! reads them once per row. Either is the outer rows times the group size.
//!
//! No outer row needs its members, only three facts about them, and each
//! is an `EXISTS` over the same table that the dependent hash index
//! already answers by key:
//!
//! - **a hit**: some row passes the subquery's `WHERE` and carries the
//!   tested value - `EXISTS (.. WHERE .. AND t.m = x)`, keyed by the
//!   subquery's own equalities and the member column;
//! - **a NULL member**: `EXISTS (.. WHERE .. AND t.m IS NULL)`;
//! - **any row at all**: `EXISTS (.. WHERE ..)`.
//!
//! `IN` is then what it is over the members themselves: TRUE on a hit;
//! otherwise NULL when the tested value is NULL and there is any row, or
//! when there is a NULL member; otherwise FALSE. A NULL tested value never
//! hits, because the key it would look up is NULL. `NOT IN` is the
//! negation, which leaves NULL as it is.
//!
//! What it must never change, and how each is kept:
//!
//! - **What `=` means.** The member is tested by the index's key, so only
//!   a member column the index keys exactly against the tested value takes
//!   this path: integers, or text under the collation `IN` itself compares
//!   the two by. Any other pair stays with the paths that had it.
//! - **Which subqueries.** One table, a conjunctive `WHERE` with an
//!   equality to the outer row that keys its table, one column
//!   selected, no LIMIT: what the index reads. And only where some conjunct
//!   reads the outer row without being a key - with none, outer rows of one
//!   key share their members, and a member set built once per key is the
//!   better answer.
//! - **Anything unexpected** - a probe the index cannot answer for a row,
//!   a build that gives up - leaves the row to the paths that had it.

use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, BoundProjection, BoundQuery};
use pintail_types::{DataType, Value};

use super::dependent_index::{self, IndexState, SubqueryForm, SubqueryIndex};
use super::{DependentRow, ExecError, MemoryTracker};
use crate::collation::Collation;
use crate::expression::{CompiledExpr, predicate_truth};

/// Why a subquery whose every correlated conjunct is a key has no probes:
/// its members are read once per key, which is the better answer.
pub(super) const SHARED_MEMBERS: &str = "outer rows of one key share its members";

/// What one `IN` slot's probes are doing.
pub(super) enum ProbeState {
    /// Built and answering every row they can.
    Built(Box<Probes>),
    /// The shape does not qualify, or a build gave up. Never retried.
    Declined,
}

/// The three questions of one `IN (subquery)`, each a built index.
pub(super) struct Probes {
    /// The tested value, compiled against the operator's input.
    needle: CompiledExpr,
    hit: SubqueryIndex,
    /// `None` for a member column that is never NULL.
    null_member: Option<SubqueryIndex>,
    any: SubqueryIndex,
}

impl ProbeState {
    pub(super) fn release(&mut self, memory: &MemoryTracker) {
        if let Self::Built(probes) = self {
            probes.hit.release(memory);
            probes.any.release(memory);
            if let Some(index) = &mut probes.null_member {
                index.release(memory);
            }
        }
    }
}

/// `expression` read from inside the subquery: every column it names is
/// one of the outer row's.
fn as_outer(expression: &mut BoundExpr) {
    match &mut expression.kind {
        BoundExprKind::Column(column) => column.outer = true,
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => as_outer(expr),
        BoundExprKind::Binary { left, right, .. } => {
            as_outer(left);
            as_outer(right);
        }
        BoundExprKind::Scalar { args, .. } => args.iter_mut().for_each(as_outer),
        _ => {}
    }
}

/// `EXISTS (SELECT 1 FROM .. WHERE <the subquery's filter> AND extra)`.
fn question(query: &BoundQuery, extra: Option<BoundExpr>) -> BoundQuery {
    let filter = match (query.filter.clone(), extra) {
        (Some(filter), Some(extra)) => Some(BoundExpr {
            data_type: Some(DataType::Boolean),
            nullable: true,
            kind: BoundExprKind::Binary {
                op: BinaryOp::And,
                left: Box::new(filter),
                right: Box::new(extra),
            },
        }),
        (filter, extra) => filter.or(extra),
    };
    BoundQuery {
        projection: vec![BoundProjection {
            name: "1".to_owned(),
            expr: BoundExpr {
                data_type: Some(DataType::Int64),
                nullable: false,
                kind: BoundExprKind::Literal(Value::Int64(1)),
            },
        }],
        filter,
        distinct: false,
        order_by: Vec::new(),
        hidden_sort_columns: 0,
        outer_set: None,
        outer_set_refusal: None,
        ..query.clone()
    }
}

fn planned(query: &BoundQuery) -> Result<Box<dependent_index::IndexPlan>, &'static str> {
    match dependent_index::plan(query, SubqueryForm::Exists)? {
        IndexState::Pending { plan, .. } => Ok(plan),
        IndexState::Built(_) | IndexState::Declined => Err("its table has no index plan"),
    }
}

/// Plans and builds the probes of `needle IN (query)` for the operator
/// whose row `context` holds, or says why the shape keeps the paths it had.
pub(super) fn build(
    needle: &BoundExpr,
    query: &BoundQuery,
    member_collation: Collation,
    context: &DependentRow<'_>,
) -> Result<Probes, &'static str> {
    let [projection] = query.projection.as_slice() else {
        return Err("it selects more than one column");
    };
    let member = &projection.expr;
    if !matches!(&member.kind, BoundExprKind::Column(column) if !column.outer) {
        return Err("its member is not a column of its table");
    }
    if query.limit.is_some() {
        return Err("it has a LIMIT");
    }
    if !dependent_index::plain_expression(needle) {
        return Err("the tested value holds a subquery");
    }
    let any = planned(&question(query, None))?;
    let (keys, reads_beyond_keys) = dependent_index::shape(&any);
    if !reads_beyond_keys {
        return Err(SHARED_MEMBERS);
    }
    // With no equality to key the table by, the hit question has the
    // members' value alone to look up, and a range on the outer row tests
    // every row that carries it: a key's thousands of rows for each outer
    // row, where reading the members serves several outer rows at once.
    if keys == 0 {
        return Err("no equality to the outer row partitions its members");
    }
    let mut tested = needle.clone();
    as_outer(&mut tested);
    let equality = BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: true,
        kind: BoundExprKind::Binary {
            op: BinaryOp::Equal,
            left: Box::new(member.clone()),
            right: Box::new(tested),
        },
    };
    // Text is a key only under the collation IN compares the two by.
    if member.data_type == Some(DataType::Utf8)
        && equality
            .text_collation()
            .and_then(Collation::from_mysql_name)
            != Some(member_collation)
    {
        return Err("its member compares under another collation as a key");
    }
    let hit = planned(&question(query, Some(equality)))?;
    if dependent_index::shape(&hit).0 != keys + 1 {
        return Err("its member is not a key against the tested value");
    }
    let null_member = member
        .nullable
        .then(|| {
            planned(&question(
                query,
                Some(BoundExpr {
                    data_type: Some(DataType::Boolean),
                    nullable: false,
                    kind: BoundExprKind::IsNull {
                        expr: Box::new(member.clone()),
                        negated: false,
                    },
                }),
            ))
        })
        .transpose()?;
    let needle = CompiledExpr::compile(needle, context.columns, context.collation)
        .map_err(|_| "the tested value does not compile against the outer row")?;
    let gave_up = "a build gave up";
    let mut hit = dependent_index::build(&hit, context).ok_or(gave_up)?;
    let Some(mut any) = dependent_index::build(&any, context) else {
        hit.release(context.memory);
        return Err(gave_up);
    };
    let null_member = match null_member {
        None => None,
        Some(plan) => {
            let Some(index) = dependent_index::build(&plan, context) else {
                hit.release(context.memory);
                any.release(context.memory);
                return Err(gave_up);
            };
            Some(index)
        }
    };
    Ok(Probes {
        needle,
        hit,
        null_member,
        any,
    })
}

/// Rows the hit index holds: what the profile reports the build read.
pub(super) fn indexed_rows(probes: &Probes) -> u64 {
    dependent_index::indexed_rows(&probes.hit)
}

/// `IN` for the current outer row - TRUE, FALSE or NULL - or `None` when a
/// probe cannot answer this row and the paths that had it must.
pub(super) fn answer(
    probes: &mut Probes,
    context: &DependentRow<'_>,
) -> Result<Option<Value>, ExecError> {
    let ask = |index: &mut SubqueryIndex| -> Result<Option<bool>, ExecError> {
        dependent_index::answer(index, context)?
            .map(|found| predicate_truth(&found))
            .transpose()
    };
    let Some(hit) = ask(&mut probes.hit)? else {
        return Ok(None);
    };
    if hit {
        return Ok(Some(Value::Boolean(true)));
    }
    if matches!(
        probes.needle.evaluate(context.batch, context.row)?,
        Value::Null
    ) {
        let Some(any) = ask(&mut probes.any)? else {
            return Ok(None);
        };
        return Ok(Some(if any {
            Value::Null
        } else {
            Value::Boolean(false)
        }));
    }
    if let Some(index) = &mut probes.null_member {
        let Some(null) = ask(index)? else {
            return Ok(None);
        };
        if null {
            return Ok(Some(Value::Null));
        }
    }
    Ok(Some(Value::Boolean(false)))
}
