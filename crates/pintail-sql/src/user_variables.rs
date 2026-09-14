//! A connection's user variables: SET, SELECT assignments, and query references.
//!
//! A variable holds the literal its assignment evaluated to, so a query
//! that reads it binds exactly as if the value had been written in its
//! place - a decimal stays a decimal, a string stays a string - while the
//! column it projects keeps its written name. The connection owns the
//! values; a statement sees them through [`with_user_variables`], which
//! scopes them to the synchronous work that binds it. Statements containing
//! assignments additionally capture mutable state in compiled expressions;
//! execution workers share that state and the connection receives it on completion.

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use sqlparser::ast::{Expr, Set, Statement, Value};

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

/// The user variables a `SET` statement assigns, as (name, expression)
/// pairs in order. `None` unless every assignment targets a user variable.
#[must_use]
pub fn user_variable_assignments(statement: &Statement) -> Option<Vec<(String, Expr)>> {
    let Statement::Set(set) = statement else {
        return None;
    };
    let pairs = match set {
        Set::SingleAssignment {
            scope: None,
            hivevar: false,
            variable,
            values,
        } if values.len() == 1 => vec![(variable.to_string(), values[0].clone())],
        Set::MultipleAssignments { assignments } => assignments
            .iter()
            .map(|assignment| (assignment.name.to_string(), assignment.value.clone()))
            .collect(),
        _ => return None,
    };
    pairs
        .into_iter()
        .map(|(name, value)| {
            let name = name
                .strip_prefix('@')
                .filter(|rest| !rest.starts_with('@'))?;
            Some((name.to_ascii_lowercase(), value))
        })
        .collect()
}

/// Renders an assignment expression for evaluation under the current escape mode.
/// The AST stores decoded strings; SQL rendering must restore backslash escapes
/// without changing quoted identifiers or the expression's other tokens.
#[must_use]
pub fn user_variable_expression_sql(expression: &Expr) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let rendered = expression.to_string();
    let mut mode = crate::session_parse_mode();
    if mode.no_backslash_escapes {
        return Some(rendered);
    }
    mode.no_backslash_escapes = true;
    let dialect = crate::PintailDialect(sqlparser::dialect::MySqlDialect {}, mode);
    let tokens = Tokenizer::new(&dialect, &rendered)
        .with_unescape(false)
        .tokenize()
        .ok()?;
    Some(
        tokens
            .into_iter()
            .map(|mut token| {
                match &mut token {
                    Token::SingleQuotedString(text)
                    | Token::DoubleQuotedString(text)
                    | Token::NationalStringLiteral(text) => *text = text.replace('\\', "\\\\"),
                    _ => {}
                }
                token.to_string()
            })
            .collect(),
    )
}

