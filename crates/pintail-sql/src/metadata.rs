use std::{cmp::Ordering, fmt};

use pintail_catalog::{CatalogSnapshot, DatabaseEntry, TableEntry};
use pintail_types::{DataType, KeyMode, Value};
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, LimitClause, ObjectName, OrderByKind, Query, Select,
    SelectFlavor, SelectItem, SetExpr, ShowCreateObject, ShowStatementFilter,
    ShowStatementFilterPosition, ShowStatementOptions, Statement, TableFactor, Value as SqlValue,
    WildcardAdditionalOptions,
};

/// One metadata result-column description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataField {
    /// MySQL-compatible field name.
    pub name: String,
    /// Result scalar type.
    pub data_type: DataType,
    /// Whether rows can contain `NULL`.
    pub nullable: bool,
}

/// Fully materialized deterministic metadata response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataResult {
    /// Ordered result fields.
    pub fields: Vec<MetadataField>,
    /// Ordered result rows.
    pub rows: Vec<Vec<Value>>,
}

/// Source-derived facts the catalog schema does not carry. Callers
/// without probe data pass [`SourceFacts::default`].
#[derive(Clone, Debug, Default)]
pub struct SourceFacts {
    /// Per-column facts.
    pub columns: Vec<ColumnFacts>,
    /// Per-index facts (primary and unique constraints).
    pub indexes: Vec<IndexFacts>,
    /// Per-foreign-key facts.
    pub foreign_keys: Vec<ForeignKeyFacts>,
}

/// One foreign-key constraint on a source table.
#[derive(Clone, Debug, Default)]
pub struct ForeignKeyFacts {
    /// Source database name.
    pub database: String,
    /// Constrained (child) table name.
    pub table: String,
    /// Constraint name.
    pub name: String,
    /// Constrained column names in constraint order.
    pub columns: Vec<String>,
    /// Referenced (parent) table name.
    pub referenced_table: String,
    /// Referenced column names, parallel to `columns`.
    pub referenced_columns: Vec<String>,
    /// Referenced constraint name (usually `PRIMARY`), when known.
    pub unique_constraint_name: Option<String>,
    /// `ON UPDATE` rule text.
    pub update_rule: String,
    /// `ON DELETE` rule text.
    pub delete_rule: String,
}

/// One primary or unique constraint on a source table.
#[derive(Clone, Debug, Default)]
pub struct IndexFacts {
    /// Source database name.
    pub database: String,
    /// Source table name.
    pub table: String,
    /// Index name; `PRIMARY` for the primary key. Names of unique
    /// constraints beyond the chosen key are synthesized, since the probe
    /// does not retain them.
    pub index_name: String,
    /// Whether the index enforces uniqueness (always true today).
    pub unique: bool,
    /// Column names in index order.
    pub columns: Vec<String>,
}

/// Source-derived column facts the catalog schema does not carry:
/// defaults, auto-increment/generated markers, and single-column UNIQUE
/// membership.
#[derive(Clone, Debug, Default)]
pub struct ColumnFacts {
    /// Source database name.
    pub database: String,
    /// Source table name.
    pub table: String,
    /// Source column name.
    pub column: String,
    /// Raw `COLUMN_DEFAULT`, absent when the column has no default.
    pub default_value: Option<String>,
    /// Whether the source declares `AUTO_INCREMENT`.
    pub auto_increment: bool,
    /// Whether the column is a stored generated column.
    pub generated_stored: bool,
    /// Whether a single-column non-null UNIQUE constraint covers it.
    pub unique_single: bool,
}

/// Executes one supported `MySQL` metadata statement against an immutable catalog.
///
/// # Errors
///
/// Returns an explicit unsupported-shape or unknown-object error.
pub fn execute_metadata(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    current_database: Option<&str>,
    facts: &SourceFacts,
) -> Result<MetadataResult, MetadataError> {
    match statement {
        Statement::ShowDatabases {
            terse: false,
            history: false,
            show_options,
        } if database_options(show_options) => apply_show_filter(
            single_string_result("Database", catalog.databases().map(DatabaseEntry::name)),
            show_options,
        ),
        Statement::ShowTables {
            terse: false,
            history: false,
            extended: false,
            full,
            external: false,
            show_options,
        } if filterable_options(show_options) => {
            let database = resolve_show_database(show_options, catalog, current_database)?;
            apply_show_filter(show_tables(database, *full), show_options)
        }
        Statement::ShowColumns {
            extended: false,
            full: false,
            show_options,
        } if filterable_options(show_options) => {
            let name = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .ok_or_else(|| MetadataError::Unsupported(statement.to_string()))?;
            let (database, table) = resolve_table(name, catalog, current_database)?;
            apply_show_filter(describe_table(database, table, facts), show_options)
        }
        Statement::ExplainTable {
            hive_format: None,
            table_name,
            ..
        } => {
            let (database, table) = resolve_table(table_name, catalog, current_database)?;
            Ok(describe_table(database, table, facts))
        }
        Statement::ShowCreate {
            obj_type: ShowCreateObject::Table,
            obj_name,
        } => {
            let (database, table) = resolve_table(obj_name, catalog, current_database)?;
            Ok(show_create_table(database, table, facts))
        }
        Statement::ShowVariable { variable } => {
            let (database, table) = resolve_show_index(variable, catalog, current_database)?;
            Ok(show_indexes(database, table, facts))
        }
        Statement::Query(query) => execute_information_schema(query, catalog, facts),
        _ => Err(MetadataError::Unsupported(statement.to_string())),
    }
}

fn resolve_show_index<'a>(
    words: &[sqlparser::ast::Ident],
    catalog: &'a CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<(&'a DatabaseEntry, &'a TableEntry), MetadataError> {
    let values = words
        .iter()
        .map(|word| word.value.as_str())
        .collect::<Vec<_>>();
    let show_index = values.first().is_some_and(|word| {
        word.eq_ignore_ascii_case("INDEX")
            || word.eq_ignore_ascii_case("INDEXES")
            || word.eq_ignore_ascii_case("KEYS")
    });
    let separator =
        |word: &&str| word.eq_ignore_ascii_case("FROM") || word.eq_ignore_ascii_case("IN");
    if !show_index || values.get(1).is_none_or(|word| !separator(word)) {
        return Err(MetadataError::Unsupported(format!(
            "SHOW {}",
            values.join(" ")
        )));
    }
    let name = match values.as_slice() {
        [_, _, table] => ObjectName::from(vec![sqlparser::ast::Ident::new(*table)]),
        [_, _, table, second_separator, database] if separator(second_separator) => {
            ObjectName::from(vec![
                sqlparser::ast::Ident::new(*database),
                sqlparser::ast::Ident::new(*table),
            ])
        }
        _ => {
            return Err(MetadataError::Unsupported(format!(
                "SHOW {}",
                values.join(" ")
            )));
        }
    };
    resolve_table(&name, catalog, current_database)
}

