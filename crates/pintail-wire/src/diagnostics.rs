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

pub(super) async fn select_into(
    backend: &super::Backend,
    query: &str,
    targets: Vec<String>,
) -> pintail_protocol::Response {
    use pintail_protocol::{ErrorKind, OkPacket, Response};
    let output = match backend.execute(query).await {
        Ok(output) => output,
        Err(error) => return Response::Error(super::error_kind(&error), error.to_string()),
    };
    let Ok(mut session) = backend.session.lock() else {
        return Response::Error(
            ErrorKind::ErUnknownError,
            "Session lock is unavailable".into(),
        );
    };
    let error = |session: &mut Session, kind: ErrorKind, message: &str| {
        session.row_count = -1;
        session.condition_count = 1;
        session.conditions = vec![Condition {
            level: "Error",
            code: kind.code(),
            sql_state: kind.sql_state(),
            message: message.into(),
        }];
        Response::Error(kind, message.into())
    };
    if output.fields.len() != targets.len() {
        return error(
            &mut session,
            ErrorKind::ErWrongNumberOfColumnsInSelect,
            "The used SELECT statements have a different number of columns",
        );
    }
    let count = output.rows.len();
    if let Some(row) = output.rows.into_values().into_iter().next() {
        let mut variables = (*session.user_variables).clone();
        for ((name, value), field) in targets.into_iter().zip(row).zip(output.fields) {
            variables.insert(name, super::user_variable_literal(value, field.data_type));
        }
        session.user_variables = std::sync::Arc::new(variables);
    }
    if count > 1 {
        return error(
            &mut session,
            ErrorKind::ErTooManyRows,
            "Result consisted of more than one row",
        );
    }
    session.row_count = i64::try_from(count).unwrap_or(0);
    if count == 0 {
        session.condition_count = session.condition_count.saturating_add(1);
        if session.conditions.len() < super::MAX_LISTED_CONDITIONS {
            session.conditions.push(Condition {
                level: "Warning",
                code: 1329,
                sql_state: b"02000",
                message: "No data - zero rows fetched, selected, or processed".into(),
            });
        }
    }
    Response::Ok(
        OkPacket {
            affected_rows: u64::try_from(count).unwrap_or(0),
            warnings: u16::try_from(session.condition_count).unwrap_or(u16::MAX),
            ..OkPacket::default()
        },
        String::new(),
    )
}

