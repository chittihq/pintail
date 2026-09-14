//! Connection-scoped SQL prepared statement commands.
use sqlparser::ast::{Expr, Value};
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

#[derive(Debug, PartialEq)]
pub enum PreparedCommand {
    Prepare {
        name: String,
        source: PreparedSource,
    },
    Execute {
        name: String,
        variables: Vec<String>,
    },
    Deallocate {
        name: String,
    },
}

#[derive(Debug, PartialEq)]
pub enum PreparedSource {
    Literal(String),
    Variable(String),
}

fn variable(expression: Expr) -> Result<String, String> {
    if let Expr::Identifier(identifier) = expression
        && let Some(name) = identifier.value.strip_prefix('@')
        && !name.is_empty()
        && !name.starts_with('@')
    {
        return Ok(name.to_ascii_lowercase());
    }
    Err("expected a user variable".into())
}

/// Recognizes SQL PREPARE, EXECUTE, DEALLOCATE and DROP PREPARE.
/// Other statement families are left to the ordinary statement parser.
#[must_use]
pub fn parse_prepared_command(sql: &str) -> Option<Result<PreparedCommand, String>> {
    let dialect = crate::PintailDialect(
        sqlparser::dialect::MySqlDialect {},
        crate::session_parse_mode(),
    );
    let tokens = crate::tokenize_mysql(sql, &dialect).ok()?;
    let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
    let prepare = parser.parse_keyword(Keyword::PREPARE);
    let execute = !prepare && parser.parse_keyword(Keyword::EXECUTE);
    let deallocate = !prepare && !execute && parser.parse_keyword(Keyword::DEALLOCATE);
    let drop_prepare = !prepare
        && !execute
        && !deallocate
        && parser.parse_keywords(&[Keyword::DROP, Keyword::PREPARE]);
    if !prepare && !execute && !deallocate && !drop_prepare {
        return None;
    }
    Some((|| {
        if deallocate {
            parser
                .expect_keyword(Keyword::PREPARE)
                .map_err(|error| error.to_string())?;
        }
        let name = parser
            .parse_identifier()
            .map_err(|error| error.to_string())?
            .value
            .to_ascii_lowercase();
        let command = if prepare {
            parser
                .expect_keyword(Keyword::FROM)
                .map_err(|error| error.to_string())?;
            let expression = parser.parse_expr().map_err(|error| error.to_string())?;
            let source = match expression {
                Expr::Value(value) => match value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        PreparedSource::Literal(text)
                    }
                    _ => return Err("expected statement text or a user variable".into()),
                },
                expression => PreparedSource::Variable(variable(expression)?),
            };
            PreparedCommand::Prepare { name, source }
        } else if execute {
            let mut variables = Vec::new();
            if parser.parse_keyword(Keyword::USING) {
                loop {
                    variables.push(variable(
                        parser.parse_expr().map_err(|error| error.to_string())?,
                    )?);
                    if !parser.consume_token(&Token::Comma) {
                        break;
                    }
                }
            }
            PreparedCommand::Execute { name, variables }
        } else {
            PreparedCommand::Deallocate { name }
        };
        let _ = parser.consume_token(&Token::SemiColon);
        if parser.peek_token().token != Token::EOF {
            return Err("unexpected tokens after prepared statement command".into());
        }
        Ok(command)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_follow_mysql_grammar_and_lexical_mode() {
        assert_eq!(
            parse_prepared_command("/* x */ PREPARE `Read` FROM @source;"),
            Some(Ok(PreparedCommand::Prepare {
                name: "read".into(),
                source: PreparedSource::Variable("source".into())
            }))
        );
        assert_eq!(
            parse_prepared_command("EXECUTE read USING @first, @SECOND"),
            Some(Ok(PreparedCommand::Execute {
                name: "read".into(),
                variables: vec!["first".into(), "second".into()]
            }))
        );
        assert_eq!(
            parse_prepared_command("DROP PREPARE read"),
            Some(Ok(PreparedCommand::Deallocate {
                name: "read".into()
            }))
        );
        assert!(parse_prepared_command("DROP TABLE read").is_none());
        for sql in [
            "PREPARE read AS SELECT 1",
            "DEALLOCATE read",
            "DROP PREPARE PREPARE read",
            "EXECUTE read USING 1",
            "EXECUTE read USING @@sql_mode",
            "EXECUTE read; SELECT 1",
            "PREPARE read FROM CONCAT('SELECT ', '1')",
        ] {
            assert!(parse_prepared_command(sql).unwrap().is_err(), "{sql}");
        }
        crate::with_parse_mode(crate::ParseMode::from_sql_mode("ANSI_QUOTES"), || {
            assert!(
                parse_prepared_command("PREPARE read FROM \"SELECT 1\"")
                    .unwrap()
                    .is_err()
            );
        });
    }
}