fn execute_information_schema(
    query: &Query,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Result<MetadataResult, MetadataError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(MetadataError::Unsupported(query.to_string()));
    };
    if query_has_unsupported_clauses(query)
        || select_has_unsupported_clauses(select)
        || select.from.len() != 1
        || !select.from[0].joins.is_empty()
    {
        return Err(MetadataError::Unsupported(query.to_string()));
    }
    let TableFactor::Table {
        name,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
        ..
    } = &select.from[0].relation
    else {
        return Err(MetadataError::Unsupported(query.to_string()));
    };
    if !with_hints.is_empty() || !partitions.is_empty() || !index_hints.is_empty() {
        return Err(MetadataError::Unsupported(query.to_string()));
    }

    let mut result = information_schema_table(name, catalog, facts)?;
    if let Some(predicate) = &select.selection {
        let mut filtered = Vec::with_capacity(result.rows.len());
        for row in result.rows {
            if evaluate_metadata_predicate(predicate, &result.fields, &row)? {
                filtered.push(row);
            }
        }
        result.rows = filtered;
    }
    result = project_metadata_result(result, &select.projection)?;
    order_metadata_result(&mut result, query)?;
    limit_metadata_result(&mut result, query)?;
    Ok(result)
}

fn query_has_unsupported_clauses(query: &Query) -> bool {
    query.with.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
}

fn select_has_unsupported_clauses(select: &Select) -> bool {
    let empty_grouping =
        matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty());
    !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select
            .select_modifiers
            .as_ref()
            .is_some_and(sqlparser::ast::SelectModifiers::is_any_set)
        || select.top.is_some()
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !empty_grouping
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.flavor != SelectFlavor::Standard
}

fn information_schema_table(
    name: &ObjectName,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Result<MetadataResult, MetadataError> {
    let parts = object_name_parts(name)?;
    let [database, table] = parts.as_slice() else {
        return Err(MetadataError::Unsupported(name.to_string()));
    };
    if !database.eq_ignore_ascii_case("information_schema") {
        return Err(MetadataError::Unsupported(name.to_string()));
    }
    if table.eq_ignore_ascii_case("schemata") {
        Ok(information_schemata(catalog))
    } else if table.eq_ignore_ascii_case("tables") {
        Ok(information_tables(catalog))
    } else if table.eq_ignore_ascii_case("columns") {
        Ok(information_columns(catalog, facts))
    } else if table.eq_ignore_ascii_case("statistics") {
        Ok(information_statistics(catalog, facts))
    } else if table.eq_ignore_ascii_case("key_column_usage") {
        Ok(information_key_column_usage(catalog, facts))
    } else if table.eq_ignore_ascii_case("table_constraints") {
        Ok(information_table_constraints(catalog, facts))
    } else if table.eq_ignore_ascii_case("referential_constraints") {
        Ok(information_referential_constraints(catalog, facts))
    } else if table.eq_ignore_ascii_case("views") {
        Ok(information_views())
    } else {
        Err(MetadataError::UnknownTable((*table).to_owned()))
    }
}

/// Pintail replicates base tables only. Exposing the standard `VIEWS` shape
/// with no rows lets client inspectors distinguish "no replicated views" from
/// a missing metadata table without fabricating definitions.
fn information_views() -> MetadataResult {
    MetadataResult {
        fields: metadata_fields(&[
            ("TABLE_CATALOG", DataType::Utf8, false),
            ("TABLE_SCHEMA", DataType::Utf8, false),
            ("TABLE_NAME", DataType::Utf8, false),
            ("VIEW_DEFINITION", DataType::Utf8, false),
            ("CHECK_OPTION", DataType::Utf8, false),
            ("IS_UPDATABLE", DataType::Utf8, false),
            ("DEFINER", DataType::Utf8, false),
            ("SECURITY_TYPE", DataType::Utf8, false),
            ("CHARACTER_SET_CLIENT", DataType::Utf8, false),
            ("COLLATION_CONNECTION", DataType::Utf8, false),
        ]),
        rows: Vec::new(),
    }
}

fn information_schemata(catalog: &CatalogSnapshot) -> MetadataResult {
    let fields = metadata_fields(&[
        ("CATALOG_NAME", DataType::Utf8, false),
        ("SCHEMA_NAME", DataType::Utf8, false),
        ("DEFAULT_CHARACTER_SET_NAME", DataType::Utf8, false),
        ("DEFAULT_COLLATION_NAME", DataType::Utf8, false),
        ("SQL_PATH", DataType::Utf8, true),
    ]);
    let rows = catalog
        .databases()
        .map(|database| {
            vec![
                utf8("def"),
                utf8(database.name()),
                utf8("utf8mb4"),
                utf8("utf8mb4_general_ci"),
                Value::Null,
            ]
        })
        .collect();
    MetadataResult { fields, rows }
}

fn information_tables(catalog: &CatalogSnapshot) -> MetadataResult {
    let fields = metadata_fields(&[
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("TABLE_TYPE", DataType::Utf8, false),
        ("ENGINE", DataType::Utf8, false),
        ("TABLE_ROWS", DataType::UInt64, true),
    ]);
    let rows = catalog
        .databases()
        .flat_map(|database| {
            database.tables().map(move |table| {
                vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8("BASE TABLE"),
                    utf8("PINTAIL"),
                    table
                        .statistics()
                        .row_count()
                        .map_or(Value::Null, Value::UInt64),
                ]
            })
        })
        .collect();
    MetadataResult { fields, rows }
}

fn information_columns(catalog: &CatalogSnapshot, facts: &SourceFacts) -> MetadataResult {
    let fields = metadata_fields(&[
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("COLUMN_NAME", DataType::Utf8, false),
        ("ORDINAL_POSITION", DataType::UInt64, false),
        ("COLUMN_DEFAULT", DataType::Utf8, true),
        ("IS_NULLABLE", DataType::Utf8, false),
        ("DATA_TYPE", DataType::Utf8, false),
        ("COLUMN_TYPE", DataType::Utf8, false),
        ("COLUMN_KEY", DataType::Utf8, false),
        ("EXTRA", DataType::Utf8, false),
    ]);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for table in database.tables() {
            for (index, column) in table.schema().columns().iter().enumerate() {
                let column_type = mysql_type(column.data_type());
                let fact = column_fact(database.name(), table.name(), column.name(), facts);
                let key = column_key(table, column.id(), fact);
                let extra = fact.map_or("", |fact| {
                    if fact.auto_increment {
                        "auto_increment"
                    } else if fact.generated_stored {
                        "STORED GENERATED"
                    } else {
                        ""
                    }
                });
                rows.push(vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8(column.name()),
                    Value::UInt64(u64::try_from(index + 1).expect("column ordinal fits u64")),
                    fact.and_then(|fact| fact.default_value.as_deref())
                        .map_or(Value::Null, utf8),
                    utf8(if column.is_nullable() { "YES" } else { "NO" }),
                    utf8(mysql_data_type(column.data_type())),
                    utf8(&column_type),
                    utf8(key),
                    utf8(extra),
                ]);
            }
        }
    }
    MetadataResult { fields, rows }
}

/// Primary and unique constraints per table: the catalog primary key plus
/// probe-derived unique constraints, in `(index, seq)` order.
fn table_indexes(
    database: &str,
    table_name: &str,
    key_columns: &[String],
    key_mode: KeyMode,
    facts: &SourceFacts,
) -> Vec<(String, Vec<String>)> {
    let mut indexes = Vec::new();
    if !key_columns.is_empty() {
        let index_name = if key_mode == KeyMode::Primary {
            "PRIMARY".to_owned()
        } else {
            facts
                .indexes
                .iter()
                .find(|index| {
                    index.database.eq_ignore_ascii_case(database)
                        && index.table.eq_ignore_ascii_case(table_name)
                        && index.unique
                        && same_columns(&index.columns, key_columns)
                })
                .map_or_else(
                    || "pintail_unique_key".to_owned(),
                    |index| index.index_name.clone(),
                )
        };
        indexes.push((index_name, key_columns.to_vec()));
    }
    for index in &facts.indexes {
        if !index.database.eq_ignore_ascii_case(database)
            || !index.table.eq_ignore_ascii_case(table_name)
            || !index.unique
        {
            continue;
        }
        if !same_columns(&index.columns, key_columns) {
            indexes.push((index.index_name.clone(), index.columns.clone()));
        }
    }
    indexes
}

