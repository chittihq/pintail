//! Resolve connection settings before evaluating a SET expression.
use sqlparser::ast::{Expr, ValueWithSpan, visit_expressions_mut};

pub(super) fn select(expression: &str, session: &super::Session) -> Result<String, String> {
    let mut statement = pintail_sql::parse_statement(&format!("SELECT {expression}"))
        .map_err(|error| error.to_string())?;
    let _: std::ops::ControlFlow<()> = visit_expressions_mut(&mut statement, |expression| {
        if matches!(
            expression,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_)
        ) {
            let name = expression.to_string();
            if name.starts_with("@@")
                && let Some(output) =
                    super::compatibility_query(&format!("SELECT {name}"), "", session)
                && output.fields.len() == 1
                && output.rows.len() == 1
            {
                *expression = Expr::Value(ValueWithSpan::from(super::user_variable_literal(
                    output.rows[0][0].clone(),
                    output.fields[0].data_type,
                )));
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    Ok(statement.to_string())
}

/// Resolve actual variable references without changing the query's syntax or labels.
pub(super) fn variables(sql: &str, session: &super::Session) -> pintail_sql::SystemVariables {
    let mut values = std::collections::HashMap::new();
    let functions = super::mentions_connection_function(sql);
    if !sql.contains("@@") && !functions {
        return std::sync::Arc::new(values);
    }
    if let Ok(statement) = pintail_sql::parse_statement(sql) {
        let _: std::ops::ControlFlow<()> =
            sqlparser::ast::visit_expressions(&statement, |expression| {
                // A function that asks about the server or the connection,
                // used inside a larger statement, reads the answer the
                // compatibility layer gives it alone.
                let called = match expression {
                    Expr::Function(function) => {
                        Some(function.name.to_string().to_ascii_lowercase())
                    }
                    // `CURRENT_USER` is also written without parentheses.
                    Expr::Identifier(identifier)
                        if identifier.quote_style.is_none()
                            && identifier.value.eq_ignore_ascii_case("current_user") =>
                    {
                        Some("current_user".to_owned())
                    }
                    _ => None,
                };
                if functions
                    && let Some(name) = called
                    && super::CONNECTION_FUNCTIONS.contains(&name.as_str())
                    && let Some(output) = super::compatibility_query(
                        &format!("SELECT {name}()"),
                        &session.user,
                        session,
                    )
                    && output.fields.len() == 1
                    && output.rows.len() == 1
                {
                    values.insert(
                        format!("{name}()"),
                        super::user_variable_literal(
                            output.rows[0][0].clone(),
                            output.fields[0].data_type,
                        ),
                    );
                }
                if matches!(
                    expression,
                    Expr::Identifier(_) | Expr::CompoundIdentifier(_)
                ) {
                    let name = expression.to_string();
                    if name.starts_with("@@")
                        && let Some(output) =
                            super::compatibility_query(&format!("SELECT {name}"), "", session)
                        && output.fields.len() == 1
                        && output.rows.len() == 1
                    {
                        values.insert(
                            name.to_ascii_lowercase(),
                            super::user_variable_literal(
                                output.rows[0][0].clone(),
                                output.fields[0].data_type,
                            ),
                        );
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
    }
    std::sync::Arc::new(values)
}
