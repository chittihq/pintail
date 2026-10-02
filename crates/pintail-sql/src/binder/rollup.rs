//! `GROUP BY ... WITH ROLLUP`, rewritten into ordinary grouped queries.
//!
//! `GROUP BY k1, ..., kn WITH ROLLUP` returns the groups of every prefix of
//! the key list: the full grouping, then each shorter one, then one grand
//! total. A prefix's rows show NULL for the keys it rolled up, and so does
//! every expression over those keys outside an aggregate, HAVING included -
//! `HAVING k2 > 1` drops the rows that rolled `k2` up. That is exactly a
//! UNION ALL of n+1 grouped queries in which the rolled keys are replaced by
//! NULL outside aggregate arguments, so the rewrite happens on the syntax
//! tree and every clause then binds, plans and executes as it already does.
//!
//! Two details keep the union faithful:
//!
//! - The grand total groups by nothing, and an ungrouped aggregate over no
//!   rows still yields a row. A rollup over no rows yields none, so that
//!   branch keeps only a total that saw rows.
//! - Each branch carries two hidden columns per key - whether the key is
//!   rolled up, and its value - which the result is sorted by and then drops.
//!   Sorting (rolled, value) per key puts each subtotal after the groups it
//!   totals and the grand total last, which is the order the rows come out
//!   in when nothing else is asked for. A written ORDER BY sorts first and
//!   these only break its ties.
//!
//! `GROUPING(k, ...)` becomes the literal bitmask of which of its arguments
//! the branch rolled up.

use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, GroupByWithModifier, Ident,
    OrderBy, OrderByExpr, OrderByKind, OrderByOptions, Query, Select, SelectItem, SetExpr,
    SetOperator, SetQuantifier, Value as SqlValue, VisitMut, VisitorMut,
};

use super::BindError;

/// Aggregate functions whose arguments see the source rows, not the rolled
/// up NULLs.
const AGGREGATES: &[&str] = &[
    "AVG",
    "BIT_AND",
    "BIT_OR",
    "BIT_XOR",
    "COUNT",
    "GROUP_CONCAT",
    "JSON_ARRAYAGG",
    "JSON_OBJECTAGG",
    "MAX",
    "MIN",
    "STD",
    "STDDEV",
    "STDDEV_POP",
    "STDDEV_SAMP",
    "SUM",
    "VARIANCE",
    "VAR_POP",
    "VAR_SAMP",
];

const ROLLED_PREFIX: &str = "<rollup-rolled-";
const VALUE_PREFIX: &str = "<rollup-value-";

/// A rollup query rewritten into a union, and how many trailing hidden
/// columns its result carries.
pub(super) struct Rewritten {
    pub(super) query: Query,
    pub(super) hidden: usize,
}

/// One grouping key: as written, and as the expression it stands for.
struct Key {
    /// The key as written, `None` for an ordinal: `GROUP BY 1` names the
    /// first select item, and a literal `1` elsewhere in the query is not it.
    written: Option<Expr>,
    /// The select item an ordinal or alias names, else the key itself.
    effective: Expr,
    /// The select item an ordinal or alias names. Rolling the key up turns
    /// that item into NULL as a whole, and only that one: in
    /// `SELECT a, a AS c ... GROUP BY a, c` rolling up `c` leaves `a`.
    item: Option<usize>,
}