fn same_columns(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn column_key(
    table: &pintail_catalog::TableEntry,
    column_id: u32,
    fact: Option<&ColumnFacts>,
) -> &'static str {
    if table.key_column_ids().contains(&column_id) {
        return match table.schema().key_mode() {
            KeyMode::Primary => "PRI",
            KeyMode::Unique if table.key_column_ids().len() == 1 => "UNI",
            KeyMode::Unique => "MUL",
            KeyMode::AppendRowId => "",
        };
    }
    if fact.is_some_and(|fact| fact.unique_single) {
        "UNI"
    } else {
        ""
    }
}

fn catalog_key_names(table: &pintail_catalog::TableEntry) -> Vec<String> {
    table
        .key_column_ids()
        .iter()
        .filter_map(|id| {
            table
                .schema()
                .columns()
                .iter()
                .find(|column| column.id() == *id)
                .map(|column| column.name().to_owned())
        })
        .collect()
}

fn information_statistics(catalog: &CatalogSnapshot, facts: &SourceFacts) -> MetadataResult {
    let fields = statistics_fields();
    let rows = catalog
        .databases()
        .flat_map(|database| {
            database
                .tables()
                .flat_map(move |table| statistics_rows_for_table(database, table, facts))
        })
        .collect();
    MetadataResult { fields, rows }
}

fn statistics_fields() -> Vec<MetadataField> {
    metadata_fields(&[
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("NON_UNIQUE", DataType::Int64, false),
        ("INDEX_SCHEMA", DataType::Utf8, false),
        ("INDEX_NAME", DataType::Utf8, false),
        ("SEQ_IN_INDEX", DataType::UInt64, false),
        ("COLUMN_NAME", DataType::Utf8, false),
        ("COLLATION", DataType::Utf8, true),
        ("CARDINALITY", DataType::Int64, true),
        ("SUB_PART", DataType::Int64, true),
        ("PACKED", DataType::Utf8, true),
        ("NULLABLE", DataType::Utf8, false),
        ("INDEX_TYPE", DataType::Utf8, false),
        ("COMMENT", DataType::Utf8, false),
        ("INDEX_COMMENT", DataType::Utf8, false),
        ("IS_VISIBLE", DataType::Utf8, false),
        ("EXPRESSION", DataType::Utf8, true),
    ])
}

fn statistics_rows_for_table(
    database: &DatabaseEntry,
    table: &TableEntry,
    facts: &SourceFacts,
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    let key_names = catalog_key_names(table);
    for (index_name, columns) in table_indexes(
        database.name(),
        table.name(),
        &key_names,
        table.schema().key_mode(),
        facts,
    ) {
        for (sequence, column_name) in columns.iter().enumerate() {
            let nullable = table
                .schema()
                .columns()
                .iter()
                .find(|column| column.name().eq_ignore_ascii_case(column_name))
                .is_some_and(pintail_types::Column::is_nullable);
            rows.push(vec![
                utf8("def"),
                utf8(database.name()),
                utf8(table.name()),
                Value::Int64(0),
                utf8(database.name()),
                utf8(&index_name),
                Value::UInt64(u64::try_from(sequence + 1).expect("sequence fits u64")),
                utf8(column_name),
                utf8("A"),
                Value::Null,
                Value::Null,
                Value::Null,
                utf8(if nullable { "YES" } else { "" }),
                utf8("BTREE"),
                utf8(""),
                utf8(""),
                utf8("YES"),
                Value::Null,
            ]);
        }
    }
    rows
}

/// Renders the `SHOW INDEX`/`SHOW KEYS` projection from the same rows served
/// through `information_schema.statistics`, so both metadata interfaces stay
/// consistent by construction.
fn show_indexes(
    database: &DatabaseEntry,
    table: &TableEntry,
    facts: &SourceFacts,
) -> MetadataResult {
    const COLUMNS: [(usize, &str); 15] = [
        (2, "Table"),
        (3, "Non_unique"),
        (5, "Key_name"),
        (6, "Seq_in_index"),
        (7, "Column_name"),
        (8, "Collation"),
        (9, "Cardinality"),
        (10, "Sub_part"),
        (11, "Packed"),
        (12, "Null"),
        (13, "Index_type"),
        (14, "Comment"),
        (15, "Index_comment"),
        (16, "Visible"),
        (17, "Expression"),
    ];
    let statistics = information_statistics_for_table(database, table, facts);
    let fields = COLUMNS
        .iter()
        .map(|(index, name)| {
            let mut field = statistics.fields[*index].clone();
            (*name).clone_into(&mut field.name);
            field
        })
        .collect();
    let rows = statistics
        .rows
        .into_iter()
        .map(|row| {
            COLUMNS
                .iter()
                .map(|(index, _)| row[*index].clone())
                .collect()
        })
        .collect();
    MetadataResult { fields, rows }
}

fn information_statistics_for_table(
    database: &DatabaseEntry,
    table: &TableEntry,
    facts: &SourceFacts,
) -> MetadataResult {
    MetadataResult {
        fields: statistics_fields(),
        rows: statistics_rows_for_table(database, table, facts),
    }
}

fn information_key_column_usage(catalog: &CatalogSnapshot, facts: &SourceFacts) -> MetadataResult {
    let fields = metadata_fields(&[
        ("CONSTRAINT_CATALOG", DataType::Utf8, false),
        ("CONSTRAINT_SCHEMA", DataType::Utf8, false),
        ("CONSTRAINT_NAME", DataType::Utf8, false),
        ("TABLE_CATALOG", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("COLUMN_NAME", DataType::Utf8, false),
        ("ORDINAL_POSITION", DataType::UInt64, false),
        ("POSITION_IN_UNIQUE_CONSTRAINT", DataType::Int64, true),
        ("REFERENCED_TABLE_SCHEMA", DataType::Utf8, true),
        ("REFERENCED_TABLE_NAME", DataType::Utf8, true),
        ("REFERENCED_COLUMN_NAME", DataType::Utf8, true),
    ]);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for table in database.tables() {
            let key_names = catalog_key_names(table);
            for (index_name, columns) in table_indexes(
                database.name(),
                table.name(),
                &key_names,
                table.schema().key_mode(),
                facts,
            ) {
                for (sequence, column_name) in columns.iter().enumerate() {
                    rows.push(vec![
                        utf8("def"),
                        utf8(database.name()),
                        utf8(&index_name),
                        utf8("def"),
                        utf8(database.name()),
                        utf8(table.name()),
                        utf8(column_name),
                        Value::UInt64(u64::try_from(sequence + 1).expect("sequence fits u64")),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                    ]);
                }
            }
            for key in table_foreign_keys(database.name(), table.name(), facts) {
                for (sequence, (column, referenced)) in
                    key.columns.iter().zip(&key.referenced_columns).enumerate()
                {
                    let position = i64::try_from(sequence + 1).expect("position fits i64");
                    rows.push(vec![
                        utf8("def"),
                        utf8(database.name()),
                        utf8(&key.name),
                        utf8("def"),
                        utf8(database.name()),
                        utf8(table.name()),
                        utf8(column),
                        Value::UInt64(u64::try_from(sequence + 1).expect("sequence fits u64")),
                        Value::Int64(position),
                        utf8(database.name()),
                        utf8(&key.referenced_table),
                        utf8(referenced),
                    ]);
                }
            }
        }
    }
    MetadataResult { fields, rows }
}

