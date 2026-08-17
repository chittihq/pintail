use sqlparser::{
    ast::{AlterTableOperation, ObjectName, ObjectType, Statement, TableConstraint},
    dialect::MySqlDialect,
    parser::Parser,
};

use crate::CdcError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AlterKind {
    AddOrDropColumns,
    /// Pure column renames as `(old name, new name)` pairs: the stable
    /// column IDs carry across, so no resnapshot is needed.
    RenameColumns(Vec<(String, String)>),
    /// Pure `MODIFY COLUMN` (or same-name `CHANGE COLUMN`) type changes.
    /// The handler evolves in place only when every change is
    /// storage-compatible; anything else quarantines for resync.
    ModifyColumns(Vec<String>),
    /// Index/constraint-only changes with no storage impact. The handler
    /// adopts the refreshed key metadata; a changed key strategy still
    /// quarantines.
    IndexOnly,
    RequiresResnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DdlAction {
    Alter { table: String, kind: AlterKind },
    Truncate { table: String },
    Drop { table: String },
    Create { table: String },
}

/// What one DDL statement means for the tracked schema.
///
/// Object names qualified to a DIFFERENT schema are dropped during
/// classification rather than filtered by the caller, because the two
/// mistakes they invite are severe and opposite: `DROP TABLE other_db.t`
/// routed by its bare table name orphans the tracked `t`, and
/// `CREATE TABLE other_db.t` reaches the auto-include path, is absent from
/// the tracked schema's probe, and errors the stream without advancing the
/// checkpoint - which retries the same statement forever.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedDdl {
    pub(crate) actions: Vec<DdlAction>,
    /// Whether any surviving action named the tracked schema EXPLICITLY.
    /// The caller's session-schema gate cannot see this on its own: a
    /// statement like `DROP TABLE app.t` issued from a session sitting in
    /// another schema is still the tracked schema's DDL.
    pub(crate) names_tracked_schema: bool,
}

/// The `(schema, table)` an `ALTER TABLE` names, read lexically.
///
/// The statements this is used for are precisely the ones the SQL parser
/// rejects, so it cannot help here. Handles the optional `IF EXISTS`, the
/// schema qualifier, and backtick quoting.
fn alter_table_target(statement: &str) -> Option<(Option<String>, String)> {
    let rest = statement
        .trim_start()
        .get("ALTER TABLE".len()..)?
        .trim_start();
    let rest = rest
        .strip_prefix("IF EXISTS")
        .or_else(|| rest.strip_prefix("if exists"))
        .map_or(rest, str::trim_start);
    let mut schema = None;
    let mut name = String::new();
    let mut chars = rest.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        match character {
            '`' => {
                quoted = !quoted;
                if !quoted && chars.peek() != Some(&'.') {
                    break;
                }
            }
            // A qualifier means what came before was the schema, not the table.
            '.' if !quoted => schema = Some(std::mem::take(&mut name)),
            c if !quoted && (c.is_whitespace() || c == '(') => break,
            c => name.push(c),
        }
    }
    (!name.is_empty()).then_some((schema, name))
}

