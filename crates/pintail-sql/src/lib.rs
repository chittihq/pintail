//! `MySQL`-dialect SQL frontend for Pintail.

mod binder;
mod bound;
mod hints;
mod interval;
mod metadata;
mod mode;
pub use mode::{ParseMode, session_parse_mode, with_parse_mode};
mod request;
pub use request::first_statement;

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::dialect::{Dialect, MySqlDialect};
use sqlparser::parser::{Parser, ParserError};

mod admission;
pub use admission::{has_bounded_admission_shape, has_bounded_planning_shape};

mod repeatable;
pub use repeatable::is_repeatable_statement;

pub use sqlparser::ast::Statement;

pub use binder::{BindError, Binder};
pub use bound::{
    AggregateFunction, BinaryOp, BoundAggregate, BoundColumn, BoundExpr, BoundExprKind,
    BoundFrameBound, BoundFrameOffset, BoundFrom, BoundJoin, BoundJoinKind, BoundLimit,
    BoundOrderKey, BoundProjection, BoundQuery, BoundRecursive, BoundSetOpKind, BoundTable,
    BoundWindow, BoundWindowFrame, BoundWindowOrderKey, DEFAULT_TEXT_COLLATION, DatePart,
    IntervalUnit, JSON_TEXT_COLLATION, MembershipError, MembershipLookup, PreparedMembership,
    SUPPORTED_TEXT_COLLATIONS, ScalarFunction, UnaryOp, WindowFunction, session_default_collation,
    set_session_default_collation,
};
pub use hints::max_execution_time_hint;
pub use metadata::{
    ColumnFacts, ForeignKeyFacts, IndexFacts, MetadataError, MetadataField, MetadataResult,
    SourceFacts, execute_metadata, metadata_relations,
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
    let dialect = PintailDialect(MySqlDialect {}, session_parse_mode());
    let mut tokens = sqlparser::tokenizer::Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(ParserError::from)?;
    if !dialect.1.pipes_as_concat {
        for token in &mut tokens {
            if token.token == sqlparser::tokenizer::Token::StringConcat {
                token.token = sqlparser::tokenizer::Token::make_keyword("OR");
            }
        }
    }
    interval::rewrite(&mut tokens);
    let mut statements = Parser::new(&dialect)
        .with_tokens_with_locations(tokens)
        .parse_statements()
        .map_err(ParseError::from)?;
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

/// Parses one expression under the session's dialect, for rewrites that
/// read more clearly written as SQL than assembled node by node.
pub(crate) fn parse_expression(sql: &str) -> Result<sqlparser::ast::Expr, ParseError> {
    let dialect = PintailDialect(MySqlDialect {}, session_parse_mode());
    Parser::new(&dialect)
        .try_with_sql(sql)
        .and_then(|mut parser| parser.parse_expr())
        .map_err(ParseError::from)
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
            "SHOW INDEX FROM `events`",
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

/// `MySqlDialect` plus custom `EXTRACT` fields.
///
/// sqlparser's `MySQL` dialect only admits its fixed `DateTimeField` set, so
/// composite units (`YEAR_MONTH`, `DAY_HOUR`, ...) die in the parser before
/// the binder can desugar them. This wrapper forwards every method
/// `MySqlDialect` overrides (sqlparser 0.62 - revisit on upgrade) and opts
/// into `allow_extract_custom`, which routes unknown fields through
/// `DateTimeField::Custom` instead.
#[derive(Debug)]
struct PintailDialect(MySqlDialect, ParseMode);

impl Dialect for PintailDialect {
    fn dialect(&self) -> std::any::TypeId {
        self.0.dialect()
    }
    fn is_identifier_start(&self, ch: char) -> bool {
        self.0.is_identifier_start(ch)
    }
    fn is_identifier_part(&self, ch: char) -> bool {
        self.0.is_identifier_part(ch)
    }
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        (self.1.ansi_quotes && ch == '"') || self.0.is_delimited_identifier_start(ch)
    }
    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        self.0.identifier_quote_style(identifier)
    }
    fn supports_string_literal_backslash_escape(&self) -> bool {
        !self.1.no_backslash_escapes
    }
    fn supports_string_literal_concatenation(&self) -> bool {
        self.0.supports_string_literal_concatenation()
    }
    fn ignores_wildcard_escapes(&self) -> bool {
        self.0.ignores_wildcard_escapes()
    }
    fn supports_numeric_prefix(&self) -> bool {
        self.0.supports_numeric_prefix()
    }
    fn supports_bitwise_shift_operators(&self) -> bool {
        self.0.supports_bitwise_shift_operators()
    }
    fn supports_multiline_comment_hints(&self) -> bool {
        self.0.supports_multiline_comment_hints()
    }
    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        (self.1.pipes_as_concat
            && parser.peek_token().token == sqlparser::tokenizer::Token::StringConcat)
            .then_some(Ok(45))
    }
    fn parse_infix(
        &self,
        parser: &mut sqlparser::parser::Parser,
        expr: &sqlparser::ast::Expr,
        precedence: u8,
    ) -> Option<Result<sqlparser::ast::Expr, sqlparser::parser::ParserError>> {
        self.0.parse_infix(parser, expr, precedence)
    }
    fn parse_statement(
        &self,
        parser: &mut sqlparser::parser::Parser,
    ) -> Option<Result<Statement, sqlparser::parser::ParserError>> {
        self.0.parse_statement(parser)
    }
    fn require_interval_qualifier(&self) -> bool {
        self.0.require_interval_qualifier()
    }
    fn supports_limit_comma(&self) -> bool {
        self.0.supports_limit_comma()
    }
    fn supports_create_table_select(&self) -> bool {
        self.0.supports_create_table_select()
    }
    fn supports_insert_set(&self) -> bool {
        self.0.supports_insert_set()
    }
    fn supports_user_host_grantee(&self) -> bool {
        self.0.supports_user_host_grantee()
    }
    fn is_table_factor_alias(
        &self,
        explicit: bool,
        kw: &sqlparser::keywords::Keyword,
        parser: &mut sqlparser::parser::Parser,
    ) -> bool {
        self.0.is_table_factor_alias(explicit, kw, parser)
    }
    fn supports_table_hints(&self) -> bool {
        self.0.supports_table_hints()
    }
    fn requires_single_line_comment_whitespace(&self) -> bool {
        self.0.requires_single_line_comment_whitespace()
    }
    fn supports_match_against(&self) -> bool {
        self.0.supports_match_against()
    }
    fn supports_select_modifiers(&self) -> bool {
        self.0.supports_select_modifiers()
    }
    fn supports_set_names(&self) -> bool {
        self.0.supports_set_names()
    }
    fn supports_comma_separated_set_assignments(&self) -> bool {
        self.0.supports_comma_separated_set_assignments()
    }
    fn supports_update_order_by(&self) -> bool {
        self.0.supports_update_order_by()
    }
    fn supports_data_type_signed_suffix(&self) -> bool {
        self.0.supports_data_type_signed_suffix()
    }
    fn supports_cross_join_constraint(&self) -> bool {
        self.0.supports_cross_join_constraint()
    }
    fn supports_double_ampersand_operator(&self) -> bool {
        self.0.supports_double_ampersand_operator()
    }
    fn supports_binary_kw_as_cast(&self) -> bool {
        self.0.supports_binary_kw_as_cast()
    }
    fn supports_comment_optimizer_hint(&self) -> bool {
        self.0.supports_comment_optimizer_hint()
    }
    fn supports_constraint_keyword_without_name(&self) -> bool {
        self.0.supports_constraint_keyword_without_name()
    }
    fn supports_key_column_option(&self) -> bool {
        self.0.supports_key_column_option()
    }
    fn allow_extract_custom(&self) -> bool {
        true
    }
}