fn information_referential_constraints(
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> MetadataResult {
    let fields = metadata_fields(&[
        ("CONSTRAINT_CATALOG", DataType::Utf8, false),
        ("CONSTRAINT_SCHEMA", DataType::Utf8, false),
        ("CONSTRAINT_NAME", DataType::Utf8, false),
        ("UNIQUE_CONSTRAINT_CATALOG", DataType::Utf8, false),
        ("UNIQUE_CONSTRAINT_SCHEMA", DataType::Utf8, false),
        ("UNIQUE_CONSTRAINT_NAME", DataType::Utf8, true),
        ("MATCH_OPTION", DataType::Utf8, false),
        ("UPDATE_RULE", DataType::Utf8, false),
        ("DELETE_RULE", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("REFERENCED_TABLE_NAME", DataType::Utf8, false),
    ]);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for table in database.tables() {
            for key in table_foreign_keys(database.name(), table.name(), facts) {
                rows.push(vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(&key.name),
                    utf8("def"),
                    utf8(database.name()),
                    key.unique_constraint_name
                        .as_deref()
                        .map_or(Value::Null, utf8),
                    utf8("NONE"),
                    utf8(&key.update_rule),
                    utf8(&key.delete_rule),
                    utf8(table.name()),
                    utf8(&key.referenced_table),
                ]);
            }
        }
    }
    MetadataResult { fields, rows }
}

fn table_foreign_keys<'facts>(
    database: &str,
    table: &str,
    facts: &'facts SourceFacts,
) -> impl Iterator<Item = &'facts ForeignKeyFacts> {
    facts.foreign_keys.iter().filter(move |key| {
        key.database.eq_ignore_ascii_case(database) && key.table.eq_ignore_ascii_case(table)
    })
}

fn information_table_constraints(catalog: &CatalogSnapshot, facts: &SourceFacts) -> MetadataResult {
    let fields = metadata_fields(&[
        ("CONSTRAINT_CATALOG", DataType::Utf8, false),
        ("CONSTRAINT_SCHEMA", DataType::Utf8, false),
        ("CONSTRAINT_NAME", DataType::Utf8, false),
        ("TABLE_SCHEMA", DataType::Utf8, false),
        ("TABLE_NAME", DataType::Utf8, false),
        ("CONSTRAINT_TYPE", DataType::Utf8, false),
        ("ENFORCED", DataType::Utf8, false),
    ]);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for table in database.tables() {
            let key_names = catalog_key_names(table);
            for (index_name, _) in table_indexes(
                database.name(),
                table.name(),
                &key_names,
                table.schema().key_mode(),
                facts,
            ) {
                let constraint_type = if index_name == "PRIMARY" {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                rows.push(vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(&index_name),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8(constraint_type),
                    utf8("YES"),
                ]);
            }
            for key in table_foreign_keys(database.name(), table.name(), facts) {
                rows.push(vec![
                    utf8("def"),
                    utf8(database.name()),
                    utf8(&key.name),
                    utf8(database.name()),
                    utf8(table.name()),
                    utf8("FOREIGN KEY"),
                    utf8("YES"),
                ]);
            }
        }
    }
    MetadataResult { fields, rows }
}

/// Synthesized `SHOW CREATE TABLE` output: the replica's schema rendered
/// as `MySQL` DDL, with defaults/`auto_increment` from probe facts. Formatting
/// details the replica does not track (exact charset per column, index
/// options) are omitted rather than guessed.
fn show_create_table(
    database: &DatabaseEntry,
    table: &TableEntry,
    facts: &SourceFacts,
) -> MetadataResult {
    use std::fmt::Write as _;
    let fields = metadata_fields(&[
        ("Table", DataType::Utf8, false),
        ("Create Table", DataType::Utf8, false),
    ]);
    let mut ddl = format!("CREATE TABLE `{}` (", table.name());
    for (index, column) in table.schema().columns().iter().enumerate() {
        if index > 0 {
            ddl.push(',');
        }
        let _ = write!(
            ddl,
            "\n  `{}` {}",
            column.name(),
            mysql_type(column.data_type())
        );
        if !column.is_nullable() {
            ddl.push_str(" NOT NULL");
        }
        let fact = facts.columns.iter().find(|fact| {
            fact.database.eq_ignore_ascii_case(database.name())
                && fact.table.eq_ignore_ascii_case(table.name())
                && fact.column.eq_ignore_ascii_case(column.name())
        });
        if let Some(fact) = fact {
            if let Some(default) = &fact.default_value {
                let _ = write!(ddl, " DEFAULT '{}'", default.replace('\'', "''"));
            }
            if fact.auto_increment {
                ddl.push_str(" AUTO_INCREMENT");
            }
        }
    }
    let key_names = catalog_key_names(table);
    for (index_name, columns) in table_indexes(
        database.name(),
        table.name(),
        &key_names,
        table.schema().key_mode(),
        facts,
    ) {
        let column_list = columns
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if index_name == "PRIMARY" {
            let _ = write!(ddl, ",\n  PRIMARY KEY ({column_list})");
        } else {
            let _ = write!(ddl, ",\n  UNIQUE KEY `{index_name}` ({column_list})");
        }
    }
    ddl.push_str("\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4");
    MetadataResult {
        fields,
        rows: vec![vec![utf8(table.name()), utf8(&ddl)]],
    }
}

fn metadata_fields(definitions: &[(&str, DataType, bool)]) -> Vec<MetadataField> {
    definitions
        .iter()
        .map(|(name, data_type, nullable)| MetadataField {
            name: (*name).to_owned(),
            data_type: *data_type,
            nullable: *nullable,
        })
        .collect()
}

fn utf8(value: &str) -> Value {
    Value::Utf8(value.to_owned())
}

fn project_metadata_result(
    source: MetadataResult,
    projection: &[SelectItem],
) -> Result<MetadataResult, MetadataError> {
    if projection.len() == 1 && projection[0].to_string().eq_ignore_ascii_case("COUNT(*)") {
        return Ok(MetadataResult {
            fields: metadata_fields(&[("COUNT(*)", DataType::UInt64, false)]),
            rows: vec![vec![Value::UInt64(
                u64::try_from(source.rows.len()).expect("metadata row count fits u64"),
            )]],
        });
    }

    let mut columns = Vec::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(options) | SelectItem::QualifiedWildcard(_, options)
                if wildcard_is_plain(options) =>
            {
                columns.extend(
                    source
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(index, field)| (index, field.clone())),
                );
            }
            SelectItem::UnnamedExpr(expr) => {
                let index = metadata_expr_column(expr, &source.fields)?;
                columns.push((index, source.fields[index].clone()));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let index = metadata_expr_column(expr, &source.fields)?;
                let mut field = source.fields[index].clone();
                field.name.clone_from(&alias.value);
                columns.push((index, field));
            }
            _ => return Err(MetadataError::Unsupported(item.to_string())),
        }
    }
    let fields = columns.iter().map(|(_, field)| field.clone()).collect();
    let rows = source
        .rows
        .into_iter()
        .map(|row| {
            columns
                .iter()
                .map(|(index, _)| row[*index].clone())
                .collect()
        })
        .collect();
    Ok(MetadataResult { fields, rows })
}