/// Rewrites `query` when its body is one `SELECT ... WITH ROLLUP`; `None`
/// for every other query.
///
/// # Errors
///
/// Refuses the modifiers and shapes the union cannot express: `WITH CUBE`
/// and `WITH TOTALS`, `DISTINCT`, window functions, and a `GROUPING()`
/// argument that is not a grouping key.
pub(super) fn rewrite(query: &Query, source: Option<&str>) -> Result<Option<Rewritten>, BindError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let GroupByExpr::Expressions(groups, modifiers) = &select.group_by else {
        return Ok(None);
    };
    if modifiers.is_empty() {
        return Ok(None);
    }
    let refuse = || BindError::UnsupportedQueryClause(select.group_by.to_string());
    if modifiers.as_slice() != [GroupByWithModifier::Rollup] || groups.is_empty() {
        return Err(refuse());
    }
    // DISTINCT would deduplicate each branch rather than the result, and a
    // window would number each branch on its own.
    if select.distinct.is_some() || !select.named_window.is_empty() || has_window(select) {
        return Err(refuse());
    }
    let keys = groups
        .iter()
        .map(|written| effective_key(written, &select.projection))
        .collect::<Result<Vec<_>, _>>()?;

    let mut body: Option<SetExpr> = None;
    for level in (0..=keys.len()).rev() {
        let branch = SetExpr::Select(Box::new(branch(select, groups, &keys, level, source)?));
        body = Some(match body {
            None => branch,
            Some(left) => SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier: SetQuantifier::All,
                left: Box::new(left),
                right: Box::new(branch),
            },
        });
    }
    let Some(body) = body else {
        return Err(refuse());
    };

    let mut order = match &query.order_by {
        None => Vec::new(),
        Some(OrderBy {
            kind: OrderByKind::Expressions(expressions),
            interpolate: None,
        }) => expressions
            .iter()
            .cloned()
            .map(|mut order| {
                // A key sorts by its hidden value column, which exists in
                // every branch; a key the select list does not show would
                // otherwise need a hidden column in the first branch only.
                if let Some(index) = keys.iter().position(|key| matches_key(&order.expr, key)) {
                    order.expr = Expr::Identifier(Ident::new(value_name(index)));
                }
                order
            })
            .collect(),
        Some(other) => return Err(BindError::InvalidOrderBy(other.to_string())),
    };
    for index in 0..keys.len() {
        for name in [rolled_name(index), value_name(index)] {
            order.push(OrderByExpr {
                expr: Expr::Identifier(Ident::new(name)),
                options: OrderByOptions {
                    asc: None,
                    nulls_first: None,
                },
                with_fill: None,
            });
        }
    }
    let mut rewritten = query.clone();
    rewritten.body = Box::new(body);
    rewritten.order_by = Some(OrderBy {
        kind: OrderByKind::Expressions(order),
        interpolate: None,
    });
    Ok(Some(Rewritten {
        query: rewritten,
        hidden: keys.len() * 2,
    }))
}

/// The hidden columns' names, in the order the result carries them.
pub(super) fn hidden_names(keys: usize) -> impl Iterator<Item = String> {
    (0..keys).flat_map(|index| [rolled_name(index), value_name(index)])
}

fn rolled_name(index: usize) -> String {
    format!("{ROLLED_PREFIX}{index}>")
}

fn value_name(index: usize) -> String {
    format!("{VALUE_PREFIX}{index}>")
}

/// The branch that groups by the first `level` keys.
fn branch(
    select: &Select,
    groups: &[Expr],
    keys: &[Key],
    level: usize,
    source: Option<&str>,
) -> Result<Select, BindError> {
    let mut branch = select.clone();
    branch.group_by = GroupByExpr::Expressions(groups[..level].to_vec(), Vec::new());
    for (index, item) in branch.projection.iter_mut().enumerate() {
        let (expr, unnamed) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, true),
            SelectItem::ExprWithAlias { expr, .. } => (expr, false),
            _ => continue,
        };
        let original = expr.clone();
        if keys[level..].iter().any(|key| key.item == Some(index)) {
            *expr = Expr::value(SqlValue::Null);
        } else {
            roll_up(expr, keys, level)?;
        }
        // The column keeps the name the written item gives it: a folded
        // `GROUPING(a)` is still the column `GROUPING(a)`, not `0`.
        if unnamed && *expr != original {
            let alias = Ident::new(super::projection_name(
                &original,
                source,
                super::SourceClause::Projection,
            ));
            *item = SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias,
            };
        }
    }
    if let Some(having) = &mut branch.having {
        roll_up(having, keys, level)?;
    }
    if level == 0 {
        let saw_rows = total_saw_rows();
        branch.having = Some(match branch.having.take() {
            None => saw_rows,
            Some(having) => Expr::BinaryOp {
                left: Box::new(Expr::Nested(Box::new(having))),
                op: sqlparser::ast::BinaryOperator::And,
                right: Box::new(saw_rows),
            },
        });
    }
    for (index, key) in keys.iter().enumerate() {
        let rolled = index >= level;
        branch.projection.push(SelectItem::ExprWithAlias {
            expr: number(u64::from(rolled)),
            alias: Ident::new(rolled_name(index)),
        });
        branch.projection.push(SelectItem::ExprWithAlias {
            expr: if rolled {
                Expr::value(SqlValue::Null)
            } else {
                key.effective.clone()
            },
            alias: Ident::new(value_name(index)),
        });
    }
    Ok(branch)
}