#[allow(clippy::too_many_lines)] // linear DDL-statement classification table
pub(crate) fn parse_ddl(statement: &str, database: &str) -> Result<ParsedDdl, CdcError> {
    let normalized = statement.trim_start().to_ascii_uppercase();
    if ![
        "ALTER TABLE",
        "TRUNCATE ",
        "DROP TABLE",
        "CREATE TABLE",
        "CREATE TEMPORARY TABLE",
        "RENAME TABLE",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
    {
        return Ok(ParsedDdl::default());
    }
    // `ALTER TABLE ... CONVERT TO CHARACTER SET ...` is valid MySQL that
    // sqlparser 0.62 cannot parse, so it arrived here as a hard error and
    // stopped schema tracking outright - replication failing on a statement
    // the source accepted. It is also exactly what an operator runs to move a
    // table onto a collation this engine can compare, so the fix for one
    // collation problem was triggering another.
    //
    // Classified as metadata-only. Pintail stores decoded values rather than
    // source bytes, so re-encoding a column between character sets leaves the
    // logical value identical; what changes is the collation, and that is
    // metadata the re-probe adopts. A narrowing conversion that MySQL cannot
    // represent losslessly would change values, and needs an operator resync -
    // recorded in docs/limitations.md rather than guessed at here, because the
    // statement alone does not say which kind it is.
    if normalized.starts_with("ALTER TABLE")
        && (normalized.contains(" CONVERT TO CHARACTER SET")
            || normalized.contains(" CONVERT TO CHARSET"))
    {
        return Ok(alter_table_target(statement)
            .map(|(schema, table)| {
                let foreign = schema
                    .as_deref()
                    .is_some_and(|schema| !schema.eq_ignore_ascii_case(database));
                if foreign {
                    return ParsedDdl::default();
                }
                ParsedDdl {
                    actions: vec![DdlAction::Alter {
                        table,
                        kind: AlterKind::IndexOnly,
                    }],
                    names_tracked_schema: schema.is_some(),
                }
            })
            .unwrap_or_default());
    }
    let statements = Parser::parse_sql(&MySqlDialect {}, statement)
        .map_err(|error| CdcError::Ddl(format!("cannot parse DDL `{statement}`: {error}")))?;
    let mut parsed = ParsedDdl::default();
    for statement in statements {
        match statement {
            Statement::AlterTable(alter) => {
                let Some(table) = table_in_schema(&alter.name, database, &mut parsed)? else {
                    continue;
                };
                let kind =
                    if alter.operations.iter().all(|operation| {
                        matches!(
                            operation,
                            AlterTableOperation::AddColumn { .. }
                                | AlterTableOperation::DropColumn { .. }
                        )
                    }) {
                        AlterKind::AddOrDropColumns
                    } else if alter.operations.iter().all(|operation| {
                        matches!(operation, AlterTableOperation::RenameColumn { .. })
                    }) {
                        AlterKind::RenameColumns(
                            alter
                                .operations
                                .iter()
                                .map(|operation| {
                                    let AlterTableOperation::RenameColumn {
                                        old_column_name,
                                        new_column_name,
                                    } = operation
                                    else {
                                        unreachable!("all operations matched RenameColumn");
                                    };
                                    (old_column_name.value.clone(), new_column_name.value.clone())
                                })
                                .collect(),
                        )
                    } else if alter.operations.iter().all(|operation| {
                        matches!(operation, AlterTableOperation::ModifyColumn { .. })
                            || matches!(
                                operation,
                                AlterTableOperation::ChangeColumn {
                                    old_name,
                                    new_name,
                                    ..
                                } if old_name.value.eq_ignore_ascii_case(&new_name.value)
                            )
                    }) {
                        AlterKind::ModifyColumns(
                            alter
                                .operations
                                .iter()
                                .map(|operation| match operation {
                                    AlterTableOperation::ModifyColumn { col_name, .. } => {
                                        col_name.value.clone()
                                    }
                                    AlterTableOperation::ChangeColumn { old_name, .. } => {
                                        old_name.value.clone()
                                    }
                                    _ => unreachable!("all operations matched modify/change"),
                                })
                                .collect(),
                        )
                    } else if alter.operations.iter().all(|operation| {
                        matches!(
                            operation,
                            AlterTableOperation::AddConstraint {
                                constraint: TableConstraint::Unique(_)
                                    | TableConstraint::ForeignKey(_)
                                    | TableConstraint::Check(_)
                                    | TableConstraint::Index(_)
                                    | TableConstraint::FulltextOrSpatial(_),
                                ..
                            } | AlterTableOperation::DropIndex { .. }
                                | AlterTableOperation::DropConstraint { .. }
                                | AlterTableOperation::DropForeignKey { .. }
                        )
                    }) {
                        AlterKind::IndexOnly
                    } else {
                        AlterKind::RequiresResnapshot
                    };
                parsed.actions.push(DdlAction::Alter { table, kind });
            }
            Statement::Truncate(truncate) => {
                for target in truncate.table_names {
                    let Some(table) = table_in_schema(&target.name, database, &mut parsed)? else {
                        continue;
                    };
                    parsed.actions.push(DdlAction::Truncate { table });
                }
            }
            Statement::Drop {
                object_type: ObjectType::Table,
                names,
                ..
            } => {
                for name in names {
                    let Some(table) = table_in_schema(&name, database, &mut parsed)? else {
                        continue;
                    };
                    parsed.actions.push(DdlAction::Drop { table });
                }
            }
            Statement::CreateTable(create) if !create.temporary => {
                let Some(table) = table_in_schema(&create.name, database, &mut parsed)? else {
                    continue;
                };
                parsed.actions.push(DdlAction::Create { table });
            }
            Statement::RenameTable(renames) => {
                for rename in renames {
                    let Some(table) = table_in_schema(&rename.old_name, database, &mut parsed)?
                    else {
                        continue;
                    };
                    parsed.actions.push(DdlAction::Alter {
                        table,
                        kind: AlterKind::RequiresResnapshot,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// Resolves a DDL object name against the tracked schema.
///
/// `None` means the name is qualified to a different schema and the action
/// must not exist: routing it by bare table name would apply another
/// schema's DDL to a tracked table of the same name.
fn table_in_schema(
    name: &ObjectName,
    database: &str,
    parsed: &mut ParsedDdl,
) -> Result<Option<String>, CdcError> {
    let table = name
        .0
        .last()
        .and_then(|part| part.as_ident())
        .map(|identifier| identifier.value.clone())
        .ok_or_else(|| {
            CdcError::Ddl(format!(
                "DDL object name `{name}` is not a table identifier"
            ))
        })?;
    if name.0.len() > 1 {
        let schema = name.0[..name.0.len() - 1]
            .last()
            .and_then(|part| part.as_ident())
            .ok_or_else(|| {
                CdcError::Ddl(format!(
                    "DDL object name `{name}` has a schema qualifier that is not an identifier"
                ))
            })?;
        if !schema.value.eq_ignore_ascii_case(database) {
            return Ok(None);
        }
        parsed.names_tracked_schema = true;
    }
    Ok(Some(table))
}

#[cfg(test)]
mod convert_charset_tests {
    use super::{AlterKind, DdlAction, parse_ddl};

    /// The statement that stopped schema tracking on a live deployment.
    #[test]
    fn whole_table_conversion_is_metadata_only() {
        let actions = parse_ddl(
            "ALTER TABLE `chitti_lms`.`AIGeneratedFeed` \
             CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci",
            "chitti_lms",
        )
        .expect("a statement MySQL accepts must not fail schema tracking");
        assert_eq!(
            actions.actions,
            vec![DdlAction::Alter {
                table: "AIGeneratedFeed".to_owned(),
                kind: AlterKind::IndexOnly,
            }],
        );
    }

    #[test]
    fn the_table_is_read_through_its_quoting_and_qualifier() {
        for statement in [
            "ALTER TABLE orders CONVERT TO CHARACTER SET utf8mb4",
            "ALTER TABLE `orders` CONVERT TO CHARACTER SET utf8mb4",
            "ALTER TABLE shop.orders CONVERT TO CHARACTER SET utf8mb4",
            "ALTER TABLE `shop`.`orders` CONVERT TO CHARSET utf8mb4",
            "alter table orders convert to character set utf8mb4",
        ] {
            let actions = parse_ddl(statement, "shop").expect(statement);
            assert_eq!(
                actions.actions,
                vec![DdlAction::Alter {
                    table: "orders".to_owned(),
                    kind: AlterKind::IndexOnly,
                }],
                "{statement}",
            );
        }
    }

    /// The prefix match must not swallow statements that merely mention the
    /// words, or a real column change would be classified as metadata.
    #[test]
    fn an_ordinary_alter_still_takes_the_parser() {
        let actions = parse_ddl(
            "ALTER TABLE orders ADD COLUMN coupon VARCHAR(24) NULL",
            "app",
        )
        .expect("ordinary DDL still parses");
        let actions = actions.actions;
        assert!(
            !actions.is_empty()
                && actions
                    != vec![DdlAction::Alter {
                        table: "orders".to_owned(),
                        kind: AlterKind::IndexOnly,
                    }],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{AlterKind, DdlAction, parse_ddl};

    #[test]
    fn classifies_supported_mysql_schema_changes() {
        assert_eq!(
            parse_ddl(
                "ALTER TABLE `app`.`events` ADD COLUMN note TEXT NULL",
                "app"
            )
            .unwrap()
            .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::AddOrDropColumns,
            }]
        );
        assert_eq!(
            parse_ddl("ALTER TABLE events DROP COLUMN note", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::AddOrDropColumns,
            }]
        );
        assert_eq!(
            parse_ddl("ALTER TABLE events RENAME COLUMN note TO memo", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::RenameColumns(vec![("note".to_owned(), "memo".to_owned())]),
            }]
        );
        // MODIFY and same-name CHANGE classify as in-place candidates; the
        // handler still quarantines unless the change is storage-compatible.
        assert_eq!(
            parse_ddl("ALTER TABLE events MODIFY COLUMN note BIGINT", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::ModifyColumns(vec!["note".to_owned()]),
            }]
        );
        assert_eq!(
            parse_ddl(
                "ALTER TABLE events CHANGE COLUMN note note VARCHAR(200)",
                "app"
            )
            .unwrap()
            .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::ModifyColumns(vec!["note".to_owned()]),
            }]
        );
        for ddl in [
            "ALTER TABLE events ADD INDEX note_index(note)",
            "ALTER TABLE events DROP INDEX note_index",
            "ALTER TABLE events ADD UNIQUE KEY unique_note (note)",
        ] {
            assert_eq!(
                parse_ddl(ddl, "app").unwrap().actions,
                vec![DdlAction::Alter {
                    table: "events".to_owned(),
                    kind: AlterKind::IndexOnly,
                }],
                "{ddl}"
            );
        }
        for ddl in [
            "ALTER TABLE events CHANGE COLUMN note memo TEXT",
            "ALTER TABLE events ADD PRIMARY KEY (id)",
            "ALTER TABLE events MODIFY COLUMN note BIGINT, ADD COLUMN extra INT",
        ] {
            assert_eq!(
                parse_ddl(ddl, "app").unwrap().actions,
                vec![DdlAction::Alter {
                    table: "events".to_owned(),
                    kind: AlterKind::RequiresResnapshot,
                }]
            );
        }
    }

    #[test]
    fn extracts_create_drop_and_truncate_tables() {
        assert_eq!(
            parse_ddl("TRUNCATE TABLE app.events", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Truncate {
                table: "events".to_owned(),
            }]
        );
        assert_eq!(
            parse_ddl("DROP TABLE app.events, app.audit", "app")
                .unwrap()
                .actions,
            vec![
                DdlAction::Drop {
                    table: "events".to_owned(),
                },
                DdlAction::Drop {
                    table: "audit".to_owned(),
                },
            ]
        );
        assert_eq!(
            parse_ddl("CREATE TABLE app.created (id BIGINT PRIMARY KEY)", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Create {
                table: "created".to_owned(),
            }]
        );
        assert!(
            parse_ddl("CREATE TEMPORARY TABLE scratch (id INT)", "app")
                .unwrap()
                .actions
                .is_empty()
        );
        assert!(parse_ddl("COMMIT", "app").unwrap().actions.is_empty());
        assert!(
            parse_ddl("CREATE USER example IDENTIFIED BY 'secret'", "app")
                .unwrap()
                .actions
                .is_empty()
        );
        assert_eq!(
            parse_ddl("RENAME TABLE events TO archived_events", "app")
                .unwrap()
                .actions,
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::RequiresResnapshot,
            }]
        );
    }

    #[test]
    fn foreign_schema_ddl_produces_no_actions() {
        // Both directions of the mistake this exists to prevent: a DROP that
        // would orphan the tracked table of the same name, and a CREATE that
        // would reach auto-include, miss the tracked probe, and wedge the
        // stream on a permanent error.
        for ddl in [
            "DROP TABLE other_db.events",
            "CREATE TABLE other_db.events (id INT PRIMARY KEY)",
            "TRUNCATE TABLE other_db.events",
            "ALTER TABLE other_db.events ADD COLUMN note TEXT NULL",
            "ALTER TABLE `other_db`.`events` CONVERT TO CHARACTER SET utf8mb4",
        ] {
            let parsed = parse_ddl(ddl, "app").expect(ddl);
            assert!(parsed.actions.is_empty(), "{ddl}");
            assert!(!parsed.names_tracked_schema, "{ddl}");
        }
        // A mixed DROP keeps only the tracked half.
        let parsed = parse_ddl("DROP TABLE app.events, other_db.events", "app").unwrap();
        assert_eq!(
            parsed.actions,
            vec![DdlAction::Drop {
                table: "events".to_owned(),
            }]
        );
        assert!(parsed.names_tracked_schema);
    }

    #[test]
    fn an_explicit_tracked_qualifier_is_reported() {
        // The caller's session-schema gate needs this to accept
        // `DROP TABLE app.t` issued from a session sitting elsewhere.
        assert!(
            parse_ddl("DROP TABLE app.events", "app")
                .unwrap()
                .names_tracked_schema
        );
        assert!(
            !parse_ddl("DROP TABLE events", "app")
                .unwrap()
                .names_tracked_schema
        );
    }
}