fn wildcard_is_plain(options: &WildcardAdditionalOptions) -> bool {
    options == &WildcardAdditionalOptions::default()
}

fn metadata_expr_column(
    expression: &Expr,
    fields: &[MetadataField],
) -> Result<usize, MetadataError> {
    let name = match expression {
        Expr::Identifier(identifier) => identifier.value.as_str(),
        Expr::CompoundIdentifier(identifiers) => identifiers
            .last()
            .map(|identifier| identifier.value.as_str())
            .ok_or_else(|| MetadataError::Unsupported(expression.to_string()))?,
        _ => return Err(MetadataError::Unsupported(expression.to_string())),
    };
    fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| MetadataError::UnknownColumn(name.to_owned()))
}

fn evaluate_metadata_predicate(
    expression: &Expr,
    fields: &[MetadataField],
    row: &[Value],
) -> Result<bool, MetadataError> {
    match expression {
        Expr::Nested(expression) => evaluate_metadata_predicate(expression, fields, row),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(evaluate_metadata_predicate(left, fields, row)?
            && evaluate_metadata_predicate(right, fields, row)?),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => Ok(evaluate_metadata_predicate(left, fields, row)?
            || evaluate_metadata_predicate(right, fields, row)?),
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) =>
        {
            let equal = metadata_equal(
                &metadata_expr_value(left, fields, row)?,
                &metadata_expr_value(right, fields, row)?,
            );
            Ok(equal.is_some_and(|equal| {
                if *op == BinaryOperator::Eq {
                    equal
                } else {
                    !equal
                }
            }))
        }
        Expr::IsNull(expression) => Ok(matches!(
            metadata_expr_value(expression, fields, row)?,
            Value::Null
        )),
        Expr::IsNotNull(expression) => Ok(!matches!(
            metadata_expr_value(expression, fields, row)?,
            Value::Null
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = metadata_expr_value(expr, fields, row)?;
            if matches!(needle, Value::Null) {
                return Ok(false);
            }
            let mut found = false;
            let mut contains_null = false;
            for candidate in list {
                match metadata_equal(&needle, &metadata_expr_value(candidate, fields, row)?) {
                    Some(true) => found = true,
                    Some(false) => {}
                    None => contains_null = true,
                }
            }
            Ok(if found {
                !*negated
            } else if contains_null {
                false
            } else {
                *negated
            })
        }
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char: None,
        } => {
            let value = metadata_expr_value(expr, fields, row)?;
            let pattern = metadata_expr_value(pattern, fields, row)?;
            if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
                return Ok(false);
            }
            let matched = match (&value, &pattern) {
                (Value::Utf8(value), Value::Utf8(pattern)) => metadata_like(value, pattern),
                _ => false,
            };
            Ok(if *negated { !matched } else { matched })
        }
        _ => Err(MetadataError::Unsupported(expression.to_string())),
    }
}

fn metadata_expr_value(
    expression: &Expr,
    fields: &[MetadataField],
    row: &[Value],
) -> Result<Value, MetadataError> {
    match expression {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            Ok(row[metadata_expr_column(expression, fields)?].clone())
        }
        Expr::Nested(expression) => metadata_expr_value(expression, fields, row),
        Expr::Value(value) => match &value.value {
            SqlValue::SingleQuotedString(value)
            | SqlValue::DoubleQuotedString(value)
            | SqlValue::NationalStringLiteral(value) => Ok(utf8(value)),
            SqlValue::Number(value, _) => value
                .parse::<u64>()
                .map(Value::UInt64)
                .map_err(|_| MetadataError::Unsupported(expression.to_string())),
            SqlValue::Boolean(value) => Ok(Value::Boolean(*value)),
            SqlValue::Null => Ok(Value::Null),
            _ => Err(MetadataError::Unsupported(expression.to_string())),
        },
        _ => Err(MetadataError::Unsupported(expression.to_string())),
    }
}

fn metadata_equal(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Utf8(left), Value::Utf8(right)) => Some(left.eq_ignore_ascii_case(right)),
        _ => Some(left == right),
    }
}

fn metadata_like(value: &str, pattern: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        AnyMany,
        AnyOne,
        Literal(char),
    }

    let value = value.to_lowercase().chars().collect::<Vec<_>>();
    let lowercase_pattern = pattern.to_lowercase();
    let mut pattern = lowercase_pattern.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(character) = pattern.next() {
        match character {
            '%' => tokens.push(Token::AnyMany),
            '_' => tokens.push(Token::AnyOne),
            '\\' => tokens.push(Token::Literal(pattern.next().unwrap_or('\\'))),
            literal => tokens.push(Token::Literal(literal)),
        }
    }
    let mut matches = vec![false; value.len() + 1];
    matches[0] = true;
    for token in tokens {
        match token {
            Token::AnyMany => {
                for index in 1..=value.len() {
                    matches[index] |= matches[index - 1];
                }
            }
            Token::AnyOne | Token::Literal(_) => {
                for index in (1..=value.len()).rev() {
                    matches[index] = matches[index - 1]
                        && match token {
                            Token::AnyOne => true,
                            Token::Literal(literal) => literal == value[index - 1],
                            Token::AnyMany => unreachable!("handled above"),
                        };
                }
                matches[0] = false;
            }
        }
    }
    matches[value.len()]
}

fn order_metadata_result(result: &mut MetadataResult, query: &Query) -> Result<(), MetadataError> {
    let Some(order_by) = &query.order_by else {
        return Ok(());
    };
    let OrderByKind::Expressions(expressions) = &order_by.kind else {
        return Err(MetadataError::Unsupported(order_by.to_string()));
    };
    if order_by.interpolate.is_some()
        || expressions
            .iter()
            .any(|expression| expression.with_fill.is_some())
    {
        return Err(MetadataError::Unsupported(order_by.to_string()));
    }
    let keys = expressions
        .iter()
        .map(|expression| {
            let index = metadata_expr_column(&expression.expr, &result.fields)?;
            Ok((index, expression.options.asc.unwrap_or(true)))
        })
        .collect::<Result<Vec<_>, MetadataError>>()?;
    result.rows.sort_by(|left, right| {
        for (index, ascending) in &keys {
            let ordering = metadata_order(&left[*index], &right[*index]);
            if ordering != Ordering::Equal {
                return if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                };
            }
        }
        Ordering::Equal
    });
    Ok(())
}

fn metadata_order(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Utf8(left), Value::Utf8(right)) => left.to_lowercase().cmp(&right.to_lowercase()),
        _ => left.cmp(right),
    }
}

fn limit_metadata_result(result: &mut MetadataResult, query: &Query) -> Result<(), MetadataError> {
    let Some(limit) = &query.limit_clause else {
        return Ok(());
    };
    let (offset, count) = match limit {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => (
            offset
                .as_ref()
                .map_or(Ok(0), |offset| metadata_usize(&offset.value))?,
            metadata_usize(limit)?,
        ),
        LimitClause::OffsetCommaLimit { offset, limit } => {
            (metadata_usize(offset)?, metadata_usize(limit)?)
        }
        LimitClause::LimitOffset { .. } => {
            return Err(MetadataError::Unsupported(limit.to_string()));
        }
    };
    result.rows = result.rows.drain(..).skip(offset).take(count).collect();
    Ok(())
}

fn metadata_usize(expression: &Expr) -> Result<usize, MetadataError> {
    let Expr::Value(value) = expression else {
        return Err(MetadataError::Unsupported(expression.to_string()));
    };
    let SqlValue::Number(value, _) = &value.value else {
        return Err(MetadataError::Unsupported(expression.to_string()));
    };
    value
        .parse()
        .map_err(|_| MetadataError::Unsupported(expression.to_string()))
}