/// Separates a SELECT's user-variable targets from the query that supplies them.
/// File output and table creation forms are left to ordinary statement binding.
#[must_use]
pub fn select_variable_targets(sql: &str) -> Option<(String, Vec<String>)> {
    use sqlparser::tokenizer::Token;
    if !sql
        .as_bytes()
        .windows(4)
        .any(|word| word.eq_ignore_ascii_case(b"into"))
    {
        return None;
    }
    let dialect = crate::PintailDialect(
        sqlparser::dialect::MySqlDialect {},
        crate::session_parse_mode(),
    );
    let tokens = crate::tokenize_mysql(sql, &dialect).ok()?;
    let tokens: Vec<_> = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect();
    let first = &tokens.first()?.token;
    if !matches!(first, Token::LParen)
        && !matches!(first, Token::Word(word) if word.value.eq_ignore_ascii_case("SELECT") || word.value.eq_ignore_ascii_case("WITH"))
    {
        return None;
    }
    let into = query_output_into(&tokens)?;
    let mut at = into + 1;
    let mut names = Vec::new();
    loop {
        let Token::Word(word) = &tokens.get(at)?.token else {
            return None;
        };
        let name = word
            .value
            .strip_prefix('@')
            .filter(|name| !name.starts_with('@'))?;
        at += 1;
        let name = if name.is_empty() {
            let name = match &tokens.get(at)?.token {
                Token::Word(word) if word.quote_style.is_some() => word.value.clone(),
                Token::SingleQuotedString(name) | Token::DoubleQuotedString(name) => name.clone(),
                _ => return None,
            };
            at += 1;
            name
        } else {
            name.to_owned()
        };
        names.push(name.to_ascii_lowercase());
        if !matches!(tokens.get(at).map(|token| &token.token), Some(Token::Comma)) {
            break;
        }
        at += 1;
    }
    let offset = |location: sqlparser::tokenizer::Location| {
        let mut line = 1;
        let mut column = 1;
        for (offset, ch) in sql.char_indices() {
            if line == location.line && column == location.column {
                return Some(offset);
            }
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        (line == location.line && column == location.column).then_some(sql.len())
    };
    let begin = offset(tokens[into].span.start)?;
    let end = offset(tokens[at - 1].span.end)?;
    let query = format!("{} {}", &sql[..begin], &sql[end..]);
    matches!(crate::parse_statement(&query).ok()?, Statement::Query(_)).then_some((query, names))
}

/// INTO belongs to query output, including its last parenthesized set operand,
/// but never to a scalar subquery or a derived table's SELECT.
fn query_output_into(tokens: &[&sqlparser::tokenizer::TokenWithSpan]) -> Option<usize> {
    use sqlparser::tokenizer::Token;
    let set_operator = |token: &Token| matches!(token, Token::Word(word) if ["UNION", "INTERSECT", "EXCEPT"].iter().any(|name| word.value.eq_ignore_ascii_case(name)));
    let mut parentheses = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match &token.token {
            Token::LParen => {
                let previous = index.checked_sub(1).map(|at| &tokens[at].token);
                let after_set = previous.is_some_and(set_operator)
                    || (matches!(previous, Some(Token::Word(word)) if word.value.eq_ignore_ascii_case("ALL") || word.value.eq_ignore_ascii_case("DISTINCT"))
                        && index
                            .checked_sub(2)
                            .is_some_and(|at| set_operator(&tokens[at].token)));
                let query_operand = parentheses.iter().all(|query| *query)
                    && (index == 0 || matches!(previous, Some(Token::LParen)) || after_set);
                parentheses.push(query_operand);
            }
            Token::RParen => {
                parentheses.pop()?;
            }
            Token::Word(word)
                if word.value.eq_ignore_ascii_case("INTO")
                    && parentheses.iter().all(|query| *query) =>
            {
                let into_depth = parentheses.len();
                let mut depth = into_depth;
                for trailing in &tokens[index + 1..] {
                    match &trailing.token {
                        Token::LParen => depth += 1,
                        Token::RParen => depth = depth.checked_sub(1)?,
                        token if depth <= into_depth && set_operator(token) => return None,
                        _ => {}
                    }
                }
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_statement;

    #[test]
    fn select_targets_preserve_query_text_and_ignore_nested_or_quoted_into() {
        for sql in [
            "SELECT 1, 'é' INTO @a, @B",
            "SELECT 1, 'é' INTO @a, @B FROM records",
            "SELECT 1, 'é' FROM records INTO @a, @B",
            "(SELECT 1, 'é') INTO @a, @B",
            "(SELECT 1 AS n, 'é') ORDER BY n LIMIT 1 INTO @a, @B",
        ] {
            let (query, names) = select_variable_targets(sql).expect("targets");
            assert_eq!(names, ["a", "b"]);
            assert!(!query.contains("INTO"));
            assert!(query.contains("'é'"));
        }
        for sql in [
            "SELECT 'INTO @a'",
            "SELECT (SELECT 1 INTO @a)",
            "SELECT 1 INTO OUTFILE 'x'",
            "SELECT 1 INTO table_name",
            "SELECT 1 INTO @@global.x",
            "SELECT 1 INTO @a; SELECT 2",
        ] {
            assert!(select_variable_targets(sql).is_none(), "{sql}");
        }
    }

    #[test]
    fn query_output_targets_may_follow_the_last_parenthesized_union_operand() {
        for sql in [
            "(SELECT 1 INTO @a)",
            "(SELECT 1) UNION (SELECT 1 INTO @a)",
            "(SELECT 1) UNION ALL ((SELECT 2 INTO @a))",
        ] {
            assert!(select_variable_targets(sql).is_some(), "{sql}");
        }
        for sql in [
            "(SELECT 1 INTO @a) UNION (SELECT 1)",
            "SELECT (SELECT 1 INTO @a)",
            "SELECT * FROM (SELECT 1 INTO @a) AS derived",
        ] {
            assert!(select_variable_targets(sql).is_none(), "{sql}");
        }
    }

    #[test]
    fn assignments_name_only_user_variables() {
        let statement = parse_statement("SET @a = 1, @B = 'x''y', @c = @a * 2").expect("parses");
        let pairs = user_variable_assignments(&statement).expect("user variables");
        assert_eq!(
            pairs
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>(),
            ["a=1", "b='x''y'", "c=@a * 2"]
        );
        for other in [
            "SET sql_mode = ''",
            "SET @@session.time_zone = '+00:00'",
            "SET @a = 1, x = 2",
        ] {
            let statement = parse_statement(other).expect("parses");
            assert!(user_variable_assignments(&statement).is_none(), "{other}");
        }
    }

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

type AssignedValues = HashMap<String, (pintail_types::Value, Option<pintail_types::DataType>)>;

/// Values assigned while one statement executes, shared by its compiled expressions.
#[derive(Clone, Debug, Default)]
pub struct UserVariableWrites(Arc<std::sync::Mutex<AssignedValues>>);

impl PartialEq for UserVariableWrites {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for UserVariableWrites {}

impl UserVariableWrites {
    /// Reads the latest assignment in this statement.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<pintail_types::Value> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|(value, _)| value.clone())
    }
    /// Inspects a value without allocating a copy before the executor reserves memory.
    pub fn with_value<T>(
        &self,
        name: &str,
        read: impl FnOnce(&pintail_types::Value) -> T,
    ) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|(value, _)| read(value))
    }
    /// Records an assignment, retaining its SQL type for the next statement.
    pub fn set(
        &self,
        name: String,
        value: pintail_types::Value,
        data_type: Option<pintail_types::DataType>,
    ) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name, (value, data_type));
    }
    /// Takes completed assignments once execution has settled.
    #[must_use]
    pub fn take(&self) -> HashMap<String, (pintail_types::Value, Option<pintail_types::DataType>)> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}
thread_local! {
    static WRITES: RefCell<Option<UserVariableWrites>> = const { RefCell::new(None) };
}
/// Captures the current statement's assignment state during compilation.
#[must_use]
pub fn user_variable_writes() -> Option<UserVariableWrites> {
    WRITES.with(|cell| cell.borrow().clone())
}
/// Scopes assignment state to binding and compilation; compiled expressions retain it.
pub fn with_user_variable_writes<T>(
    writes: Option<UserVariableWrites>,
    work: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<UserVariableWrites>);
    impl Drop for Restore {
        fn drop(&mut self) {
            WRITES.with(|cell| *cell.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(WRITES.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), writes)));
    work()
}
