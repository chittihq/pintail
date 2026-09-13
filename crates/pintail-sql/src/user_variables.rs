//! A connection's user variables: `SET @name = expr` and `@name` in a query.
//!
//! A variable holds the literal its assignment evaluated to, so a query
//! that reads it binds exactly as if the value had been written in its
//! place - a decimal stays a decimal, a string stays a string - while the
//! column it projects keeps its written name. The connection owns the
//! values; a statement sees them through [`with_user_variables`], which
//! scopes them to the synchronous work that binds it.

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use sqlparser::ast::Value;

/// A connection's variables by lowercase name, without the `@`.
pub type UserVariables = Arc<HashMap<String, Value>>;

thread_local! {
    static VARIABLES: RefCell<Option<UserVariables>> = const { RefCell::new(None) };
}

/// Runs synchronous work that reads `variables`, restoring the previous
/// scope even after a panic.
pub fn with_user_variables<T>(variables: UserVariables, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<UserVariables>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            VARIABLES.with(|cell| *cell.borrow_mut() = previous);
        }
    }
    let _restore = Restore(VARIABLES.with(|cell| cell.borrow_mut().replace(variables)));
    work()
}

/// The literal a user variable reference reads: its value, or NULL for a
/// variable never assigned, as in `MySQL`. `None` when `name` is not a user
/// variable reference at all.
pub(crate) fn user_variable(name: &str) -> Option<Value> {
    let name = name
        .strip_prefix('@')
        .filter(|rest| !rest.starts_with('@'))?;
    Some(
        VARIABLES
            .with(|cell| {
                cell.borrow()
                    .as_ref()
                    .and_then(|variables| variables.get(&name.to_ascii_lowercase()).cloned())
            })
            .unwrap_or(Value::Null),
    )
}

// A `SET` list is read from its own text by the wire layer, not from a
// parsed form: rendering an expression back to SQL loses backslash escapes,
// and the list can mix user variables with session settings, which a parse
// into user-variable pairs alone cannot represent.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_reads_the_scoped_value_or_null() {
        let variables = Arc::new(HashMap::from([(
            "a".to_owned(),
            Value::Number("1.50".to_owned(), false),
        )]));
        with_user_variables(variables, || {
            assert_eq!(
                user_variable("@A"),
                Some(Value::Number("1.50".to_owned(), false))
            );
            assert_eq!(user_variable("@unset"), Some(Value::Null));
            assert_eq!(user_variable("@@version"), None);
        });
        assert_eq!(user_variable("@a"), Some(Value::Null));
    }
}