fn single_string_result<'a>(
    field: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> MetadataResult {
    MetadataResult {
        fields: vec![MetadataField {
            name: field.to_owned(),
            data_type: DataType::Utf8,
            nullable: false,
        }],
        rows: values
            .into_iter()
            .map(|value| vec![Value::Utf8(value.to_owned())])
            .collect(),
    }
}

fn show_tables(database: &DatabaseEntry, full: bool) -> MetadataResult {
    if !full {
        return single_string_result(
            &format!("Tables_in_{}", database.name()),
            database.tables().map(TableEntry::name),
        );
    }
    MetadataResult {
        fields: metadata_fields(&[
            (
                &format!("Tables_in_{}", database.name()),
                DataType::Utf8,
                false,
            ),
            ("Table_type", DataType::Utf8, false),
        ]),
        rows: database
            .tables()
            .map(|table| vec![utf8(table.name()), utf8("BASE TABLE")])
            .collect(),
    }
}

fn apply_show_filter(
    mut result: MetadataResult,
    options: &ShowStatementOptions,
) -> Result<MetadataResult, MetadataError> {
    let Some(position) = &options.filter_position else {
        return Ok(result);
    };
    let filter = match position {
        ShowStatementFilterPosition::Infix(filter)
        | ShowStatementFilterPosition::Suffix(filter) => filter,
    };
    result.rows = match filter {
        ShowStatementFilter::Like(pattern)
        | ShowStatementFilter::ILike(pattern)
        | ShowStatementFilter::NoKeyword(pattern) => result
            .rows
            .into_iter()
            .filter(|row| {
                let Value::Utf8(value) = &row[0] else {
                    return false;
                };
                metadata_like(value, pattern)
            })
            .collect(),
        ShowStatementFilter::Where(predicate) => {
            let mut rows = Vec::with_capacity(result.rows.len());
            for row in result.rows {
                if evaluate_metadata_predicate(predicate, &result.fields, &row)? {
                    rows.push(row);
                }
            }
            rows
        }
    };
    Ok(result)
}

fn describe_table(
    database: &DatabaseEntry,
    table: &TableEntry,
    facts: &SourceFacts,
) -> MetadataResult {
    let fields = metadata_fields(&[
        ("Field", DataType::Utf8, false),
        ("Type", DataType::Utf8, false),
        ("Null", DataType::Utf8, false),
        ("Key", DataType::Utf8, false),
        ("Default", DataType::Utf8, true),
        ("Extra", DataType::Utf8, false),
    ]);
    let rows = table
        .schema()
        .columns()
        .iter()
        .map(|column| {
            let fact = column_fact(database.name(), table.name(), column.name(), facts);
            let key = column_key(table, column.id(), fact);
            let extra = fact.map_or("", |fact| {
                if fact.auto_increment {
                    "auto_increment"
                } else if fact.generated_stored {
                    "STORED GENERATED"
                } else {
                    ""
                }
            });
            vec![
                Value::Utf8(column.name().to_owned()),
                Value::Utf8(mysql_type(column.data_type())),
                Value::Utf8(if column.is_nullable() { "YES" } else { "NO" }.to_owned()),
                utf8(key),
                fact.and_then(|fact| fact.default_value.as_deref())
                    .map_or(Value::Null, utf8),
                utf8(extra),
            ]
        })
        .collect();
    MetadataResult { fields, rows }
}

fn column_fact<'facts>(
    database: &str,
    table: &str,
    column: &str,
    facts: &'facts SourceFacts,
) -> Option<&'facts ColumnFacts> {
    facts.columns.iter().find(|fact| {
        fact.database.eq_ignore_ascii_case(database)
            && fact.table.eq_ignore_ascii_case(table)
            && fact.column.eq_ignore_ascii_case(column)
    })
}

fn mysql_type(data_type: DataType) -> String {
    match data_type {
        DataType::Boolean => "tinyint(1)".to_owned(),
        DataType::Int8 => "tinyint".to_owned(),
        DataType::Int16 => "smallint".to_owned(),
        DataType::Int32 => "int".to_owned(),
        DataType::Int64 => "bigint".to_owned(),
        DataType::UInt8 => "tinyint unsigned".to_owned(),
        DataType::UInt16 => "smallint unsigned".to_owned(),
        DataType::UInt32 => "int unsigned".to_owned(),
        DataType::UInt64 => "bigint unsigned".to_owned(),
        DataType::Float32 => "float".to_owned(),
        DataType::Float64 => "double".to_owned(),
        DataType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        DataType::Date32 => "date".to_owned(),
        DataType::DateTime64 { fsp } => format!("datetime({fsp})"),
        DataType::Time64 { fsp } => format!("time({fsp})"),
        DataType::Utf8 => "text".to_owned(),
        DataType::Binary => "blob".to_owned(),
        DataType::Json => "json".to_owned(),
    }
}

const fn mysql_data_type(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Boolean | DataType::Int8 | DataType::UInt8 => "tinyint",
        DataType::Int16 | DataType::UInt16 => "smallint",
        DataType::Int32 | DataType::UInt32 => "int",
        DataType::Int64 | DataType::UInt64 => "bigint",
        DataType::Float32 => "float",
        DataType::Float64 => "double",
        DataType::Decimal { .. } => "decimal",
        DataType::Date32 => "date",
        DataType::DateTime64 { .. } => "datetime",
        DataType::Time64 { .. } => "time",
        DataType::Utf8 => "text",
        DataType::Binary => "blob",
        DataType::Json => "json",
    }
}

fn resolve_show_database<'a>(
    options: &ShowStatementOptions,
    catalog: &'a CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<&'a DatabaseEntry, MetadataError> {
    let name = options
        .show_in
        .as_ref()
        .and_then(|show_in| show_in.parent_name.as_ref())
        .map(object_name_parts)
        .transpose()?
        .and_then(|parts| (parts.len() == 1).then(|| parts[0]))
        .or(current_database)
        .ok_or(MetadataError::NoCurrentDatabase)?;
    catalog
        .database(name)
        .ok_or_else(|| MetadataError::UnknownDatabase(name.to_owned()))
}

fn resolve_table<'a>(
    name: &ObjectName,
    catalog: &'a CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<(&'a DatabaseEntry, &'a TableEntry), MetadataError> {
    let parts = object_name_parts(name)?;
    let (database, table) = match parts.as_slice() {
        [table] => (
            current_database.ok_or(MetadataError::NoCurrentDatabase)?,
            *table,
        ),
        [database, table] => (*database, *table),
        _ => return Err(MetadataError::InvalidObjectName(name.to_string())),
    };
    let database = catalog
        .database(database)
        .ok_or_else(|| MetadataError::UnknownDatabase(database.to_owned()))?;
    let table = database
        .table(table)
        .ok_or_else(|| MetadataError::UnknownTable(table.to_owned()))?;
    Ok((database, table))
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<&str>, MetadataError> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|identifier| identifier.value.as_str())
                .ok_or_else(|| MetadataError::InvalidObjectName(name.to_string()))
        })
        .collect()
}

fn database_options(options: &ShowStatementOptions) -> bool {
    options.show_in.is_none() && filterable_options(options)
}

fn filterable_options(options: &ShowStatementOptions) -> bool {
    options.starts_with.is_none() && options.limit.is_none() && options.limit_from.is_none()
}

