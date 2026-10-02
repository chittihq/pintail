//! Row constructors in comparisons: `(a, b) = (1, 2)`, `ROW(a, b) < (x, y)`,
//! `(a, b) IN ((1, 2), (3, 4))`.
//!
//! A row comparison is a fixed combination of its columns' comparisons, so
//! it is rewritten into them before binding and every later stage sees
//! ordinary scalar predicates:
//!
//! - `=` is every pair equal, `<=>` every pair null-safe equal, and `<>` any
//!   pair different. Three-valued logic carries `MySQL`'s NULL answer through:
//!   `(1, NULL) = (1, 2)` is NULL, `(1, NULL) = (2, 2)` is false.
//! - `<`, `<=`, `>`, `>=` compare lexicographically: the first pair decides
//!   unless it is equal, in which case the rest of the row does.
//!   `(a1, a2) < (b1, b2)` is `a1 < b1 OR (a1 = b1 AND a2 < b2)`, which is
//!   NULL exactly where `MySQL` answers NULL - an undecided pair whose
//!   successors cannot settle the comparison.
//! - `IN` is equality with any listed row, and `NOT IN` its negation. Against
//!   a subquery, `IN` asks whether some member row equals, through EXISTS
//!   over a derived table of the subquery that names its columns.
//!
//! Rows nest: a column that is itself a row compares by the same rules when
//! the rewritten pair is bound.

use sqlparser::ast::{
    BinaryOperator, CaseWhen, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Query,
    Select, SelectItem, SetExpr, TableAliasColumnDef, TableFactor,
    helpers::attached_token::AttachedToken,
};

use super::BindError;

/// The columns of a row constructor, or `None` for any other expression.
pub(super) fn columns(expr: &Expr) -> Option<Vec<Expr>> {
    match expr {
        Expr::Tuple(items) => Some(items.clone()),
        Expr::Nested(inner) => columns(inner),
        Expr::Function(function)
            if function.over.is_none() && function.name.to_string().eq_ignore_ascii_case("ROW") =>
        {
            let FunctionArguments::List(list) = &function.args else {
                return None;
            };
            if list.args.len() < 2 {
                return None;
            }
            list.args
                .iter()
                .map(|argument| match argument {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr.clone()),
                    _ => None,
                })
                .collect()
        }
        _ => None,
    }
}

fn arity_error(expected: usize) -> BindError {
    BindError::InvalidScalarFunction(format!("Operand should contain {expected} column(s)"))
}

fn binary(left: Expr, op: BinaryOperator, right: Expr) -> Expr {
    Expr::Nested(Box::new(Expr::BinaryOp {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }))
}

/// `left op right` over two rows, rewritten into column comparisons; `None`
/// when either side is not a row constructor or `op` is not a comparison.
///
/// # Errors
///
/// A row compared with a row of another width, or with a scalar.
pub(super) fn comparison(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
) -> Result<Option<Expr>, BindError> {
    if !matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Spaceship
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    ) {
        return Ok(None);
    }
    let (left_columns, right_columns) = match (columns(left), columns(right)) {
        (Some(left), Some(right)) => (left, right),
        (None, None) => return Ok(None),
        // A row against a subquery is the subquery binder's to answer.
        (Some(_), None) if is_subquery(right) => return Ok(None),
        (None, Some(_)) if is_subquery(left) => return Ok(None),
        (Some(row), None) | (None, Some(row)) => return Err(arity_error(row.len())),
    };
    if left_columns.len() != right_columns.len() {
        return Err(arity_error(left_columns.len()));
    }
    let pairs = left_columns
        .into_iter()
        .zip(right_columns)
        .collect::<Vec<_>>();
    Ok(Some(match op {
        BinaryOperator::Eq | BinaryOperator::Spaceship => all(pairs, op),
        BinaryOperator::NotEq => pairs
            .into_iter()
            .map(|(left, right)| binary(left, BinaryOperator::NotEq, right))
            .reduce(|left, right| binary(left, BinaryOperator::Or, right))
            .ok_or_else(|| arity_error(0))?,
        _ => lexicographic(pairs, op),
    }))
}