#[cfg(test)]
pub(super) mod tests {
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
    async fn time_conversion_bounds_hours_and_keeps_numeric_fractions() {
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT TIME('1000009000:10:10.1999999999999'), TIME('10000090000:10:10'), CAST(1800000000 AS TIME), TIME(154559.616 + 0e0)").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Utf8("838:59:59.000000".into()),
                pintail_types::Value::Null,
                pintail_types::Value::Null,
                pintail_types::Value::Utf8("15:45:59.616000".into()),
            ]
        );
    }

    #[tokio::test]
    async fn spring_forward_gaps_resolve_to_the_transition() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        backend.query(b"SET time_zone='Europe/Moscow'").await;
        let result = backend.execute("SELECT CONVERT_TZ('2003-03-30 02:30:00', 'MET', 'UTC'), UNIX_TIMESTAMP('2003-03-30 02:30:00')").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Utf8("2003-03-30 01:00:00".into()),
                pintail_types::Value::UInt64(1_048_978_800)
            ]
        );
    }

    #[tokio::test]
    async fn packed_datetime_casts_keep_fractional_seconds() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        backend
            .query(b"SET sql_mode='TIME_TRUNCATE_FRACTIONAL'")
            .await;
        let result = backend.execute("SELECT CAST(20010101101010.9999994 AS DATETIME), CAST(20010101101010.9999995 AS DATETIME(6))").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Utf8("2001-01-01 10:10:10".into()),
                pintail_types::Value::Utf8("2001-01-01 10:10:10.999999".into())
            ]
        );
    }

    #[tokio::test]
    async fn datetime_comparison_bounds_use_microsecond_session_precision() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        for (mode, expected) in [
            ("", "2001-01-01 00:00:01.000000"),
            ("TIME_TRUNCATE_FRACTIONAL", "2001-01-01 00:00:00.999999"),
        ] {
            backend
                .query(format!("SET sql_mode='{mode}'").as_bytes())
                .await;
            let result = backend.execute("SELECT d FROM (SELECT CAST('2001-01-01 00:00:00.999999' AS DATETIME(6)) AS d UNION ALL SELECT CAST('2001-01-01 00:00:01' AS DATETIME(6))) clocks WHERE d='2001-01-01 00:00:00.9999998'").await.unwrap();
            assert_eq!(
                result.rows,
                vec![vec![pintail_types::Value::Utf8(expected.into())]],
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn datetime_casts_round_once_or_truncate_by_session_mode() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        for (mode, expected) in [
            ("", "2001-01-01 10:10:11.000000"),
            ("TIME_TRUNCATE_FRACTIONAL", "2001-01-01 10:10:10.999999"),
        ] {
            backend
                .query(format!("SET sql_mode='{mode}'").as_bytes())
                .await;
            let result = backend.execute("SELECT CAST(20010101101010.9999995 AS DATETIME(6)), CAST('2001-01-01 10:10:10.9999995' AS DATETIME(6))").await.unwrap();
            assert_eq!(
                result.rows[0],
                vec![pintail_types::Value::Utf8(expected.into()); 2],
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn logical_xor_and_bit_shifts_follow_mysql_precedence() {
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT NULL XOR 1 AND 0, 240 & 15 << 4, 15 & 240 >> 4")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Null,
                pintail_types::Value::UInt64(240),
                pintail_types::Value::UInt64(15)
            ]
        );
    }

    #[tokio::test]
    async fn arithmetic_hex_literals_use_their_unsigned_numeric_value() {
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT 2 * 0x40 | 0x0F, 0x65 - 0x0F ^ 0x55, (0x65 - 0x0F) ^ 0x55")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::UInt64(143),
                pintail_types::Value::UInt64(11),
                pintail_types::Value::UInt64(3)
            ]
        );
    }

    #[tokio::test]
    async fn empty_json_object_aggregate_is_null() {
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT JSON_OBJECTAGG('entry', value) FROM (SELECT 1 AS value) entries WHERE value > 2").await.unwrap();
        assert_eq!(result.rows[0], vec![pintail_types::Value::Null]);
        let result = backend
            .execute("SELECT JSON_OBJECTAGG('entry', value) FROM (SELECT NULL AS value) entries")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![pintail_types::Value::Utf8("{\"entry\": null}".into())]
        );
    }

    #[tokio::test]
    async fn zero_scale_decimal_literals_have_no_trailing_point() {
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT 10., 10.0, -10.").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Utf8("10".into()),
                pintail_types::Value::Utf8("10.0".into()),
                pintail_types::Value::Utf8("-10".into())
            ]
        );
    }

    #[tokio::test]
    async fn parenthesized_column_projections_keep_the_column_name() {
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT DISTINCT((label)) FROM (SELECT 1 AS label) entries")
            .await
            .unwrap();
        assert_eq!(result.fields[0].name, "label");
    }

    #[tokio::test]
    async fn parenthesized_queries_keep_inner_order_and_limit() {
        let (_directory, backend) = local_backend();
        let source = "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5";
        let sql = format!(
            "(SELECT n FROM ({source}) entries ORDER BY n DESC LIMIT 3) ORDER BY n LIMIT 2"
        );
        let result = backend.execute(&sql).await.unwrap();
        assert_eq!(
            result.rows,
            vec![
                vec![pintail_types::Value::Int64(3)],
                vec![pintail_types::Value::Int64(4)]
            ]
        );
        let result = backend
            .execute(&format!(
                "(SELECT n FROM ({source}) entries ORDER BY n DESC LIMIT 2)"
            ))
            .await
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                vec![pintail_types::Value::Int64(5)],
                vec![pintail_types::Value::Int64(4)]
            ]
        );
    }

    #[tokio::test]
    async fn variable_reads_keep_the_type_at_statement_start() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        backend.query(b"SET @a='x', @b='y'").await;
        let result = backend
            .execute("SELECT @a:=10, @b:=2, @a > @b, @a < @b")
            .await
            .unwrap();
        assert_eq!(
            &result.rows[0][2..],
            &[
                pintail_types::Value::Boolean(false),
                pintail_types::Value::Boolean(true)
            ]
        );
        let result = backend
            .execute("SELECT @a:='10', @b:='2', @a > @b, @a < @b")
            .await
            .unwrap();
        assert_eq!(
            &result.rows[0][2..],
            &[
                pintail_types::Value::Boolean(true),
                pintail_types::Value::Boolean(false)
            ]
        );
    }

    #[tokio::test]
    async fn variable_assignments_flow_through_union_branches() {
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT @counter:=1 UNION SELECT @counter:=@counter+1")
            .await
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                vec![pintail_types::Value::float64(1.0)],
                vec![pintail_types::Value::float64(2.0)]
            ]
        );
        let result = backend
            .execute("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3e0")
            .await
            .unwrap();
        assert_eq!(
            result.rows,
            [1.0, 2.0, 3.0]
                .map(|value| vec![pintail_types::Value::float64(value)])
                .to_vec()
        );
    }

    #[tokio::test]
    async fn comparisons_before_variables_need_no_whitespace() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        backend.query(b"SET @a=1, @b=2").await;
        let result = backend.execute("SELECT @a<@b, @b>@a").await.unwrap();
        assert_eq!(result.rows[0], vec![pintail_types::Value::Boolean(true); 2]);
    }

    #[tokio::test]
    async fn short_numeric_calendar_casts_use_numeric_date_rules() {
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT CAST(1111 AS DATE), CAST(011111 AS DATE), CAST('1111' AS DATE), CAST(1111 AS DATETIME), CAST('1111' AS DATETIME), CAST(1111.1234567 AS DATETIME(6))")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Utf8("2000-11-11".into()),
                pintail_types::Value::Utf8("2001-11-11".into()),
                pintail_types::Value::Null,
                pintail_types::Value::Utf8("2000-11-11 00:00:00".into()),
                pintail_types::Value::Null,
                pintail_types::Value::Utf8("2000-11-11 00:00:00.000000".into())
            ]
        );
    }

    #[tokio::test]
    async fn date_format_anchors_typed_time_to_the_statement_date() {
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT DATE_FORMAT(CAST('09:00' AS TIME), '%l.%i %p'), DATE_FORMAT(CAST('09:00' AS TIME), '%Y-%m-%d'), CURDATE(), DATE_FORMAT('09:00', '%H:%i')").await.unwrap();
        assert_eq!(
            result.rows[0][0],
            pintail_types::Value::Utf8("9.00 AM".into())
        );
        assert_eq!(result.rows[0][1], result.rows[0][2]);
        assert_eq!(result.rows[0][3], pintail_types::Value::Null);
    }

    #[tokio::test]
    async fn fixed_decimal_float_comparisons_use_declared_precision() {
        let (_directory, backend) = local_backend();
        backend
            .execute("CREATE TABLE amounts (v DOUBLE(8,2))")
            .await
            .unwrap();
        backend
            .execute("INSERT INTO amounts VALUES (0.1),(0.2),(-0.3)")
            .await
            .unwrap();
        let result = backend
            .execute("SELECT SUM(v)=0, SUM(v)<>0, SUM(v)<=>0, SUM(v)=0e0 FROM amounts")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            [true, false, true, false].map(pintail_types::Value::Boolean)
        );
        let derived = backend
            .execute(
                "SELECT s=0, s<>0, s<=>0, s=0e0 FROM (SELECT SUM(v) AS s FROM amounts) AS totals",
            )
            .await
            .unwrap();
        assert_eq!(derived.rows, result.rows);
        assert_eq!(
            backend
                .execute("SELECT SUM(v) AS s FROM amounts HAVING s<>0")
                .await
                .unwrap()
                .rows
                .len(),
            0
        );
        assert_eq!(
            backend
                .execute("SELECT SUM(v) AS s FROM amounts HAVING s<=>0")
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        let nulls = backend
            .execute("SELECT SUM(v)=0, SUM(v)<=>0, SUM(v)<=>NULL FROM amounts WHERE FALSE")
            .await
            .unwrap();
        assert_eq!(
            nulls.rows[0],
            vec![
                pintail_types::Value::Null,
                pintail_types::Value::Boolean(false),
                pintail_types::Value::Boolean(true)
            ]
        );
    }

    #[tokio::test]
    async fn grouped_variable_reads_use_group_input_values() {
        use pintail_protocol::Handler;
        use pintail_types::Value;
        let (_directory, mut backend) = local_backend();
        backend
            .execute("CREATE TABLE buckets (n INT)")
            .await
            .unwrap();
        backend
            .execute("INSERT INTO buckets VALUES (1),(2),(2),(3),(3),(3)")
            .await
            .unwrap();
        for sql in [
            "SELECT @a, @a:=@a+COUNT(*), COUNT(*), @a FROM buckets GROUP BY n ORDER BY n",
            "SELECT @a+0, @a:=@a+0+COUNT(*), COUNT(*), @a+0 FROM buckets GROUP BY n ORDER BY n",
        ] {
            backend.query(b"SET @a=0").await;
            let result = backend.execute(sql).await.unwrap();
            assert_eq!(result.rows.len(), 3);
            for row in &result.rows {
                assert_eq!(row[0], Value::Int64(0));
                assert_eq!(row[3], Value::Int64(0));
                assert_eq!(row[1], row[2]);
            }
        }
        backend.query(b"SET @a=0").await;
        for initial in [Value::Int64(0), Value::Utf8("hello again".into())] {
            let result = backend.execute("SELECT @a, @a:='hello', @a, @a:=3, @a, @a:='hello again' FROM buckets GROUP BY n").await.unwrap();
            for row in &result.rows {
                for index in [0, 2, 4] {
                    assert_eq!(row[index], initial);
                }
            }
        }
    }

    #[tokio::test]
    async fn time_arithmetic_recognizes_packed_numeric_datetimes() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT SUBTIME(20120519090607, '1 1:1:1.000002'), SUBTIME(20120519090607 | 20120519090607, '1 1:1:1.000002'), SUBTIME(120120519090607, '1 1:1:1.000002'), SUBTIME(123456, '00:00:01')").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                Value::Utf8("2012-05-18 08:05:05.999998".into()),
                Value::Utf8("2012-05-18 08:05:05.999998".into()),
                Value::Null,
                Value::Utf8("12:34:55".into())
            ]
        );
        let result = backend.execute("SELECT SUBTIME('120519090607','00:00:01'), SUBTIME(123456.1,'00:00:01'), SUBTIME(9000000,'00:00:01'), SUBTIME('9000000','00:00:01')").await.unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                Value::Utf8("2012-05-19 09:06:06".into()),
                Value::Utf8("12:34:55.100000".into()),
                Value::Null,
                Value::Utf8("838:59:58".into())
            ]
        );
    }

    #[tokio::test]
    async fn parenthesized_select_into_assigns_the_outer_query_result() {
        use pintail_protocol::{Handler, Response};
        let (_directory, mut backend) = local_backend();
        for (sql, expected) in [
            ("(SELECT 1) LIMIT 1 INTO @var", 1),
            ("(SELECT 2 AS c) ORDER BY c INTO @var", 2),
            ("(SELECT 3 AS c) ORDER BY c LIMIT 1 INTO @var", 3),
            ("(SELECT 4) INTO @var", 4),
        ] {
            assert!(matches!(
                backend.query(sql.as_bytes()).await,
                Response::Ok(..)
            ));
            let result = backend.execute("SELECT @var").await.unwrap();
            assert_eq!(result.rows[0][0], pintail_types::Value::Int64(expected));
        }
    }

    pub(in crate::server) fn local_backend() -> (tempfile::TempDir, super::super::Backend) {
        use super::super::{Authenticated, Backend};
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
        let backend = Backend::new(
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
        (directory, backend)
    }

    #[tokio::test]
    async fn right_join_wildcards_follow_written_relation_order() {
        let (_directory, backend) = local_backend();
        for sql in [
            "SELECT * FROM (SELECT 1 AS first_value) AS a RIGHT JOIN (SELECT 2 AS second_value) AS b ON a.first_value=b.second_value",
            "SELECT * FROM (SELECT 1 AS first_value) AS a RIGHT JOIN (SELECT 2 AS second_value) AS b ON a.first_value=b.second_value RIGHT JOIN (SELECT 3 AS third_value) AS c ON b.second_value=c.third_value",
        ] {
            let result = backend.execute(sql).await.unwrap();
            assert_eq!(result.fields[0].name, "first_value");
            assert_eq!(result.fields[1].name, "second_value");
            assert_eq!(result.rows[0][0], pintail_types::Value::Null);
            assert_eq!(
                result.rows[0].last(),
                Some(&pintail_types::Value::Int64(if result.fields.len() == 2 {
                    2
                } else {
                    3
                }))
            );
        }
    }

    #[tokio::test]
    async fn nested_natural_joins_keep_only_visible_wildcard_columns() {
        let (_directory, backend) = local_backend();
        let source = "(SELECT 3 AS x, 1 AS y) AS a NATURAL JOIN ((SELECT 1 AS y, 11 AS z) AS b NATURAL JOIN (SELECT 11 AS z, 4 AS w) AS c)";
        let result = backend
            .execute(&format!("SELECT * FROM {source}"))
            .await
            .unwrap();
        assert_eq!(
            result
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["y", "x", "z", "w"]
        );
        let result = backend
            .execute(&format!("SELECT y, b.z, c.z FROM {source}"))
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![
                pintail_types::Value::Int64(1),
                pintail_types::Value::Int64(11),
                pintail_types::Value::Int64(11)
            ]
        );
    }

    #[tokio::test]
    async fn having_prefers_a_group_key_over_a_conflicting_projection_alias() {
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT v*0 AS v FROM (SELECT 1 AS v UNION ALL SELECT 2 UNION ALL SELECT 3) AS numbers GROUP BY v HAVING v<>0").await.unwrap();
        assert_eq!(result.rows.len(), 3);
        let result = backend.execute("SELECT COUNT(*) AS v FROM (SELECT 1 AS v UNION ALL SELECT 1 UNION ALL SELECT 3) AS numbers GROUP BY v HAVING v>1").await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], pintail_types::Value::UInt64(1));
    }

    #[tokio::test]
    async fn grouped_join_assignments_observe_rows_before_group_materialization() {
        use pintail_protocol::Handler;
        use pintail_types::Value;
        let (_directory, mut backend) = local_backend();
        for sql in [
            "CREATE TABLE samples (bucket INT, label VARCHAR(20))",
            "CREATE TABLE names (label VARCHAR(20), bucket INT)",
            "INSERT INTO samples VALUES (1,'first'),(1,'second'),(2,'third'),(2,'fourth')",
            "INSERT INTO names VALUES ('first',1),('second',1),('third',2),('fourth',2)",
            "SET sql_mode=''",
        ] {
            assert!(
                matches!(
                    backend.query(sql.as_bytes()).await,
                    pintail_protocol::Response::Ok(..)
                ),
                "{sql}"
            );
        }
        for projection in ["@seen:=n.label", "CONCAT(@seen:=n.label)"] {
            backend.execute(&format!("SELECT n.bucket,{projection},COUNT(DISTINCT n.label) FROM samples s LEFT JOIN names n ON s.label=n.label GROUP BY n.bucket HAVING COUNT(DISTINCT n.label)>1")).await.unwrap();
            let result = backend.execute("SELECT @seen").await.unwrap();
            assert_eq!(result.rows[0][0], Value::Utf8("fourth".to_owned()));
        }
    }

    #[tokio::test]
    async fn sorted_join_materializes_decimal_projection_scale() {
        use pintail_protocol::Handler;
        use pintail_types::Value;
        let (_directory, mut backend) = local_backend();
        for sql in [
            "CREATE TABLE metrics (id INT, amount INT)",
            "CREATE TABLE details (id INT)",
            "INSERT INTO metrics VALUES (1,1),(2,2)",
            "INSERT INTO details VALUES (1),(1),(2)",
        ] {
            assert!(
                matches!(
                    backend.query(sql.as_bytes()).await,
                    pintail_protocol::Response::Ok(..)
                ),
                "{sql}"
            );
        }
        let derived =
            "(SELECT id, IF(amount > 5000, 1 / amount, 5000) AS n FROM metrics) AS values_by_id";
        for (sql, expected) in [
            (
                format!("SELECT n FROM {derived} JOIN details USING (id) ORDER BY 1"),
                "5000.0000",
            ),
            (
                format!("SELECT CONCAT(n) FROM {derived} JOIN details USING (id) ORDER BY 1"),
                "5000",
            ),
            (
                format!("SELECT n FROM {derived} JOIN details USING (id)"),
                "5000",
            ),
            (format!("SELECT n FROM {derived} ORDER BY 1"), "5000"),
        ] {
            let result = backend.execute(&sql).await.unwrap();
            for row in result.rows.into_values() {
                let text = match &row[0] {
                    Value::Int64(value) => value.to_string(),
                    Value::DecimalAverage(value) => value.label.clone(),
                    Value::Utf8(text) => text.clone(),
                    other => panic!("unexpected {other:?}"),
                };
                assert_eq!(text, expected, "{sql}");
            }
        }
    }

    #[tokio::test]
    async fn permissive_grouping_selects_a_representative_row() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        for sql in [
            "CREATE TABLE samples (bucket INT, label VARCHAR(20))",
            "INSERT INTO samples VALUES (1, 'first'), (1, 'second'), (2, 'third')",
            "SET sql_mode=''",
        ] {
            assert!(
                matches!(
                    backend.query(sql.as_bytes()).await,
                    pintail_protocol::Response::Ok(..)
                ),
                "{sql}"
            );
        }
        let result = backend
            .execute("SELECT bucket, label, COUNT(*) FROM samples GROUP BY bucket ORDER BY bucket")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0][1],
            pintail_types::Value::Utf8("first".to_owned())
        );
        assert_eq!(
            result.rows[1][1],
            pintail_types::Value::Utf8("third".to_owned())
        );
        backend.query(b"SET sql_mode='ONLY_FULL_GROUP_BY'").await;
        assert!(
            backend
                .execute("SELECT bucket, label, COUNT(*) FROM samples GROUP BY bucket")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn integer_time_casts_round_the_clock_before_encoding_digits() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT CAST(TIME'11:22:59.9' AS SIGNED), CAST(TIME'-11:22:59.9' AS SIGNED), TIME'-11:22:59.9' << 0, CAST(TIME'11:22:59.9' AS DECIMAL(10,1))").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Int64(112_300),
                Value::Int64(-112_300),
                Value::UInt64(0_u64.wrapping_sub(112_300)),
                Value::Utf8("112259.9".into())
            ]]
        );
    }

    #[tokio::test]
    async fn time_literals_accept_day_prefix_spacing_and_short_clocks() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT TIME'1  01:01:01', TIME'1  01:01', TIME'1  01', TIME'0  01', TIME'1 '")
            .await
            .unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("25:01:01".into()),
                Value::Utf8("25:01:00".into()),
                Value::Utf8("25:00:00".into()),
                Value::Utf8("01:00:00".into()),
                Value::Utf8("00:00:01".into())
            ]]
        );
    }

    #[tokio::test]
    async fn date_column_bounds_accept_mixed_separators() {
        let (_directory, backend) = local_backend();
        let equal = backend.execute("SELECT day FROM (SELECT DATE'2001-01-01' AS day) AS dates WHERE day < '2001-01/01'").await.unwrap();
        assert!(equal.rows.is_empty());
        let later = backend
            .execute(
                "SELECT day FROM (SELECT DATE'2001-01-01' AS day) AS dates WHERE day < '01-4:15'",
            )
            .await
            .unwrap();
        assert_eq!(later.rows.len(), 1);
    }

    #[tokio::test]
    async fn temporal_literals_accept_relaxed_separators() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        for (literal, expected) in [
            ("DATE'01:01:01'", "2001-01-01"),
            ("TIMESTAMP'2010-01-01 00'", "2010-01-01 00:00:00"),
            ("TIMESTAMP'2010-01-01 10:10:10.'", "2010-01-01 10:10:10"),
            ("TIMESTAMP'2021-07-15 23:01:02  '", "2021-07-15 23:01:02"),
            ("TIMESTAMP'2021//07-15 23..01:02'", "2021-07-15 23:01:02"),
            ("TIMESTAMP'2021-07-15 23:01..02'", "2021-07-15 23:01:02"),
            ("TIMESTAMP'2021-07-17.18:45:00'", "2021-07-17 18:45:00"),
            ("TIMESTAMP'20211018.121000'", "2021-10-18 12:10:00"),
            ("TIME'10:10:10.12'", "10:10:10.12"),
            ("TIME'10:11.12'", "10:11:00.12"),
            ("CAST(TIME'10:10:10.995' AS TIME(2))", "10:10:11.00"),
            (
                "CAST('2015-01-15 23-24:25' AS DATETIME)",
                "2015-01-15 23:24:25",
            ),
        ] {
            let result = backend.execute(&format!("SELECT {literal}")).await.unwrap();
            assert_eq!(result.rows[0][0], Value::Utf8(expected.into()), "{literal}");
        }
        let clock = backend.execute("SELECT HOUR('01:02:03')").await.unwrap();
        assert_eq!(clock.rows[0][0], Value::Int64(1));
    }

    #[tokio::test]
    async fn explicit_timestamp_offsets_resolve_in_the_connection_zone() {
        use pintail_protocol::Handler;
        use pintail_types::Value;
        let (_directory, mut backend) = local_backend();
        backend.query(b"SET time_zone='+01:00'").await;
        let result = backend.execute("SELECT TIMESTAMP'2015-01-01 10:10:10.1+05:30', TIMESTAMP'2015-01-01 10:10:10.0000009+05:30', TIME('2015-01-01 00:30:00+05:30'), DATE('2015-01-01 00:30:00+05:30')").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("2015-01-01 05:40:10.1".into()),
                Value::Utf8("2015-01-01 05:40:10.000001".into()),
                Value::Utf8("20:00:00".into()),
                Value::Utf8("2014-12-31".into())
            ]]
        );
        backend.query(b"SET time_zone='+00:00'").await;
        let result = backend
            .execute("SELECT TIMESTAMP'2015-01-01 10:10:10+05:30'")
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], Value::Utf8("2015-01-01 04:40:10".into()));
        let epoch = backend
            .execute("SELECT UNIX_TIMESTAMP('2015-11-13 23:59:59+02:00')")
            .await
            .unwrap();
        assert_eq!(epoch.rows[0][0], Value::UInt64(1_447_451_999));
    }

    #[tokio::test]
    async fn second_intervals_preserve_fractional_amounts() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT TIME'00:00:00.1' + INTERVAL 1.25 SECOND, TIME'-10:00:00.1' - INTERVAL 1.1 SECOND, TIMESTAMP'2024-02-29 23:59:59.9' + INTERVAL 0.25 SECOND").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("00:00:01.35".into()),
                Value::Utf8("-10:00:01.2".into()),
                Value::Utf8("2024-03-01 00:00:00.15".into())
            ]]
        );
        let overflow = backend.execute("SELECT DATE_ADD('1995-01-05', INTERVAL 9223372036854775806 SECOND), DATE_ADD('1995-01-05', INTERVAL -9223372036854775806 SECOND)").await.unwrap();
        assert_eq!(
            overflow.rows.into_values(),
            vec![vec![Value::Null, Value::Null]]
        );
    }

    #[tokio::test]
    async fn time_extrema_use_the_greatest_fractional_precision() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT LEAST(TIME'00:00:00.1', TIME'00:00:00.12'), GREATEST(TIME'00:00:00.1', TIME'00:00:00.12'), LEAST(TIME'-24:00:00.1', TIME'-240:00:00.12')").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("00:00:00.10".into()),
                Value::Utf8("00:00:00.12".into()),
                Value::Utf8("-240:00:00.12".into())
            ]]
        );
    }

    #[tokio::test]
    async fn time_function_results_keep_temporal_comparison_types() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT CAST('10:00:00' AS TIME(6)) = MAKETIME(10,0,0), CAST('10:00:00' AS TIME(6)) = SEC_TO_TIME(36000)").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![Value::Boolean(true), Value::Boolean(true)]]
        );
        for function in ["ADDTIME", "TIMEDIFF"] {
            let result = backend.execute(&format!("SELECT {function}(duration, '00:00:00') AS duration FROM (SELECT CAST('-24:00:00' AS TIME) AS duration UNION ALL SELECT CAST('-240:00:00' AS TIME)) AS durations ORDER BY duration")).await.unwrap();
            assert_eq!(
                result.rows.into_values(),
                vec![
                    vec![Value::Utf8("-240:00:00".into())],
                    vec![Value::Utf8("-24:00:00".into())]
                ]
            );
        }
    }

    #[tokio::test]
    async fn maketime_rounds_or_truncates_fractional_seconds() {
        use pintail_protocol::Handler;
        use pintail_types::Value;
        let (_directory, mut backend) = local_backend();
        let result = backend.execute("SELECT MAKETIME(12,15,59.9999999), MAKETIME(12,15,30.500), MAKETIME(838,59,59.0000005)").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("12:16:00.000000".into()),
                Value::Utf8("12:15:30.500".into()),
                Value::Utf8("838:59:59.000000".into())
            ]]
        );
        backend
            .query(b"SET sql_mode='TIME_TRUNCATE_FRACTIONAL'")
            .await;
        let result = backend
            .execute(
                "SELECT MAKETIME(12,15,59.9999999), MAKETIME(12,15,CAST(59.9999999 AS DOUBLE))",
            )
            .await
            .unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Utf8("12:15:59.999999".into()),
                Value::Utf8("12:15:59.999999".into())
            ]]
        );
    }

    #[tokio::test]
    async fn signed_time_order_uses_duration_value() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend.execute("SELECT duration FROM (SELECT CAST('-24:00:00.000001' AS TIME(6)) AS duration UNION ALL SELECT CAST('-240:00:00' AS TIME(6)) UNION ALL SELECT CAST('02:00:00' AS TIME(6)) UNION ALL SELECT CAST('100:00:00' AS TIME(6))) AS durations ORDER BY duration").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            [
                "-240:00:00.000000",
                "-24:00:00.000001",
                "02:00:00.000000",
                "100:00:00.000000"
            ]
            .into_iter()
            .map(|s| vec![Value::Utf8(s.to_owned())])
            .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn clock_parts_include_duration_days() {
        use pintail_types::Value;
        let (_directory, backend) = local_backend();
        let result = backend
            .execute("SELECT HOUR('1 00:00:00'), HOUR('-2 03:04:05.123456'), MINUTE('2 03:04:05'), SECOND('2 03:04:05')")
            .await
            .unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![vec![
                Value::Int64(24),
                Value::Int64(51),
                Value::Int64(4),
                Value::Int64(5)
            ]]
        );
    }

    #[tokio::test]
    async fn date_text_variables_keep_dynamic_fractional_metadata() {
        let (_directory, backend) = local_backend();
        backend
            .execute("SELECT @instant := FROM_UNIXTIME(1)")
            .await
            .unwrap();
        let result = backend
            .execute("SELECT UNIX_TIMESTAMP(@instant)")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0][0],
            pintail_types::Value::Utf8("1.000000".to_owned())
        );
    }

    #[tokio::test]
    async fn select_assignments_persist_and_read_previous_rows() {
        use pintail_protocol::Handler;
        use pintail_types::Value as SqlValue;
        let (_directory, mut backend) = local_backend();
        assert!(matches!(
            backend.query(b"SET @counter=0").await,
            pintail_protocol::Response::Ok(..)
        ));
        let result = backend.execute("SELECT @counter := @counter + 1 FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) AS numbers").await.unwrap();
        assert_eq!(
            result.rows.into_values(),
            vec![
                vec![SqlValue::Int64(1)],
                vec![SqlValue::Int64(2)],
                vec![SqlValue::Int64(3)]
            ]
        );
        let result = backend.execute("SELECT @counter").await.unwrap();
        assert_eq!(result.rows[0][0], SqlValue::Int64(3));
        backend
            .execute("SELECT CONCAT(@label := 'value', '!')")
            .await
            .unwrap();
        let result = backend.execute("SELECT @label").await.unwrap();
        assert_eq!(result.rows[0][0], SqlValue::Utf8("value".to_owned()));
        backend
            .execute("SELECT IF(FALSE, @counter := 99, 0)")
            .await
            .unwrap();
        backend
            .execute("SELECT @counter := 99 WHERE FALSE")
            .await
            .unwrap();
        backend
            .prepare(b"SELECT @counter := @counter + 1")
            .await
            .unwrap();
        let result = backend.execute("SELECT @counter").await.unwrap();
        assert_eq!(result.rows[0][0], SqlValue::Int64(3));
        let (_other_directory, other) = local_backend();
        let result = other.execute("SELECT @counter").await.unwrap();
        assert_eq!(result.rows[0][0], SqlValue::Null);
    }

    #[tokio::test]
    async fn ansi_mode_changes_real_casts_columns_and_concatenation() {
        use pintail_protocol::Handler;
        use pintail_types::Value as SqlValue;
        let (_directory, mut backend) = local_backend();
        let before = backend
            .execute("SELECT CAST(CAST(16777217 AS REAL) AS SIGNED)")
            .await
            .unwrap();
        assert_eq!(before.rows[0][0], SqlValue::Int64(16_777_217));
        assert!(matches!(
            backend.query(b"SET sql_mode='ANSI'").await,
            pintail_protocol::Response::Ok(..)
        ));
        let result = backend
            .execute("SELECT 'A'||'B', CAST(CAST(16777217 AS REAL) AS SIGNED)")
            .await
            .unwrap();
        assert_eq!(
            result.rows[0],
            vec![SqlValue::Utf8("AB".into()), SqlValue::Int64(16_777_216)]
        );
        backend
            .execute("CREATE TABLE real_values(value REAL)")
            .await
            .unwrap();
        backend
            .execute("INSERT INTO real_values VALUES(16777217)")
            .await
            .unwrap();
        let result = backend
            .execute("SELECT CAST(value AS SIGNED) FROM real_values")
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], SqlValue::Int64(16_777_216));
    }

    #[tokio::test]
    async fn sql_mode_assignments_evaluate_expressions_and_session_references() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        assert!(matches!(
            backend
                .query(b"SET sql_mode=CONCAT(@@session.sql_mode, ',NO_BACKSLASH_ESCAPES')")
                .await,
            pintail_protocol::Response::Ok(..)
        ));
        assert!(
            backend
                .session
                .lock()
                .unwrap()
                .sql_mode
                .contains("NO_BACKSLASH_ESCAPES")
        );
        let output = backend
            .execute(r"SELECT 'a\bc' LIKE 'a\%' AS matches")
            .await
            .unwrap();
        assert_eq!(output.rows[0][0], pintail_types::Value::Boolean(true));
        assert!(matches!(
            backend.query(b"SET sql_mode=@@global.sql_mode").await,
            pintail_protocol::Response::Ok(..)
        ));
        assert_eq!(
            backend.session.lock().unwrap().sql_mode,
            backend.default_sql_mode
        );
    }

    #[tokio::test]
    async fn short_identifiers_increment_across_statement_assignments() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
        for sql in [b"SET @a=UUID_SHORT()".as_slice(), b"SET @b=UUID_SHORT()"] {
            assert!(matches!(
                backend.query(sql).await,
                pintail_protocol::Response::Ok(..)
            ));
        }
        let output = backend.execute("SELECT @b-@a").await.unwrap();
        assert_eq!(output.rows[0][0], pintail_types::Value::Int64(1));
        let output = backend.execute("SELECT UUID_SHORT() FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) AS numbers").await.unwrap();
        let values: std::collections::BTreeSet<_> = output
            .rows
            .into_values()
            .into_iter()
            .map(|row| match row[0] {
                pintail_types::Value::UInt64(value) => value,
                ref other => panic!("expected an unsigned identifier, got {other:?}"),
            })
            .collect();
        assert_eq!(values.len(), 3);
    }

    #[tokio::test]
    async fn oversized_binary_casts_leave_a_packet_limit_warning() {
        let (_directory, backend) = local_backend();
        let output = backend
            .execute("SELECT CAST('a' AS BINARY(67108865)) IS NULL")
            .await
            .unwrap();
        assert_eq!(output.rows[0][0], pintail_types::Value::Boolean(true));
        let session = backend.session.lock().unwrap();
        assert_eq!(session.condition_count, 1);
        assert_eq!(session.conditions[0].code, 1301);
        assert_eq!(
            session.conditions[0].message,
            "Result of cast_as_binary() was larger than max_allowed_packet (67108864) - truncated"
        );
    }

    #[tokio::test]
    async fn row_counts_follow_the_real_write_and_query_path() {
        use pintail_protocol::Handler;
        let (_directory, mut backend) = local_backend();
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
        assert!(matches!(
            backend
                .query(
                    b"SELECT id, id+1 INTO @first, @second FROM values_table ORDER BY id LIMIT 1"
                )
                .await,
            pintail_protocol::Response::Ok(..)
        ));
        {
            let session = backend.session.lock().unwrap();
            assert_eq!(
                session.user_variables["first"],
                Value::Number("1".into(), false)
            );
            assert_eq!(
                session.user_variables["second"],
                Value::Number("2".into(), false)
            );
            assert_eq!(session.row_count, 1);
        }
        assert!(matches!(
            backend
                .query(b"SELECT id INTO @first FROM values_table WHERE FALSE")
                .await,
            pintail_protocol::Response::Ok(..)
        ));
        {
            let session = backend.session.lock().unwrap();
            assert_eq!(
                session.user_variables["first"],
                Value::Number("1".into(), false)
            );
            assert_eq!(session.row_count, 0);
            assert_eq!(session.conditions[0].code, 1329);
        }
        assert!(matches!(
            backend
                .query(b"SELECT id INTO @first FROM values_table")
                .await,
            pintail_protocol::Response::Error(..)
        ));
        assert_eq!(backend.session.lock().unwrap().conditions[0].code, 1172);
    }
}
