//! `MySQL`-dialect SQL frontend for Pintail.

mod binder;
mod bound;
mod metadata;

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::{Parser, ParserError};

pub use sqlparser::ast::Statement;

pub use binder::{BindError, Binder};
pub use bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind, BoundFrom,
    BoundJoin, BoundJoinKind, BoundLimit, BoundOrderKey, BoundProjection, BoundQuery,
    BoundRecursive, BoundSetOpKind, BoundTable, BoundWindow, BoundWindowOrderKey, DatePart,
    IntervalUnit, ScalarFunction, UnaryOp, WindowFunction,
};
pub use metadata::{MetadataError, MetadataField, MetadataResult, execute_metadata};

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
    let mut statements = Parser::parse_sql(&MySqlDialect {}, sql).map_err(ParseError::from)?;
    // sqlparser's MySQL dialect parses the right side of DIV with a full
    // `parse_expr`, swallowing every lower-precedence continuation
    // (`a DIV b AND c` becomes `a DIV (b AND c)`). Rebalance those nodes to
    // MySQL's grammar, where DIV binds at multiplicative precedence.
    for statement in &mut statements {
        let flow: ControlFlow<()> = sqlparser::ast::visit_expressions_mut(statement, |expr| {
            rebalance_integer_divide(expr);
            ControlFlow::Continue(())
        });
        debug_assert!(flow.is_continue());
    }
    Ok(statements)
}

/// Repairs sqlparser's `MySQL` `DIV` misparse. The dialect parses the right
/// side of `DIV` with a full-precedence `parse_expr`, swallowing every
/// looser-binding continuation (`a DIV b AND c` becomes `a DIV (b AND c)`,
/// and the damage cascades up through enclosing comparisons). A legitimate
/// left-associative parse never places a bare looser-binding construct as a
/// right child — explicit parentheses arrive as `Expr::Nested` — so any
/// such shape is rotated back to `MySQL`'s grammar.
#[allow(clippy::too_many_lines)] // one exhaustive swallowed-construct match
fn rebalance_integer_divide(expr: &mut sqlparser::ast::Expr) {
    use sqlparser::ast::{BinaryOperator, Expr};

    /// `MySQL` operator precedence (higher binds tighter) for the operators
    /// the `MySQL` dialect can produce. Unknown operators never rotate.
    fn precedence(op: &BinaryOperator) -> Option<u8> {
        match op {
            BinaryOperator::Or => Some(1),
            BinaryOperator::Xor => Some(2),
            BinaryOperator::And => Some(3),
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::Spaceship
            | BinaryOperator::Regexp => Some(COMPARISON_PRECEDENCE),
            BinaryOperator::BitwiseOr => Some(6),
            BinaryOperator::BitwiseAnd => Some(7),
            BinaryOperator::PGBitwiseShiftLeft | BinaryOperator::PGBitwiseShiftRight => Some(8),
            BinaryOperator::Plus | BinaryOperator::Minus => Some(9),
            BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::MyIntegerDivide => Some(10),
            BinaryOperator::BitwiseXor => Some(11),
            _ => None,
        }
    }
    /// `IS NULL`, `BETWEEN`, `IN`, and `LIKE` sit in `MySQL`'s comparison band.
    const COMPARISON_PRECEDENCE: u8 = 5;

    fn rebuilt(left: Box<Expr>, op: BinaryOperator, right: Box<Expr>) -> Box<Expr> {
        let mut node = Expr::BinaryOp { left, op, right };
        rebalance_integer_divide(&mut node);
        Box::new(node)
    }

    loop {
        let Expr::BinaryOp { op, .. } = &*expr else {
            return;
        };
        let Some(parent) = precedence(op) else {
            return;
        };
        let placeholder = Expr::value(sqlparser::ast::Value::Null);
        let Expr::BinaryOp { left, op, right } = std::mem::replace(expr, placeholder) else {
            unreachable!("matched a binary operator above");
        };
        *expr = match *right {
            Expr::BinaryOp {
                left: swallowed,
                op: continuation,
                right: rest,
            } if precedence(&continuation).is_some_and(|inner| inner <= parent) => Expr::BinaryOp {
                left: rebuilt(left, op, swallowed),
                op: continuation,
                right: rest,
            },
            Expr::IsNull(swallowed) if COMPARISON_PRECEDENCE <= parent => {
                Expr::IsNull(rebuilt(left, op, swallowed))
            }
            Expr::IsNotNull(swallowed) if COMPARISON_PRECEDENCE <= parent => {
                Expr::IsNotNull(rebuilt(left, op, swallowed))
            }
            Expr::Between {
                expr: swallowed,
                negated,
                low,
                high,
            } if COMPARISON_PRECEDENCE <= parent => Expr::Between {
                expr: rebuilt(left, op, swallowed),
                negated,
                low,
                high,
            },
            Expr::InList {
                expr: swallowed,
                list,
                negated,
            } if COMPARISON_PRECEDENCE <= parent => Expr::InList {
                expr: rebuilt(left, op, swallowed),
                list,
                negated,
            },
            Expr::InSubquery {
                expr: swallowed,
                subquery,
                negated,
            } if COMPARISON_PRECEDENCE <= parent => Expr::InSubquery {
                expr: rebuilt(left, op, swallowed),
                subquery,
                negated,
            },
            Expr::Like {
                negated,
                any,
                expr: swallowed,
                pattern,
                escape_char,
            } if COMPARISON_PRECEDENCE <= parent => Expr::Like {
                negated,
                any,
                expr: rebuilt(left, op, swallowed),
                pattern,
                escape_char,
            },
            other => {
                *expr = Expr::BinaryOp {
                    left,
                    op,
                    right: Box::new(other),
                };
                return;
            }
        };
        // The rotation may expose another swallowed continuation at the new
        // top-level operator; loop until the node is stable.
    }
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