fn is_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery(_) => true,
        Expr::Nested(inner) => is_subquery(inner),
        _ => false,
    }
}

fn all(pairs: Vec<(Expr, Expr)>, op: &BinaryOperator) -> Expr {
    pairs
        .into_iter()
        .map(|(left, right)| binary(left, op.clone(), right))
        .reduce(|left, right| binary(left, BinaryOperator::And, right))
        .unwrap_or_else(|| Expr::value(sqlparser::ast::Value::Boolean(true)))
}

/// `(a1, ..., an) op (b1, ..., bn)` for an ordering `op`.
fn lexicographic(mut pairs: Vec<(Expr, Expr)>, op: &BinaryOperator) -> Expr {
    let strict = match op {
        BinaryOperator::LtEq => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::Gt,
        other => other.clone(),
    };
    // Built from the last pair outward: the last pair uses `op` itself, so
    // `<=` admits equal rows; every earlier pair either decides strictly or
    // is equal and defers.
    let Some((left, right)) = pairs.pop() else {
        return Expr::value(sqlparser::ast::Value::Boolean(false));
    };
    let mut tail = binary(left, op.clone(), right);
    while let Some((left, right)) = pairs.pop() {
        let decides = binary(left.clone(), strict.clone(), right.clone());
        let defers = binary(
            binary(left, BinaryOperator::Eq, right),
            BinaryOperator::And,
            tail,
        );
        tail = binary(decides, BinaryOperator::Or, defers);
    }
    tail
}

/// `row [NOT] IN (row, ...)`, rewritten into row equalities; `None` when
/// `expr` is not a row constructor.
///
/// # Errors
///
/// A listed item that is not a row of the same width.
pub(super) fn in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
) -> Result<Option<Expr>, BindError> {
    let Some(width) = columns(expr).map(|columns| columns.len()) else {
        return Ok(None);
    };
    let mut any: Option<Expr> = None;
    for item in list {
        if columns(item).is_none_or(|columns| columns.len() != width) {
            return Err(arity_error(width));
        }
        let equal = binary(expr.clone(), BinaryOperator::Eq, item.clone());
        any = Some(match any {
            None => equal,
            Some(previous) => binary(previous, BinaryOperator::Or, equal),
        });
    }
    let Some(any) = any else {
        return Err(arity_error(width));
    };
    Ok(Some(if negated {
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            expr: Box::new(Expr::Nested(Box::new(any))),
        }
    } else {
        any
    }))
}

/// The name the rewrite gives a subquery's `index`th column.
fn member_column(index: usize) -> Ident {
    Ident::with_quote('`', format!("<row-member-{index}>"))
}

const MEMBERS: &str = "<row-members>";

/// `EXISTS (SELECT 1 FROM (members) AS <row-members> (columns) WHERE
/// condition)`, the derived table naming the subquery's `width` columns for
/// the rewrite.
fn exists_member(members: &Query, width: usize, condition: Expr) -> Result<Expr, BindError> {
    let template = crate::parse_expression(&format!(
        "EXISTS (SELECT 1 FROM (SELECT 1) AS `{MEMBERS}` WHERE TRUE)"
    ))
    .map_err(|error| BindError::UnsupportedSubquery(error.to_string()))?;
    let Expr::Exists {
        mut subquery,
        negated,
    } = template
    else {
        unreachable!("the template parses as EXISTS");
    };
    let SetExpr::Select(select) = subquery.body.as_mut() else {
        unreachable!("the template's body is a SELECT");
    };
    select.selection = Some(condition);
    let TableFactor::Derived {
        subquery: from,
        alias: Some(alias),
        ..
    } = &mut select.from[0].relation
    else {
        unreachable!("the template reads a derived table under an alias");
    };
    **from = members.clone();
    alias.columns = (0..width)
        .map(|index| TableAliasColumnDef {
            name: member_column(index),
            data_type: None,
        })
        .collect();
    Ok(Expr::Exists { subquery, negated })
}