/// `COUNT(*) > 0`: the grand total's guard against an empty input.
fn total_saw_rows() -> Expr {
    Expr::BinaryOp {
        left: Box::new(Expr::Function(sqlparser::ast::Function {
            name: sqlparser::ast::ObjectName::from(vec![Ident::new("COUNT")]),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)],
                clauses: Vec::new(),
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: Vec::new(),
        })),
        op: sqlparser::ast::BinaryOperator::Gt,
        right: Box::new(number(0)),
    }
}

fn number(value: u64) -> Expr {
    Expr::value(SqlValue::Number(value.to_string(), false))
}

/// What a written key stands for: `GROUP BY 2` is the second select item,
/// and `GROUP BY name` is the item aliased `name` when one is.
fn effective_key(written: &Expr, projection: &[SelectItem]) -> Result<Key, BindError> {
    if let Expr::Value(value) = written
        && let SqlValue::Number(digits, _) = &value.value
        && !digits.contains(['.', 'e', 'E'])
    {
        let index = digits
            .parse::<usize>()
            .ok()
            .and_then(|ordinal| ordinal.checked_sub(1));
        return match index.and_then(|index| Some((index, projection.get(index)?))) {
            Some((
                index,
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. },
            )) => Ok(Key {
                written: None,
                effective: expr.clone(),
                item: Some(index),
            }),
            _ => Err(BindError::UnsupportedQueryClause(written.to_string())),
        };
    }
    if let Expr::Identifier(name) = written
        && let Some((index, expr)) =
            projection
                .iter()
                .enumerate()
                .find_map(|(index, item)| match item {
                    SelectItem::ExprWithAlias { expr, alias }
                        if alias.value.eq_ignore_ascii_case(&name.value) =>
                    {
                        Some((index, expr))
                    }
                    _ => None,
                })
    {
        return Ok(Key {
            written: Some(written.clone()),
            effective: expr.clone(),
            item: Some(index),
        });
    }
    Ok(Key {
        written: Some(written.clone()),
        effective: written.clone(),
        item: None,
    })
}

/// Whether `expr` is the same thing as `key`: the same expression, or the
/// same column however qualified.
fn matches_key(expr: &Expr, key: &Key) -> bool {
    if *expr == key.effective || key.written.as_ref() == Some(expr) {
        return true;
    }
    let Some((qualifier, name)) = column_reference(expr) else {
        return false;
    };
    std::iter::once(&key.effective)
        .chain(&key.written)
        .any(|candidate| {
            column_reference(candidate).is_some_and(|(key_qualifier, key_name)| {
                key_name.eq_ignore_ascii_case(name)
                    && match (qualifier, key_qualifier) {
                        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                        _ => true,
                    }
            })
        })
}

/// `(qualifier, column)` for a column reference.
fn column_reference(expr: &Expr) -> Option<(Option<&str>, &str)> {
    match expr {
        Expr::Identifier(name) => Some((None, name.value.as_str())),
        Expr::CompoundIdentifier(parts) => {
            let (name, rest) = parts.split_last()?;
            Some((
                rest.last().map(|part| part.value.as_str()),
                name.value.as_str(),
            ))
        }
        _ => None,
    }
}

