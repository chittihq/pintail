//! `MySQL`-dialect SQL frontend for Pintail.

mod binder;
mod bound;

use std::fmt;

use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::{Parser, ParserError};

pub use sqlparser::ast::Statement;

pub use binder::{BindError, Binder};
pub use bound::{
    BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundLimit, BoundProjection, BoundQuery,
    BoundTable, UnaryOp,
};

/// An error produced while parsing a SQL request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The SQL lexer or parser rejected the input.
    InvalidSql(ParserError),
    /// The input contained no statement.
    Empty,
    /// A single-statement API received more than one statement.
    MultipleStatements {
        /// Number of statements in the input.
        count: usize,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSql(error) => error.fmt(formatter),
            Self::Empty => formatter.write_str("SQL input contains no statement"),
            Self::MultipleStatements { count } => {
                write!(formatter, "expected one SQL statement, found {count}")
            }
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSql(error) => Some(error),
            Self::Empty | Self::MultipleStatements { .. } => None,
        }
    }
}

impl From<ParserError> for ParseError {
    fn from(error: ParserError) -> Self {
        Self::InvalidSql(error)
    }
}

/// Parse every semicolon-delimited statement using `MySQL` lexical and grammar
/// rules.
///
/// An empty input is valid here and returns an empty vector. Call
/// [`parse_statement`] when the request protocol requires exactly one
/// statement.
///
/// # Errors
///
/// Returns [`ParseError::InvalidSql`] when tokenization or parsing fails.
pub fn parse_statements(sql: &str) -> Result<Vec<Statement>, ParseError> {
    Parser::parse_sql(&MySqlDialect {}, sql).map_err(ParseError::from)
}

/// Parse exactly one MySQL-dialect statement.
///
/// # Errors
///
/// Returns [`ParseError::Empty`] or [`ParseError::MultipleStatements`] when
/// the input does not contain exactly one statement, and
/// [`ParseError::InvalidSql`] when tokenization or parsing fails.
pub fn parse_statement(sql: &str) -> Result<Statement, ParseError> {
    let statements = parse_statements(sql)?;
    match statements.len() {
        0 => Err(ParseError::Empty),
        1 => statements.into_iter().next().ok_or(ParseError::Empty),
        count => Err(ParseError::MultipleStatements { count }),
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::ast::{LimitClause, Statement};

    use super::{ParseError, parse_statement, parse_statements};

    #[test]
    fn parses_mysql_identifiers_and_limit_offset_count() {
        let statement =
            parse_statement("SELECT `order`, `total` FROM `sales-db`.`daily totals` LIMIT 10, 25")
                .expect("valid MySQL query");

        let Statement::Query(query) = &statement else {
            panic!("expected query");
        };
        assert!(matches!(
            query.limit_clause,
            Some(LimitClause::OffsetCommaLimit { .. })
        ));
        assert_eq!(
            statement.to_string(),
            "SELECT `order`, `total` FROM `sales-db`.`daily totals` LIMIT 10, 25"
        );
    }

    #[test]
    fn parses_mysql_metadata_and_explain_statements() {
        let cases = [
            "SHOW DATABASES",
            "SHOW TABLES FROM `analytics`",
            "SHOW COLUMNS FROM `events`",
            "DESCRIBE `events`",
            "EXPLAIN SELECT * FROM `events`",
        ];

        for sql in cases {
            parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
    }

    #[test]
    fn parses_non_recursive_ctes() {
        let statement =
            parse_statement("WITH recent AS (SELECT id FROM events) SELECT id FROM recent")
                .expect("valid common table expression");

        let Statement::Query(query) = statement else {
            panic!("expected query");
        };
        assert!(query.with.is_some());
    }

    #[test]
    fn preserves_statement_batch_boundaries() {
        let statements = parse_statements("SELECT 1; SELECT 2").expect("valid statement batch");
        assert_eq!(statements.len(), 2);
        assert_eq!(
            parse_statement("SELECT 1; SELECT 2"),
            Err(ParseError::MultipleStatements { count: 2 })
        );
    }

    #[test]
    fn rejects_empty_and_invalid_single_statements() {
        assert_eq!(parse_statement(" ; "), Err(ParseError::Empty));
        assert!(matches!(
            parse_statement("SELECT FROM"),
            Err(ParseError::InvalidSql(_))
        ));
    }
}
