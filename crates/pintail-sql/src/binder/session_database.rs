//! `DATABASE()` and `SCHEMA()` inside a query.
//!
//! Both name the session's default database, which the binder is given and
//! nothing below it is. A query that calls either is read with the call
//! replaced by that name as a string, or NULL when the session has none.
//! The string keeps the call's place in the statement's text, so an
//! unaliased column is still named `DATABASE()` as written.

use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Function, FunctionArguments, Query, Spanned, Value, ValueWithSpan, visit_expressions,
    visit_expressions_mut,
};

/// Whether `function` is `DATABASE()` or `SCHEMA()`: unqualified, with no
/// argument and no window.
fn names_session_database(function: &Function) -> bool {
    let no_arguments = match &function.args {
        FunctionArguments::List(list) => list.args.is_empty() && list.clauses.is_empty(),
        FunctionArguments::None | FunctionArguments::Subquery(_) => false,
    };
    no_arguments
        && function.over.is_none()
        && function.name.0.len() == 1
        && ["DATABASE", "SCHEMA"]
            .iter()
            .any(|name| function.name.to_string().eq_ignore_ascii_case(name))
}

/// `query` with every `DATABASE()` and `SCHEMA()` replaced by `database`,
/// or `None` when it calls neither.
pub(super) fn rewrite(query: &Query, database: Option<&str>) -> Option<Query> {
    let called = visit_expressions(query, |expr| match expr {
        Expr::Function(function) if names_session_database(function) => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    })
    .is_break();
    if !called {
        return None;
    }
    let mut query = query.clone();
    let _ = visit_expressions_mut(&mut query, |expr| {
        if let Expr::Function(function) = expr
            && names_session_database(function)
        {
            *expr = Expr::Value(ValueWithSpan {
                value: database.map_or(Value::Null, |name| {
                    Value::SingleQuotedString(name.to_owned())
                }),
                span: function.span(),
            });
        }
        ControlFlow::<()>::Continue(())
    });
    Some(query)
}