/// The leftmost SELECT of a query body: the one that names a set
/// operation's columns.
fn naming_select(body: &mut SetExpr) -> Option<&mut Select> {
    match body {
        SetExpr::Select(select) => Some(select),
        SetExpr::Query(query) => naming_select(query.body.as_mut()),
        SetExpr::SetOperation { left, .. } => naming_select(left),
        _ => None,
    }
}

/// `subquery` as the rewrite reads it, checked against the row's width.
///
/// The subquery is left as written and the derived table over it names its
/// columns. Renaming its select list instead took away the aliases its own
/// GROUP BY, HAVING and ORDER BY name: `(a, b) IN (SELECT x AS a, y AS b
/// FROM t GROUP BY a, b)` then grouped by the outer row's `a` and `b`, one
/// group for the whole subquery, and under any other alias it did not bind.
fn members(subquery: &Query, width: usize) -> Result<Query, BindError> {
    let mut members = subquery.clone();
    let select = naming_select(members.body.as_mut())
        .ok_or_else(|| BindError::UnsupportedSubquery(subquery.to_string()))?;
    // A star's width is the table's, which is not known here.
    if !select.projection.iter().all(|item| {
        matches!(
            item,
            SelectItem::UnnamedExpr(_) | SelectItem::ExprWithAlias { .. }
        )
    }) {
        return Err(BindError::UnsupportedSubquery(subquery.to_string()));
    }
    if select.projection.len() != width {
        return Err(arity_error(width));
    }
    Ok(members)
}

/// Every column of `row` equal to the member's column in its place.
fn equal_to_member(row: &[Expr]) -> Expr {
    let pairs = row
        .iter()
        .enumerate()
        .map(|(index, column)| {
            (
                column.clone(),
                Expr::CompoundIdentifier(vec![
                    Ident::with_quote('`', MEMBERS),
                    member_column(index),
                ]),
            )
        })
        .collect();
    all(pairs, &BinaryOperator::Eq)
}

/// `row [NOT] IN (subquery)`, rewritten into membership tests over the
/// subquery's rows; `None` when `expr` is not a row constructor.
///
/// Membership is three-valued, as for a single column: true when some row
/// equals, else NULL when some row's equality is undecided, else false.
/// `filter` is set where the test is a WHERE conjunct as written - there a
/// NULL answer rejects the row just as false does, so the undecided case
/// needs no test of its own and the whole membership is one EXISTS.
///
/// # Errors
///
/// A subquery of another width than the row.
pub(super) fn in_subquery(
    expr: &Expr,
    subquery: &Query,
    negated: bool,
    filter: bool,
) -> Result<Option<Expr>, BindError> {
    let Some(row) = columns(expr) else {
        return Ok(None);
    };
    let members = members(subquery, row.len())?;
    let equal = equal_to_member(&row);
    let found = exists_member(&members, row.len(), equal.clone())?;
    if filter && !negated {
        return Ok(Some(found));
    }
    let undecided = exists_member(
        &members,
        row.len(),
        Expr::IsNull(Box::new(Expr::Nested(Box::new(equal)))),
    )?;
    let boolean = |value| Expr::value(sqlparser::ast::Value::Boolean(value));
    let membership = Expr::Case {
        case_token: AttachedToken::empty(),
        end_token: AttachedToken::empty(),
        operand: None,
        conditions: vec![
            CaseWhen {
                condition: found,
                result: boolean(true),
            },
            CaseWhen {
                condition: undecided,
                result: Expr::value(sqlparser::ast::Value::Null),
            },
        ],
        else_result: Some(Box::new(boolean(false))),
    };
    Ok(Some(if negated {
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            expr: Box::new(Expr::Nested(Box::new(membership))),
        }
    } else {
        membership
    }))
}