/// Metadata statement failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataError {
    /// Statement or extension is outside the supported compatibility surface.
    Unsupported(String),
    /// A table name requires a current database.
    NoCurrentDatabase,
    /// No catalog database has this name.
    UnknownDatabase(String),
    /// No catalog table has this name.
    UnknownTable(String),
    /// No metadata field has this name.
    UnknownColumn(String),
    /// An object name has an unsupported shape.
    InvalidObjectName(String),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(statement) => {
                write!(formatter, "unsupported metadata statement: {statement}")
            }
            Self::NoCurrentDatabase => formatter.write_str("no current database selected"),
            Self::UnknownDatabase(database) => write!(formatter, "unknown database {database}"),
            Self::UnknownTable(table) => write!(formatter, "unknown table {table}"),
            Self::UnknownColumn(column) => write!(formatter, "unknown column {column}"),
            Self::InvalidObjectName(name) => write!(formatter, "invalid object name {name}"),
        }
    }
}

impl std::error::Error for MetadataError {}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_types::{Column, DataType, KeyMode, TableSchema, Value};

    use crate::{SourceFacts, execute_metadata, parse_statement};

    fn catalog() -> CatalogSnapshot {
        let table = TableEntry::new(
            TableId::new(2),
            "Events",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(3),
        )
        .expect("table");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        CatalogSnapshot::new([database]).expect("catalog")
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one linear metadata walkthrough
    fn serves_constraint_metadata_and_show_create_table() {
        let table = TableEntry::new(
            TableId::new(2),
            "Events",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(3),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("key columns");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let facts = SourceFacts {
            columns: vec![
                crate::ColumnFacts {
                    database: "Analytics".to_owned(),
                    table: "Events".to_owned(),
                    column: "id".to_owned(),
                    default_value: None,
                    auto_increment: true,
                    generated_stored: false,
                    unique_single: false,
                },
                crate::ColumnFacts {
                    database: "Analytics".to_owned(),
                    table: "Events".to_owned(),
                    column: "name".to_owned(),
                    default_value: None,
                    auto_increment: false,
                    generated_stored: false,
                    unique_single: true,
                },
            ],
            indexes: vec![crate::IndexFacts {
                database: "Analytics".to_owned(),
                table: "Events".to_owned(),
                index_name: "unique_name".to_owned(),
                unique: true,
                columns: vec!["name".to_owned()],
            }],
            foreign_keys: vec![crate::ForeignKeyFacts {
                database: "Analytics".to_owned(),
                table: "Events".to_owned(),
                name: "fk_events_owner".to_owned(),
                columns: vec!["id".to_owned()],
                referenced_table: "Owners".to_owned(),
                referenced_columns: vec!["id".to_owned()],
                unique_constraint_name: Some("PRIMARY".to_owned()),
                update_rule: "NO ACTION".to_owned(),
                delete_rule: "CASCADE".to_owned(),
            }],
        };

        let statistics = execute_metadata(
            &parse_statement("SELECT * FROM information_schema.statistics").expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("statistics");
        assert_eq!(statistics.rows.len(), 2);
        assert_eq!(statistics.rows[0][5], Value::Utf8("PRIMARY".to_owned()));
        assert_eq!(statistics.rows[0][7], Value::Utf8("id".to_owned()));
        assert_eq!(statistics.rows[1][5], Value::Utf8("unique_name".to_owned()));

        let usage = execute_metadata(
            &parse_statement("SELECT * FROM information_schema.key_column_usage").expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("key_column_usage");
        assert_eq!(usage.rows.len(), 3);
        assert_eq!(usage.rows[0][2], Value::Utf8("PRIMARY".to_owned()));
        assert_eq!(usage.rows[2][2], Value::Utf8("fk_events_owner".to_owned()));
        assert_eq!(usage.rows[2][10], Value::Utf8("Owners".to_owned()));

        let constraints = execute_metadata(
            &parse_statement("SELECT * FROM information_schema.table_constraints").expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("table_constraints");
        assert_eq!(constraints.rows.len(), 3);
        assert_eq!(
            constraints.rows[0][5],
            Value::Utf8("PRIMARY KEY".to_owned())
        );
        assert_eq!(constraints.rows[1][5], Value::Utf8("UNIQUE".to_owned()));
        assert_eq!(
            constraints.rows[2][5],
            Value::Utf8("FOREIGN KEY".to_owned())
        );

        let referential = execute_metadata(
            &parse_statement("SELECT * FROM information_schema.referential_constraints")
                .expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("referential_constraints");
        assert_eq!(referential.rows.len(), 1);
        assert_eq!(
            referential.rows[0][2],
            Value::Utf8("fk_events_owner".to_owned())
        );
        assert_eq!(referential.rows[0][8], Value::Utf8("CASCADE".to_owned()));
        assert_eq!(referential.rows[0][10], Value::Utf8("Owners".to_owned()));

        let create = execute_metadata(
            &parse_statement("SHOW CREATE TABLE Analytics.Events").expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("show create");
        let Value::Utf8(ddl) = &create.rows[0][1] else {
            panic!("DDL cell must be text");
        };
        assert!(ddl.contains("PRIMARY KEY (`id`)"), "{ddl}");
        assert!(ddl.contains("UNIQUE KEY `unique_name` (`name`)"), "{ddl}");
        assert!(ddl.contains("AUTO_INCREMENT"), "{ddl}");

        let columns = execute_metadata(
            &parse_statement("SHOW COLUMNS FROM Analytics.Events").expect("parse"),
            &catalog,
            None,
            &facts,
        )
        .expect("show columns");
        assert_eq!(columns.rows[0][3], Value::Utf8("PRI".to_owned()));
        assert_eq!(columns.rows[0][4], Value::Null);
        assert_eq!(columns.rows[0][5], Value::Utf8("auto_increment".to_owned()));
        assert_eq!(columns.rows[1][3], Value::Utf8("UNI".to_owned()));
    }

    #[test]
    fn serves_show_and_describe_from_one_catalog_snapshot() {
        let catalog = catalog();
        let databases = execute_metadata(
            &parse_statement("SHOW DATABASES").expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("databases");
        assert_eq!(databases.rows, [vec![Value::Utf8("Analytics".to_owned())]]);

        let tables = execute_metadata(
            &parse_statement("SHOW TABLES FROM Analytics").expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("tables");
        assert_eq!(tables.rows, [vec![Value::Utf8("Events".to_owned())]]);

        let columns = execute_metadata(
            &parse_statement("DESCRIBE Analytics.Events").expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("columns");
        assert_eq!(columns.fields[0].name, "Field");
        assert_eq!(columns.rows[0][0], Value::Utf8("id".to_owned()));
        assert_eq!(
            columns.rows[0][1],
            Value::Utf8("bigint unsigned".to_owned())
        );
        assert_eq!(columns.rows[1][2], Value::Utf8("YES".to_owned()));
    }

    #[test]
    fn show_full_tables_reports_mysql_table_type() {
        let result = execute_metadata(
            &parse_statement("SHOW FULL TABLES FROM Analytics").expect("parse"),
            &catalog(),
            None,
            &SourceFacts::default(),
        )
        .expect("full tables");

        assert_eq!(
            result
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["Tables_in_Analytics", "Table_type"]
        );
        assert_eq!(
            result.rows,
            [vec![
                Value::Utf8("Events".to_owned()),
                Value::Utf8("BASE TABLE".to_owned()),
            ]]
        );
    }

    #[test]
    fn show_index_exposes_statistics_in_mysql_shape() {
        let table = TableEntry::new(
            TableId::new(2),
            "Events",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(3),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("primary key");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");

        let result = execute_metadata(
            &parse_statement("SHOW INDEX FROM Events FROM Analytics").expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("show index");

        assert_eq!(
            result
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Table",
                "Non_unique",
                "Key_name",
                "Seq_in_index",
                "Column_name",
                "Collation",
                "Cardinality",
                "Sub_part",
                "Packed",
                "Null",
                "Index_type",
                "Comment",
                "Index_comment",
                "Visible",
                "Expression",
            ]
        );
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Utf8("Events".to_owned()));
        assert_eq!(result.rows[0][1], Value::Int64(0));
        assert_eq!(result.rows[0][2], Value::Utf8("PRIMARY".to_owned()));
        assert_eq!(result.rows[0][3], Value::UInt64(1));
        assert_eq!(result.rows[0][4], Value::Utf8("id".to_owned()));
    }

    #[test]
    fn information_schema_views_explicitly_reports_no_replicated_views() {
        let result = execute_metadata(
            &parse_statement("SELECT * FROM information_schema.views").expect("parse"),
            &catalog(),
            None,
            &SourceFacts::default(),
        )
        .expect("views metadata");

        assert!(result.rows.is_empty());
        assert_eq!(
            result
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            [
                "TABLE_CATALOG",
                "TABLE_SCHEMA",
                "TABLE_NAME",
                "VIEW_DEFINITION",
                "CHECK_OPTION",
                "IS_UPDATABLE",
                "DEFINER",
                "SECURITY_TYPE",
                "CHARACTER_SET_CLIENT",
                "COLLATION_CONNECTION",
            ]
        );
    }

    #[test]
    fn show_index_accepts_mysql_keys_indexes_and_in_synonyms() {
        let table = TableEntry::new(
            TableId::new(2),
            "Events",
            TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)])
                .expect("schema"),
            TableStatistics::default(),
        )
        .expect("table")
        .with_key_columns([1])
        .expect("primary key");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");

        for (sql, current_database) in [
            ("SHOW KEYS IN Events", Some("Analytics")),
            ("SHOW INDEXES FROM Events IN Analytics", None),
        ] {
            let result = execute_metadata(
                &parse_statement(sql).expect("parse"),
                &catalog,
                current_database,
                &SourceFacts::default(),
            )
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
            assert_eq!(result.rows[0][2], Value::Utf8("PRIMARY".to_owned()));
        }
    }

    #[test]
    fn show_tables_like_filters_names_case_insensitively() {
        let result = execute_metadata(
            &parse_statement("SHOW TABLES FROM Analytics LIKE 'eve%'").expect("parse"),
            &catalog(),
            None,
            &SourceFacts::default(),
        )
        .expect("filtered tables");

        assert_eq!(result.rows, [vec![Value::Utf8("Events".to_owned())]]);
    }

    #[test]
    fn metadata_like_counts_unicode_characters_and_honors_escapes() {
        assert!(super::metadata_like("é", "_"));
        assert!(super::metadata_like("foo_bar", r"foo\_bar"));
        assert!(super::metadata_like("foo%bar", r"foo\%bar"));
        assert!(!super::metadata_like("fooxbar", r"foo\_bar"));
    }

    #[test]
    fn show_columns_labels_the_selected_unique_key_as_unique() {
        let table = TableEntry::new(
            TableId::new(3),
            "Users",
            TableSchema::with_key_mode(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "email", DataType::Utf8, false),
                ],
                KeyMode::Unique,
            )
            .expect("unique-key schema"),
            TableStatistics::default(),
        )
        .expect("table")
        .with_key_columns([2])
        .expect("key columns");
        let database =
            DatabaseEntry::new(DatabaseId::new(1), "Analytics", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");

        let columns = execute_metadata(
            &parse_statement("SHOW COLUMNS FROM Analytics.Users").expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("show columns");
        assert_eq!(columns.rows[0][3], Value::Utf8(String::new()));
        assert_eq!(columns.rows[1][3], Value::Utf8("UNI".to_owned()));
    }

    #[test]
    fn show_databases_like_filters_names() {
        let result = execute_metadata(
            &parse_statement("SHOW DATABASES LIKE 'ana%'").expect("parse"),
            &catalog(),
            None,
            &SourceFacts::default(),
        )
        .expect("filtered databases");

        assert_eq!(result.rows, [vec![Value::Utf8("Analytics".to_owned())]]);
    }

    #[test]
    fn show_columns_like_filters_field_names() {
        let result = execute_metadata(
            &parse_statement("SHOW COLUMNS FROM Events FROM Analytics LIKE 'na%'").expect("parse"),
            &catalog(),
            None,
            &SourceFacts::default(),
        )
        .expect("filtered columns");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Utf8("name".to_owned()));
    }

    #[test]
    fn serves_basic_information_schema_queries() {
        let catalog = catalog();
        let schemata = execute_metadata(
            &parse_statement(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name = 'analytics'",
            )
            .expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("schemata");
        assert_eq!(schemata.rows, [vec![Value::Utf8("Analytics".to_owned())]]);

        let tables = execute_metadata(
            &parse_statement(
                "SELECT table_name, table_rows FROM information_schema.tables \
                 WHERE table_schema = 'Analytics' ORDER BY table_name LIMIT 1",
            )
            .expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("tables");
        assert_eq!(
            tables.rows,
            [vec![Value::Utf8("Events".to_owned()), Value::UInt64(3)]]
        );

        let columns = execute_metadata(
            &parse_statement(
                "SELECT column_name AS name, ordinal_position, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = 'analytics' AND table_name LIKE 'eve%' \
                 ORDER BY ordinal_position",
            )
            .expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("columns");
        assert_eq!(columns.fields[0].name, "name");
        assert_eq!(
            columns.rows,
            [
                vec![
                    Value::Utf8("id".to_owned()),
                    Value::UInt64(1),
                    Value::Utf8("NO".to_owned()),
                ],
                vec![
                    Value::Utf8("name".to_owned()),
                    Value::UInt64(2),
                    Value::Utf8("YES".to_owned()),
                ],
            ]
        );

        let count = execute_metadata(
            &parse_statement(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_schema = 'Analytics'",
            )
            .expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("count");
        assert_eq!(count.rows, [vec![Value::UInt64(2)]]);
    }

    #[test]
    fn information_schema_filters_null_with_mysql_three_valued_logic() {
        let catalog = catalog();
        for predicate in [
            "column_default = NULL",
            "column_default != 'value'",
            "column_default NOT IN ('value')",
            "column_name NOT IN ('missing', NULL)",
            "column_default NOT LIKE '%'",
        ] {
            let sql = format!("SELECT COUNT(*) FROM information_schema.columns WHERE {predicate}");
            let result = execute_metadata(
                &parse_statement(&sql).expect("parse"),
                &catalog,
                None,
                &SourceFacts::default(),
            )
            .expect("metadata query");
            assert_eq!(
                result.rows,
                [vec![Value::UInt64(0)]],
                "predicate: {predicate}"
            );
        }

        let result = execute_metadata(
            &parse_statement(
                "SELECT column_name FROM information_schema.columns \
                 WHERE column_name IN ('id', NULL)",
            )
            .expect("parse"),
            &catalog,
            None,
            &SourceFacts::default(),
        )
        .expect("metadata query");
        assert_eq!(result.rows, [vec![Value::Utf8("id".to_owned())]]);
    }
}