/// A projection of connection variables or zero-argument identity functions.
/// Tables and arbitrary expressions stay on the normal query path.
#[must_use]
pub fn connection_projection(sql: &str) -> Option<Vec<(String, String)>> {
    use sqlparser::ast::{Expr, SelectItem, SetExpr};
    let Statement::Query(query) = parse_statement(sql).ok()? else {
        return None;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if !select.from.is_empty() || select.selection.is_some() {
        return None;
    }
    select
        .projection
        .iter()
        .map(|item| {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(expr) => (expr, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                _ => return None,
            };
            let text = expr.to_string();
            let simple = matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
                && text.starts_with("@@");
            if !simple
                && !["VERSION()", "DATABASE()"]
                    .iter()
                    .any(|name| text.eq_ignore_ascii_case(name))
            {
                return None;
            }
            Some((text.clone(), alias.unwrap_or(text)))
        })
        .collect()
}

/// Resolves the current-database identity in a discovery statement before binding.
pub fn resolve_database_function(statement: &mut Statement, database: &str) {
    use sqlparser::ast::{Expr, FunctionArguments, Value, VisitMut, VisitorMut};
    struct Resolve<'a>(&'a str);
    impl VisitorMut for Resolve<'_> {
        type Break = ();
        fn post_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<()> {
            if let Expr::Function(function) = expression
                && function.name.to_string().eq_ignore_ascii_case("database")
                && matches!(&function.args, FunctionArguments::List(args) if args.args.is_empty() && args.clauses.is_empty())
                && function.over.is_none()
            {
                *expression = Expr::Value(Value::SingleQuotedString(self.0.to_owned()).into());
            }
            ControlFlow::Continue(())
        }
    }
    let _ = statement.visit(&mut Resolve(database));
}