fn is_aggregate(expr: &Expr) -> bool {
    let Expr::Function(function) = expr else {
        return false;
    };
    function.over.is_none()
        && function.name.0.len() == 1
        && AGGREGATES
            .iter()
            .any(|name| function.name.to_string().eq_ignore_ascii_case(name))
}

fn is_grouping(expr: &Expr) -> Option<&[FunctionArg]> {
    let Expr::Function(function) = expr else {
        return None;
    };
    if function.over.is_some() || !function.name.to_string().eq_ignore_ascii_case("GROUPING") {
        return None;
    }
    match &function.args {
        FunctionArguments::List(list) => Some(&list.args),
        _ => None,
    }
}

fn has_window(select: &Select) -> bool {
    let mut select = select.clone();
    let mut found = false;
    let _ = sqlparser::ast::visit_expressions_mut(&mut select, |expr| {
        found |= matches!(expr, Expr::Function(function) if function.over.is_some());
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Replaces the keys the branch rolled up with NULL, outside aggregate
/// arguments and subqueries, and folds `GROUPING()` to its bitmask.
fn roll_up(expr: &mut Expr, keys: &[Key], level: usize) -> Result<(), BindError> {
    let mut visitor = RollUp {
        keys,
        level,
        aggregate_depth: 0,
        query_depth: 0,
        depth: 0,
        kept_at: None,
    };
    match expr.visit(&mut visitor) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

struct RollUp<'a> {
    keys: &'a [Key],
    level: usize,
    aggregate_depth: usize,
    query_depth: usize,
    /// How deep the walk is in the expression tree.
    depth: usize,
    /// The depth of an expression that is itself a key this branch still
    /// groups by. It keeps its grouped value whole: in `GROUP BY LEFT(a, 10),
    /// a` the subtotal that rolls `a` up still shows `LEFT(a, 10)`.
    kept_at: Option<usize>,
}

impl VisitorMut for RollUp<'_> {
    type Break = BindError;

    fn pre_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
        self.query_depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
        self.query_depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self.query_depth > 0 {
            return ControlFlow::Continue(());
        }
        self.depth += 1;
        if self.kept_at.is_some() {
            return ControlFlow::Continue(());
        }
        if self.aggregate_depth == 0
            && self.keys[..self.level]
                .iter()
                .any(|key| matches_key(expr, key))
        {
            self.kept_at = Some(self.depth);
            return ControlFlow::Continue(());
        }
        if let Some(arguments) = is_grouping(expr) {
            let mut mask = 0_u64;
            for (position, argument) in arguments.iter().enumerate() {
                let FunctionArg::Unnamed(FunctionArgExpr::Expr(argument)) = argument else {
                    return ControlFlow::Break(BindError::InvalidGrouping(expr.to_string()));
                };
                let Some(index) = self.keys.iter().position(|key| matches_key(argument, key))
                else {
                    return ControlFlow::Break(BindError::InvalidGrouping(format!(
                        "argument #{} of GROUPING function is not in GROUP BY",
                        position + 1
                    )));
                };
                mask = (mask << 1) | u64::from(index >= self.level);
            }
            *expr = number(mask);
            return ControlFlow::Continue(());
        }
        if self.aggregate_depth == 0
            && self.keys[self.level..]
                .iter()
                .any(|key| matches_key(expr, key))
        {
            *expr = Expr::value(SqlValue::Null);
            return ControlFlow::Continue(());
        }
        if is_aggregate(expr) {
            self.aggregate_depth += 1;
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self.query_depth > 0 {
            return ControlFlow::Continue(());
        }
        match self.kept_at {
            Some(depth) if depth == self.depth => self.kept_at = None,
            None if is_aggregate(expr) => self.aggregate_depth -= 1,
            Some(_) | None => {}
        }
        self.depth -= 1;
        ControlFlow::Continue(())
    }
}
