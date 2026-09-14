use super::{
    Backend, parameter_literal, placeholder_preview_literals, record_prepared_refused,
    substitute_parameters,
};
use pintail_protocol::{BinaryValue, ErrorKind, OkPacket, Response};
use pintail_sql::{ParseMode, PreparedCommand, PreparedSource, with_parse_mode};
use sqlparser::ast::Value;

pub(super) struct NamedStatement {
    sql: String,
    mode: ParseMode,
}

impl Backend {
    pub(super) async fn named_statement(
        &mut self,
        command: Result<PreparedCommand, String>,
        mode: ParseMode,
    ) -> Response {
        let command = match command {
            Ok(command) => command,
            Err(error) => return Response::Error(ErrorKind::ErParseError, error),
        };
        match command {
            PreparedCommand::Prepare { name, source } => {
                if let Some(old) = self.named_prepared.remove(&name) {
                    self.prepared_bytes -= old.sql.len();
                }
                let sql = match source {
                    PreparedSource::Literal(sql) => sql,
                    PreparedSource::Variable(variable) => {
                        let value = self
                            .session
                            .lock()
                            .ok()
                            .and_then(|session| session.user_variables.get(&variable).cloned());
                        match value {
                            Some(
                                Value::SingleQuotedString(sql) | Value::DoubleQuotedString(sql),
                            ) => sql,
                            _ => {
                                return Response::Error(
                                    ErrorKind::ErParseError,
                                    "prepared statement source is not text".into(),
                                );
                            }
                        }
                    }
                };
                if let Some(refusal) = self.prepared_statement_refusal(sql.len()) {
                    record_prepared_refused();
                    return Response::Error(ErrorKind::ErMaxPreparedStmtCountReached, refusal);
                }
                let validation = with_parse_mode(mode, || {
                    let preview = substitute_parameters(&sql, &placeholder_preview_literals(&sql))?;
                    pintail_sql::parse_statement(&preview).map_err(|error| error.to_string())
                });
                if let Err(error) = validation {
                    return Response::Error(ErrorKind::ErParseError, error);
                }
                self.prepared_bytes += sql.len();
                self.named_prepared
                    .insert(name, NamedStatement { sql, mode });
            }
            PreparedCommand::Execute { name, variables } => {
                let Some(statement) = self.named_prepared.get(&name) else {
                    return unknown_statement(&name);
                };
                let parameters = match self.session.lock() {
                    Ok(session) => with_parse_mode(statement.mode, || {
                        variables
                            .iter()
                            .map(|name| {
                                match session.user_variables.get(name).unwrap_or(&Value::Null) {
                                    Value::SingleQuotedString(text)
                                    | Value::DoubleQuotedString(text) => parameter_literal(
                                        &BinaryValue::Bytes(text.as_bytes().to_vec()),
                                    ),
                                    value => Ok(value.to_string()),
                                }
                            })
                            .collect::<Result<Vec<_>, String>>()
                    }),
                    Err(error) => Err(error.to_string()),
                };
                let sql = parameters.and_then(|parameters| {
                    with_parse_mode(statement.mode, || {
                        substitute_parameters(&statement.sql, &parameters)
                    })
                });
                return match sql {
                    Ok(sql) => {
                        self.text_answer(&sql, Some(statement.mode), &statement.sql)
                            .await
                    }
                    Err(error) => Response::Error(ErrorKind::ErWrongArguments, error),
                };
            }
            PreparedCommand::Deallocate { name } => {
                let Some(statement) = self.named_prepared.remove(&name) else {
                    return unknown_statement(&name);
                };
                self.prepared_bytes -= statement.sql.len();
            }
        }
        if let Ok(mut session) = self.session.lock() {
            session.row_count = 0;
            session.conditions.clear();
            session.condition_count = 0;
        }
        Response::Ok(OkPacket::default(), String::new())
    }
}

