use sqlparser::{
    ast::{AlterTableOperation, ObjectName, ObjectType, Statement},
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
    RequiresResnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DdlAction {
    Alter { table: String, kind: AlterKind },
    Truncate { table: String },
    Drop { table: String },
    Create { table: String },
}

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
        for ddl in [
            "ALTER TABLE events MODIFY COLUMN note BIGINT",
            "ALTER TABLE events CHANGE COLUMN note memo TEXT",
            "ALTER TABLE events ADD INDEX note_index(note)",
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
