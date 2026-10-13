//! Statement-pinned system variables, resolved by the connection before binding.
use std::{cell::RefCell, collections::HashMap, sync::Arc};

use sqlparser::ast::{Expr, Value};

/// Literal values keyed by the complete lowercase system-variable reference,
/// or by the lowercase call (`user()`) of a connection function.
pub type SystemVariables = Arc<HashMap<String, Value>>;

thread_local! {
    static VARIABLES: RefCell<Option<SystemVariables>> = const { RefCell::new(None) };
}

/// Installs a statement's system values and restores the preceding scope on exit.
pub fn with_system_variables<T>(variables: SystemVariables, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<SystemVariables>);
    impl Drop for Restore {
        fn drop(&mut self) {
            VARIABLES.with(|cell| *cell.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(VARIABLES.with(|cell| cell.borrow_mut().replace(variables)));
    work()
}

pub(crate) fn literal(expression: &Expr) -> Option<Value> {
    // A function that asks about the server or the connection - `USER()`,
    // `CONNECTION_ID()` - is pinned the same way, under its lowercase call.
    if let Expr::Function(function) = expression {
        use sqlparser::ast::FunctionArguments;
        let bare = match &function.args {
            FunctionArguments::None => true,
            FunctionArguments::List(list) => list.args.is_empty() && list.clauses.is_empty(),
            FunctionArguments::Subquery(_) => false,
        };
        if !bare || function.over.is_some() {
            return None;
        }
        let name = format!("{}()", function.name.to_string().to_ascii_lowercase());
        return VARIABLES.with(|cell| cell.borrow().as_ref()?.get(&name).cloned());
    }
    // `CURRENT_USER` is also written without its parentheses.
    if let Expr::Identifier(identifier) = expression
        && identifier.quote_style.is_none()
        && identifier.value.eq_ignore_ascii_case("current_user")
    {
        return VARIABLES.with(|cell| cell.borrow().as_ref()?.get("current_user()").cloned());
    }
    let identifiers = match expression {
        Expr::Identifier(identifier) => std::slice::from_ref(identifier),
        Expr::CompoundIdentifier(identifiers) => identifiers,
        _ => return None,
    };
    if identifiers.first()?.quote_style.is_some() || !identifiers[0].value.starts_with("@@") {
        return None;
    }
    let name = identifiers
        .iter()
        .map(|identifier| identifier.value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".");
    VARIABLES.with(|cell| cell.borrow().as_ref()?.get(&name).cloned())
}
