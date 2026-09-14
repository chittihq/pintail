//! Reading a connection's current diagnostics area into user variables.
use super::{Condition, Session};
use sqlparser::ast::{Expr, Value};

fn keyword<'a>(text: &'a str, wanted: &str) -> Option<&'a str> {
    let text = text.trim_start();
    let head = text.get(..wanted.len())?;
    let tail = &text[wanted.len()..];
    (head.eq_ignore_ascii_case(wanted) && tail.chars().next().is_none_or(char::is_whitespace))
        .then(|| tail.trim_start())
}

pub(super) fn apply(sql: &str, session: &mut Session) -> Option<Result<(), String>> {
    let rest = keyword(sql.trim().trim_end_matches(';'), "GET")?;
    let rest = keyword(rest, "CURRENT").unwrap_or(rest);
    let rest = keyword(rest, "DIAGNOSTICS")?;
    Some(assign(rest, session))
}

fn assign(mut rest: &str, session: &mut Session) -> Result<(), String> {
    let mut condition = None;
    if let Some(tail) = keyword(rest, "CONDITION") {
        let (number, tail) = tail
            .split_once(char::is_whitespace)
            .ok_or_else(|| "GET DIAGNOSTICS requires assignments".to_owned())?;
        let number = number.strip_prefix('@').map_or_else(
            || number.parse::<usize>().ok(),
            |name| match session.user_variables.get(&name.to_ascii_lowercase()) {
                Some(Value::Number(value, _) | Value::SingleQuotedString(value)) => {
                    value.parse().ok()
                }
                _ => None,
            },
        );
        condition = Some(number.unwrap_or(0));
        rest = tail.trim_start();
    }
    let statement =
        pintail_sql::parse_statement(&format!("SET {rest}")).map_err(|error| error.to_string())?;
    let assignments = pintail_sql::user_variable_assignments(&statement)
        .ok_or_else(|| "GET DIAGNOSTICS targets must be user variables".to_owned())?;
    let items = assignments
        .into_iter()
        .map(|(name, expression)| {
            let Expr::Identifier(item) = expression else {
                return Err("Invalid diagnostics item".to_owned());
            };
            Ok((name, item.value.to_ascii_uppercase()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let selected = if let Some(number) = condition {
        if let Some(condition) = number
            .checked_sub(1)
            .and_then(|index| session.conditions.get(index))
            .cloned()
        {
            Some(condition)
        } else {
            session.condition_count = session.condition_count.saturating_add(1);
            if session.conditions.len() < super::MAX_LISTED_CONDITIONS {
                session.conditions.push(Condition {
                    level: "Error",
                    code: 1758,
                    sql_state: b"35000",
                    message: "Invalid condition number".to_owned(),
                });
            }
            return Ok(());
        }
    } else {
        None
    };
    let mut variables = (*session.user_variables).clone();
    for (name, item) in items {
        let value = if let Some(condition) = &selected {
            condition_item(condition, &item)?
        } else {
            match item.as_str() {
                "NUMBER" => Value::Number(session.condition_count.to_string(), false),
                "ROW_COUNT" => Value::Number(session.row_count.to_string(), false),
                _ => return Err(format!("Unknown statement diagnostics item: {item}")),
            }
        };
        variables.insert(name, value);
    }
    session.user_variables = std::sync::Arc::new(variables);
    Ok(())
}

fn condition_item(condition: &Condition, item: &str) -> Result<Value, String> {
    let state = std::str::from_utf8(condition.sql_state).expect("SQLSTATE is ASCII");
    let text = match item {
        "MYSQL_ERRNO" => return Ok(Value::Number(condition.code.to_string(), false)),
        "MESSAGE_TEXT" => condition.message.as_str(),
        "RETURNED_SQLSTATE" => state,
        "CLASS_ORIGIN" | "SUBCLASS_ORIGIN" => {
            if state.starts_with("HY") {
                "MySQL"
            } else {
                "ISO 9075"
            }
        }
        "CONSTRAINT_CATALOG" | "CONSTRAINT_SCHEMA" | "CONSTRAINT_NAME" | "CATALOG_NAME"
        | "SCHEMA_NAME" | "TABLE_NAME" | "COLUMN_NAME" | "CURSOR_NAME" => "",
        _ => return Err(format!("Unknown condition diagnostics item: {item}")),
    };
    Ok(Value::SingleQuotedString(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_read_statement_and_condition_fields_without_clearing_them() {
        let mut session = Session {
            row_count: 3,
            condition_count: 1,
            conditions: vec![Condition {
                level: "Warning",
                code: 1365,
                sql_state: b"22012",
                message: "Division by 0".into(),
            }],
            ..Session::default()
        };
        apply(
            "GET CURRENT DIAGNOSTICS @n=NUMBER,@rows=ROW_COUNT",
            &mut session,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            session.user_variables["n"],
            Value::Number("1".into(), false)
        );
        assert_eq!(
            session.user_variables["rows"],
            Value::Number("3".into(), false)
        );
        apply("GET DIAGNOSTICS CONDITION @n @message=MESSAGE_TEXT,@code=MYSQL_ERRNO,@state=RETURNED_SQLSTATE", &mut session).unwrap().unwrap();
        assert_eq!(
            session.user_variables["message"],
            Value::SingleQuotedString("Division by 0".into())
        );
        assert_eq!(
            session.user_variables["code"],
            Value::Number("1365".into(), false)
        );
        assert_eq!(
            session.user_variables["state"],
            Value::SingleQuotedString("22012".into())
        );
        assert_eq!(session.condition_count, 1);
        assert_eq!(session.row_count, 3);
        apply(
            "GET DIAGNOSTICS CONDITION 9 @message=MESSAGE_TEXT",
            &mut session,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.condition_count, 2);
        assert_eq!(session.conditions[1].code, 1758);
        assert_eq!(
            session.user_variables["message"],
            Value::SingleQuotedString("Division by 0".into())
        );
    }
    #[tokio::test]
    async fn row_counts_follow_the_real_write_and_query_path() {
        use super::super::{Authenticated, Backend};
        use pintail_protocol::Handler;
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let metadata = pintail_meta::MetaStore::open(&metadata_path).unwrap();
        metadata
            .create_local_database("local", "scratch", "2026-01-01T00:00:00Z")
            .unwrap();
        std::fs::create_dir_all(directory.path().join("databases/local/tables")).unwrap();
        pintail_write::LocalDatabase::new(directory.path(), &metadata_path, "local")
            .recover()
            .unwrap();
        let mut backend = Backend::new(
            directory.path(),
            &metadata_path,
            64 * 1024 * 1024,
            super::super::WireLimits::default(),
        );
        *backend.authentication.lock().unwrap() = Some(Authenticated {
            database_id: "local".into(),
            database_name: "scratch".into(),
            key_name: "test".into(),
            local: true,
            source_time_zone: None,
        });
        for (sql, expected) in [
            ("CREATE TABLE values_table(id INT)", 0),
            ("INSERT INTO values_table VALUES(1),(2),(3)", 3),
            ("SELECT id FROM values_table", -1),
        ] {
            backend.execute(sql).await.unwrap();
            assert!(matches!(
                backend.query(b"GET DIAGNOSTICS @rows=ROW_COUNT").await,
                pintail_protocol::Response::Ok(..)
            ));
            assert_eq!(
                backend.session.lock().unwrap().user_variables["rows"],
                Value::Number(expected.to_string(), false)
            );
        }
        assert!(matches!(
            backend.query(b"SET @value=1").await,
            pintail_protocol::Response::Ok(..)
        ));
        assert_eq!(backend.session.lock().unwrap().row_count, 0);
        let result = backend.execute("SELECT ROW_COUNT()").await.unwrap();
        assert_eq!(result.rows[0][0], pintail_types::Value::Int64(0));
        assert_eq!(backend.session.lock().unwrap().row_count, -1);
        for _ in 0..2 {
            let result = backend.execute("SELECT CAST(-19999999999999999999 AS SIGNED), CAST(-19999999999999999999 AS SIGNED)").await.unwrap();
            assert_eq!(
                result.rows[0],
                vec![pintail_types::Value::Int64(i64::MIN); 2]
            );
            assert!(matches!(
                backend.query(b"GET DIAGNOSTICS @count=NUMBER").await,
                pintail_protocol::Response::Ok(..)
            ));
            assert!(matches!(backend.query(b"GET DIAGNOSTICS CONDITION 1 @code=MYSQL_ERRNO,@state=RETURNED_SQLSTATE,@message=MESSAGE_TEXT").await, pintail_protocol::Response::Ok(..)));
            let session = backend.session.lock().unwrap();
            assert_eq!(
                session.user_variables["count"],
                Value::Number("2".into(), false)
            );
            assert_eq!(
                session.user_variables["code"],
                Value::Number("1292".into(), false)
            );
            assert_eq!(
                session.user_variables["state"],
                Value::SingleQuotedString("22007".into())
            );
            assert_eq!(
                session.user_variables["message"],
                Value::SingleQuotedString(
                    "Truncated incorrect DECIMAL value: '-19999999999999999999'".into()
                )
            );
        }
        backend.execute("SELECT 1").await.unwrap();
        assert_eq!(backend.session.lock().unwrap().condition_count, 0);
    }
}