fn unknown_statement(name: &str) -> Response {
    Response::Error(
        ErrorKind::ErUnknownStmtHandler,
        format!("Unknown prepared statement handler ({name})"),
    )
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::tests::local_backend;
    use pintail_protocol::{Handler, Response};
    use pintail_types::Value;

    #[tokio::test]
    async fn named_statements_execute_parameters_without_running_at_prepare() {
        let (_directory, mut backend) = local_backend();
        assert!(matches!(
            backend
                .query(b"CREATE TABLE clocks (duration TIME(6))")
                .await,
            Response::Ok(..)
        ));
        assert!(matches!(
            backend
                .query(b"PREPARE add_clock FROM 'INSERT INTO clocks VALUES (?)'")
                .await,
            Response::Ok(..)
        ));
        assert!(
            backend
                .execute("SELECT * FROM clocks")
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        backend.query(b"SET @duration='11:22:33.123456'").await;
        assert!(matches!(
            backend.query(b"EXECUTE add_clock USING @duration").await,
            Response::Ok(..)
        ));
        assert_eq!(
            backend.execute("SELECT * FROM clocks").await.unwrap().rows[0][0],
            Value::Utf8("11:22:33.123456".into())
        );
        assert!(matches!(
            backend.query(b"DEALLOCATE PREPARE add_clock").await,
            Response::Ok(..)
        ));
        assert!(matches!(
            backend.query(b"EXECUTE add_clock USING @duration").await,
            Response::Error(..)
        ));
    }
    #[tokio::test]
    async fn named_statement_handles_share_limits_and_reset() {
        let (_directory, mut backend) = local_backend();
        backend.limits.max_prepared_statements = 1;
        assert!(matches!(
            backend.query(b"PREPARE item FROM 'SELECT 1'").await,
            Response::Ok(..)
        ));
        assert!(backend.prepare(b"SELECT 2").await.is_err());
        assert!(matches!(
            backend.query(b"PREPARE item FROM 'SELECT 2'").await,
            Response::Ok(..)
        ));
        assert_eq!(backend.prepared_bytes, "SELECT 2".len());
        assert!(matches!(
            backend.query(b"PREPARE item FROM 'SELECT ('").await,
            Response::Error(..)
        ));
        assert!(backend.named_prepared.is_empty());
        assert_eq!(backend.prepared_bytes, 0);
        backend.prepare(b"SELECT 2").await.unwrap();
        assert!(matches!(
            backend.query(b"PREPARE item FROM 'SELECT 1'").await,
            Response::Error(..)
        ));
        backend.reset_connection().await;
        assert_eq!(backend.prepared_bytes, 0);
        assert!(backend.named_prepared.is_empty());
        assert!(matches!(
            backend.query(b"EXECUTE item").await,
            Response::Error(..)
        ));
    }

    #[tokio::test]
    async fn named_statements_capture_mode_and_read_current_parameters() {
        let (_directory, mut backend) = local_backend();
        for sql in [
            "SET sql_mode='NO_BACKSLASH_ESCAPES'",
            "SET @source='INSERT INTO notes VALUES (?)'",
            "CREATE TABLE notes (body TEXT)",
            "PREPARE append_note FROM @source",
            "SET sql_mode=''",
            "SET @body='first'",
            "EXECUTE append_note USING @body",
            "SET @body='second'",
            "EXECUTE append_note USING @body",
        ] {
            assert!(
                matches!(backend.query(sql.as_bytes()).await, Response::Ok(..)),
                "{sql}"
            );
        }
        assert!(matches!(
            backend.query(b"EXECUTE append_note").await,
            Response::Error(..)
        ));
        let rows = backend
            .execute("SELECT body FROM notes ORDER BY body")
            .await
            .unwrap()
            .rows;
        assert_eq!(
            rows,
            vec![
                vec![Value::Utf8("first".into())],
                vec![Value::Utf8("second".into())]
            ]
        );
        let escaped = "a\\b'c";
        {
            let mut session = backend.session.lock().unwrap();
            std::sync::Arc::make_mut(&mut session.user_variables).insert(
                "body".into(),
                sqlparser::ast::Value::SingleQuotedString(escaped.into()),
            );
        }
        assert!(matches!(
            backend.query(b"EXECUTE append_note USING @body").await,
            Response::Ok(..)
        ));
        assert_eq!(
            backend
                .execute("SELECT body FROM notes ORDER BY body LIMIT 1")
                .await
                .unwrap()
                .rows[0][0],
            Value::Utf8(escaped.into())
        );
        let (_other_directory, mut other) = local_backend();
        assert!(matches!(
            other.query(b"EXECUTE append_note USING @body").await,
            Response::Error(..)
        ));
        backend.query(b"SET @counter=0").await;
        assert!(matches!(
            backend
                .query(b"PREPARE advance FROM 'SELECT @counter:=@counter+1'")
                .await,
            Response::Ok(..)
        ));
        assert_eq!(
            backend.execute("SELECT @counter").await.unwrap().rows[0][0],
            Value::Int64(0)
        );
        let response = backend.query(b"EXECUTE advance").await;
        assert!(!matches!(response, Response::Error(..)));
        assert_eq!(
            backend.execute("SELECT @counter").await.unwrap().rows[0][0],
            Value::Int64(1)
        );
    }
}
