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

/// The table an `ALTER TABLE` names, read lexically.
///
/// The statements this is used for are precisely the ones the SQL parser
/// rejects, so it cannot help here. Handles the optional `IF EXISTS`, the
/// schema qualifier, and backtick quoting.
fn alter_table_target(statement: &str) -> Option<String> {
    let rest = statement
        .trim_start()
        .get("ALTER TABLE".len()..)?
        .trim_start();
    let rest = rest
        .strip_prefix("IF EXISTS")
        .or_else(|| rest.strip_prefix("if exists"))
        .map_or(rest, str::trim_start);
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
            '.' if !quoted => name.clear(),
            c if !quoted && (c.is_whitespace() || c == '(') => break,
            c => name.push(c),
        }
    }
    (!name.is_empty()).then_some(name)
}

#[allow(clippy::too_many_lines)] // linear DDL-statement classification table
pub(crate) fn parse_ddl(statement: &str) -> Result<Vec<DdlAction>, CdcError> {
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
        return Ok(Vec::new());
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
            .map(|table| {
                vec![DdlAction::Alter {
                    table,
                    kind: AlterKind::IndexOnly,
                }]
            })
            .unwrap_or_default());
    }
    let statements = Parser::parse_sql(&MySqlDialect {}, statement)
        .map_err(|error| CdcError::Ddl(format!("cannot parse DDL `{statement}`: {error}")))?;
    let mut actions = Vec::new();
    for statement in statements {
        match statement {
            Statement::AlterTable(alter) => {
                let table = table_name(&alter.name)?;
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
                actions.push(DdlAction::Alter { table, kind });
            }
            Statement::Truncate(truncate) => {
                for target in truncate.table_names {
                    actions.push(DdlAction::Truncate {
                        table: table_name(&target.name)?,
                    });
                }
            }
            Statement::Drop {
                object_type: ObjectType::Table,
                names,
                ..
            } => {
                for name in names {
                    actions.push(DdlAction::Drop {
                        table: table_name(&name)?,
                    });
                }
            }
            Statement::CreateTable(create) if !create.temporary => {
                actions.push(DdlAction::Create {
                    table: table_name(&create.name)?,
                });
            }
            Statement::RenameTable(renames) => {
                for rename in renames {
                    actions.push(DdlAction::Alter {
                        table: table_name(&rename.old_name)?,
                        kind: AlterKind::RequiresResnapshot,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(actions)
}

fn table_name(name: &ObjectName) -> Result<String, CdcError> {
    name.0
        .last()
        .and_then(|part| part.as_ident())
        .map(|identifier| identifier.value.clone())
        .ok_or_else(|| {
            CdcError::Ddl(format!(
                "DDL object name `{name}` is not a table identifier"
            ))
        })
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
        )
        .expect("a statement MySQL accepts must not fail schema tracking");
        assert_eq!(
            actions,
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
            let actions = parse_ddl(statement).expect(statement);
            assert_eq!(
                actions,
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
        let actions = parse_ddl("ALTER TABLE orders ADD COLUMN coupon VARCHAR(24) NULL")
            .expect("ordinary DDL still parses");
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
            parse_ddl("ALTER TABLE `app`.`events` ADD COLUMN note TEXT NULL").unwrap(),
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::AddOrDropColumns,
            }]
        );
        assert_eq!(
            parse_ddl("ALTER TABLE events DROP COLUMN note").unwrap(),
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::AddOrDropColumns,
            }]
        );
        assert_eq!(
            parse_ddl("ALTER TABLE events RENAME COLUMN note TO memo").unwrap(),
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::RenameColumns(vec![("note".to_owned(), "memo".to_owned())]),
            }]
        );
        // MODIFY and same-name CHANGE classify as in-place candidates; the
        // handler still quarantines unless the change is storage-compatible.
        assert_eq!(
            parse_ddl("ALTER TABLE events MODIFY COLUMN note BIGINT").unwrap(),
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::ModifyColumns(vec!["note".to_owned()]),
            }]
        );
        assert_eq!(
            parse_ddl("ALTER TABLE events CHANGE COLUMN note note VARCHAR(200)").unwrap(),
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
                parse_ddl(ddl).unwrap(),
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
                parse_ddl(ddl).unwrap(),
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
            parse_ddl("TRUNCATE TABLE app.events").unwrap(),
            vec![DdlAction::Truncate {
                table: "events".to_owned(),
            }]
        );
        assert_eq!(
            parse_ddl("DROP TABLE app.events, app.audit").unwrap(),
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
            parse_ddl("CREATE TABLE app.created (id BIGINT PRIMARY KEY)").unwrap(),
            vec![DdlAction::Create {
                table: "created".to_owned(),
            }]
        );
        assert!(
            parse_ddl("CREATE TEMPORARY TABLE scratch (id INT)")
                .unwrap()
                .is_empty()
        );
        assert!(parse_ddl("COMMIT").unwrap().is_empty());
        assert!(
            parse_ddl("CREATE USER example IDENTIFIED BY 'secret'")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_ddl("RENAME TABLE events TO archived_events").unwrap(),
            vec![DdlAction::Alter {
                table: "events".to_owned(),
                kind: AlterKind::RequiresResnapshot,
            }]
        );
    }
}
