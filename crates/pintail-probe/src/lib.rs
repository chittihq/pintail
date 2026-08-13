//! `MySQL` source capability and schema probing for Pintail.
//!
//! Probing is deliberately read-only. It captures enough source metadata to
//! choose CDC versus polling, plan a consistent snapshot, select the physical
//! key fallback, and build Pintail schemas without guessing from result values.

use std::collections::BTreeMap;

use mysql_async::{Pool, prelude::Queryable};
use pintail_types::{Column, DataType, KeyMode, SchemaError, TableSchema};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Complete source capability and schema report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeReport {
    /// Source database that was inspected.
    pub database: String,
    /// Server product/version information.
    pub server: ServerIdentity,
    /// Variables relevant to snapshot and CDC operation.
    pub variables: BTreeMap<String, String>,
    /// Grants returned for the connected account.
    pub grants: Vec<String>,
    /// Derived replication capabilities.
    pub capabilities: SourceCapabilities,
    /// Source tables in deterministic name order.
    pub tables: Vec<SourceTable>,
    /// Explicit compatibility or fidelity warnings.
    pub warnings: Vec<String>,
}

/// Identifies the source server family.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFlavor {
    /// Oracle `MySQL`.
    Mysql,
    /// `MariaDB`.
    MariaDb,
}

/// Source product information.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerIdentity {
    /// Raw `@@version` value.
    pub version: String,
    /// Raw `@@version_comment` value.
    pub version_comment: String,
    /// Product family inferred from the server's own version strings.
    pub flavor: SourceFlavor,
}

/// Recommended continuous-replication mode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedMode {
    /// Native row-binlog CDC can be used.
    Cdc,
    /// The source must use polling/reconciliation.
    Polling,
}

/// Derived source capabilities used by the replication coordinator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SourceCapabilities {
    /// Whether the binary log is enabled.
    pub log_bin: bool,
    /// Whether the binary log format is `ROW`.
    pub row_binlog: bool,
    /// Whether row events contain full before/after images.
    pub full_row_image: bool,
    /// Whether full row metadata is enabled.
    pub full_row_metadata: bool,
    /// Whether the account has both replication stream and position grants.
    pub replication_grants: bool,
    /// Whether the account appears able to acquire the brief global read lock.
    pub global_read_lock: bool,
    /// Whether GTID mode is available.
    pub gtid_available: bool,
    /// Effective mode recommendation.
    pub recommended_mode: RecommendedMode,
    /// Human-readable reasons when CDC is unavailable or snapshot locking will
    /// degrade.
    pub reasons: Vec<String>,
}

/// One probed source table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceTable {
    /// Source table name.
    pub name: String,
    /// Storage engine reported by `MySQL`.
    pub engine: Option<String>,
    /// Source row count: exact when it could be counted inside the probe's
    /// budget, otherwise the storage engine's estimate.
    pub estimated_rows: Option<u64>,
    /// Whether `estimated_rows` was counted exactly rather than estimated.
    ///
    /// The UI needs this: a number rendered without a "~" that is actually a
    /// twenty-page `InnoDB` sample invites someone to reconcile it against their
    /// own `COUNT(*)` and find it wrong.
    #[serde(default)]
    pub rows_are_exact: bool,
    /// Included physical columns in ordinal order.
    pub columns: Vec<SourceColumn>,
    /// Selected primary/unique/append key strategy.
    pub key: SourceKey,
    /// Complete non-null, non-prefix UNIQUE constraints available for polling
    /// collision audits.
    pub unique_keys: Vec<Vec<String>>,
    /// Whether invisible cascading foreign-key changes require periodic
    /// primary-key reconciliation even in CDC mode.
    pub requires_reconciliation: bool,
    /// Foreign-key constraints on this table, for metadata fidelity.
    #[serde(default)]
    pub foreign_keys: Vec<SourceForeignKey>,
    /// Table-specific mapping warnings.
    pub warnings: Vec<String>,
}

/// One `REFERENTIAL_CONSTRAINTS` row: constraint name, delete rule, update
/// rule, referenced unique-constraint name, referenced table.
type CascadeRuleRow = (String, String, String, Option<String>, Option<String>);

/// One foreign-key constraint captured from the source.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, Serialize)]
pub struct SourceForeignKey {
    /// Constraint name.
    pub name: String,
    /// Constrained column names in constraint order.
    pub columns: Vec<String>,
    /// Referenced (parent) table name.
    pub referenced_table: String,
    /// Referenced column names, parallel to `columns`.
    pub referenced_columns: Vec<String>,
    /// Referenced constraint name (usually `PRIMARY`), when reported.
    pub unique_constraint_name: Option<String>,
    /// `ON UPDATE` rule text.
    pub update_rule: String,
    /// `ON DELETE` rule text.
    pub delete_rule: String,
}

impl SourceTable {
    /// Builds the logical Pintail schema discovered by the probe.
    ///
    /// # Errors
    ///
    /// Returns an error if source metadata produced an invalid Pintail schema.
    pub fn table_schema(&self) -> Result<TableSchema, SchemaError> {
        self.table_schema_with_version(1)
    }

    /// Builds the logical schema at a caller-owned catalog generation.
    ///
    /// # Errors
    ///
    /// Returns an error if source metadata produced an invalid Pintail schema.
    pub fn table_schema_with_version(&self, version: u32) -> Result<TableSchema, SchemaError> {
        TableSchema::with_key_mode(
            version,
            self.columns
                .iter()
                .map(|column| {
                    let sort_key = self
                        .key
                        .columns
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&column.name));
                    Column::new(column.id, &column.name, column.pintail_type, !sort_key)
                        .with_collation(column.collation.clone())
                        // ENUM only: SET has no single ordinal, so it stays text.
                        .with_enum_labels(
                            column
                                .mysql_data_type
                                .eq_ignore_ascii_case("enum")
                                .then(|| {
                                    pintail_types::declaration_labels(
                                        &column.mysql_column_type,
                                        "enum",
                                    )
                                })
                                .flatten(),
                        )
                })
                .collect(),
            self.key.mode,
        )
    }

    /// Returns stable schema column IDs used to construct the physical key.
    #[must_use]
    pub fn key_column_ids(&self) -> Vec<u32> {
        self.key
            .columns
            .iter()
            .filter_map(|name| {
                self.columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(name))
                    .map(|column| column.id)
            })
            .collect()
    }
}

/// Source column metadata and its lossless Pintail mapping.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // mirrors independent source metadata flags
pub struct SourceColumn {
    /// Stable source ordinal, used as the initial Pintail column ID.
    pub id: u32,
    /// Source column name.
    pub name: String,
    /// Raw `INFORMATION_SCHEMA.COLUMNS.DATA_TYPE`.
    pub mysql_data_type: String,
    /// Raw display type, including unsigned/enum/set declarations.
    pub mysql_column_type: String,
    /// Exact logical Pintail type.
    pub pintail_type: DataType,
    /// Whether the source column permits `NULL`.
    pub nullable: bool,
    /// Source character set, when textual.
    pub character_set: Option<String>,
    /// Source collation, when textual.
    pub collation: Option<String>,
    /// Whether this is a generated stored column.
    pub generated_stored: bool,
    /// Raw stored-generation expression, without the surrounding `AS (...)`.
    #[serde(default)]
    pub generation_expression: String,
    /// Raw `INFORMATION_SCHEMA.COLUMNS.EXTRA` metadata.
    #[serde(default)]
    pub extra: String,
    /// Whether the source declares this column `AUTO_INCREMENT`.
    pub auto_increment: bool,
    /// Raw `INFORMATION_SCHEMA.COLUMNS.COLUMN_DEFAULT`, absent when the
    /// column has no default (older stored probes decode to `None` too).
    #[serde(default)]
    pub default_value: Option<String>,
    /// Whether `EXTRA` identifies the default as an evaluated expression.
    #[serde(default)]
    pub default_generated: bool,
}

/// Physical key selected from source indexes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceKey {
    /// Pintail key fallback mode.
    pub mode: KeyMode,
    /// Source index name, absent for append-row-id tables.
    pub index_name: Option<String>,
    /// Source column names in index order.
    pub columns: Vec<String>,
}

/// Probe failure.
#[derive(Debug, Error)]
pub enum ProbeError {
    /// `MySQL` protocol or query failure.
    #[error("MySQL probe failed: {0}")]
    Mysql(#[from] mysql_async::Error),
    /// Source metadata is internally inconsistent or unsupported.
    #[error("invalid MySQL metadata: {0}")]
    InvalidMetadata(String),
}

#[derive(Clone, Debug)]
struct RawColumn {
    ordinal: u32,
    name: String,
    nullable: bool,
    data_type: String,
    column_type: String,
    numeric_precision: Option<u8>,
    numeric_scale: Option<u8>,
    datetime_precision: Option<u8>,
    character_set: Option<String>,
    collation: Option<String>,
    extra: String,
    generation_expression: String,
    default_value: Option<String>,
}

#[derive(Clone, Debug)]
struct RawIndexPart {
    name: String,
    non_unique: bool,
    sequence: u32,
    column: String,
    prefix_length: Option<u64>,
}
/// How long one table's exact count may run before it is abandoned.
const COUNT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// How long counting may run across the WHOLE probe before the rest of the
/// tables fall back to statistics.
///
/// The per-table budget alone is not a bound on the wait: a hundred large
/// tables would each spend their thirty seconds and leave someone staring at
/// a connection screen for the best part of an hour. Once this is spent the
/// remaining tables report the estimate, which is what they would have
/// reported anyway before counting existed.
const COUNT_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

/// Counts one table exactly, abandoning the attempt at [`COUNT_BUDGET`].
///
/// `COUNT(*)` on `InnoDB` is a full index scan - there is no stored row count -
/// so on a large table it runs for minutes. The bound is applied by the SERVER
/// first, through `MAX_EXECUTION_TIME` on `MySQL` and `max_statement_time` on
/// `MariaDB`, because that is the only bound that actually stops the work:
/// dropping the client connection leaves the scan running to completion for
/// nobody, holding a read view open the whole time.
///
/// The client-side deadline behind it exists for servers that ignore the hint,
/// which predates `MySQL` 5.7.8, and it does not merely give up: it issues
/// `KILL QUERY` on a second connection, because giving up without killing is
/// precisely the failure the server-side bound is there to prevent.
async fn count_rows_within_budget(
    pool: &Pool,
    connection: &mut mysql_async::Conn,
    connection_id: u64,
    database: &str,
    table: &str,
    flavor: SourceFlavor,
) -> Option<u64> {
    let budget_ms = COUNT_BUDGET.as_millis();
    let target = format!(
        "`{}`.`{}`",
        database.replace('`', "``"),
        table.replace('`', "``")
    );
    let sql = match flavor {
        SourceFlavor::Mysql => {
            format!("SELECT /*+ MAX_EXECUTION_TIME({budget_ms}) */ COUNT(*) FROM {target}")
        }
        SourceFlavor::MariaDb => {
            let seconds = COUNT_BUDGET.as_secs();
            format!("SET STATEMENT max_statement_time={seconds} FOR SELECT COUNT(*) FROM {target}")
        }
    };
    // A little past the server's own bound, so the server wins the race in the
    // ordinary case and this only fires when the hint was ignored.
    let deadline = COUNT_BUDGET.saturating_add(std::time::Duration::from_secs(5));
    match tokio::time::timeout(deadline, connection.query_first::<u64, _>(sql)).await {
        Ok(Ok(Some(rows))) => Some(rows),
        // The server aborted it at the budget, which is the expected outcome
        // for a table too large to count.
        Ok(Err(error)) => {
            pintail_log::log_info!(
                "probe count abandoned db={database} table={table}: {error}; using statistics"
            );
            None
        }
        Ok(Ok(None)) => None,
        Err(_) => {
            pintail_log::log_info!(
                "probe count exceeded {}s db={database} table={table}; killing query {connection_id}",
                deadline.as_secs()
            );
            // A separate connection: the one running the count cannot issue
            // this, which is the whole reason a client-side timeout without a
            // kill leaves the server scanning.
            match pool.get_conn().await {
                Ok(mut killer) => {
                    if let Err(error) = killer
                        .query_drop(format!("KILL QUERY {connection_id}"))
                        .await
                    {
                        pintail_log::log_info!(
                            "probe could not kill query {connection_id}: {error}"
                        );
                    }
                }
                Err(error) => {
                    pintail_log::log_info!("probe could not open a killer connection: {error}");
                }
            }
            None
        }
    }
}

/// Counts one table when the probe still has counting time left, charging what
/// it spends against the whole-probe budget.
///
/// Split out of `probe` so the loop reads as what it is - metadata per table -
/// rather than burying the budget arithmetic in it.
#[allow(clippy::too_many_arguments)]
async fn count_if_budget_remains(
    pool: &Pool,
    connection: &mut mysql_async::Conn,
    connection_id: u64,
    database: &str,
    table: &str,
    flavor: SourceFlavor,
    spent: &mut std::time::Duration,
) -> Option<u64> {
    if *spent >= COUNT_TOTAL_BUDGET {
        return None;
    }
    let started = std::time::Instant::now();
    let counted =
        count_rows_within_budget(pool, connection, connection_id, database, table, flavor).await;
    *spent = spent.saturating_add(started.elapsed());
    counted
}

/// Probes one database through a real `mysql_async` connection.
///
/// # Errors
///
/// Returns a protocol error or rejects inconsistent source metadata.
#[allow(clippy::too_many_lines)] // one linear walk: identity, then table by table
pub async fn probe(pool: &Pool, database: &str) -> Result<ProbeReport, ProbeError> {
    if database.is_empty() {
        return Err(ProbeError::InvalidMetadata(
            "database name cannot be empty".to_owned(),
        ));
    }
    let mut connection = pool.get_conn().await?;
    let (version, version_comment): (String, String) = connection
        .query_first("SELECT @@version, @@version_comment")
        .await?
        .ok_or_else(|| ProbeError::InvalidMetadata("server identity query was empty".to_owned()))?;
    let flavor = if version.to_ascii_lowercase().contains("mariadb")
        || version_comment.to_ascii_lowercase().contains("mariadb")
    {
        SourceFlavor::MariaDb
    } else {
        SourceFlavor::Mysql
    };
    let variable_rows: Vec<(String, String)> = connection
        .query(
            "SHOW VARIABLES WHERE Variable_name IN (\
             'log_bin','binlog_format','binlog_row_image','binlog_row_metadata',\
             'gtid_mode','gtid_strict_mode','binlog_expire_logs_seconds')",
        )
        .await?;
    let variables = variable_rows
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect::<BTreeMap<_, _>>();
    let grants: Vec<String> = connection.query("SHOW GRANTS FOR CURRENT_USER()").await?;
    let capabilities = derive_capabilities(&variables, &grants, flavor);

    // Needed to KILL a count that overruns; it identifies the session the
    // count runs on, and only a different session can kill it.
    let connection_id: u64 = connection
        .query_first("SELECT CONNECTION_ID()")
        .await?
        .unwrap_or_default();
    let raw_tables: Vec<(String, Option<String>, Option<u64>)> = connection
        .exec(
            "SELECT TABLE_NAME, ENGINE, TABLE_ROWS \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
             ORDER BY TABLE_NAME",
            (database,),
        )
        .await?;
    // Cost here scales with table count, not schema size: each table below
    // issues its own column, index, foreign-key and key-usage queries. A
    // measured 82-table source took 11.6 seconds, which is long enough that
    // a caller with a deadline can abandon a probe the server then completes
    // for nobody - and until these lines existed there was no way to tell
    // that apart from a probe that hung.
    let started = std::time::Instant::now();
    let total = raw_tables.len();
    pintail_log::log_info!("probe start db={database} tables={total}");
    let mut counting_spent = std::time::Duration::ZERO;
    let mut tables = Vec::with_capacity(raw_tables.len());
    let mut warnings = Vec::new();
    for (index, (name, engine, estimated_rows)) in raw_tables.into_iter().enumerate() {
        // Per table, so a single pathological table is identifiable rather
        // than hiding inside one aggregate duration.
        let table_started = std::time::Instant::now();
        let probed_name = name.clone();
        let counted = count_if_budget_remains(
            pool,
            &mut connection,
            connection_id,
            database,
            &name,
            flavor,
            &mut counting_spent,
        )
        .await;
        let table = probe_table(
            &mut connection,
            database,
            name,
            engine,
            counted.or(estimated_rows),
            counted.is_some(),
        )
        .await?;
        pintail_log::log_debug!(
            "probe table db={database} table={probed_name} {}/{total} {}ms",
            index + 1,
            table_started.elapsed().as_millis()
        );
        warnings.extend(
            table
                .warnings
                .iter()
                .map(|warning| format!("{}: {warning}", table.name)),
        );
        tables.push(table);
    }
    pintail_log::log_info!(
        "probe done db={database} tables={total} warnings={} {}ms",
        warnings.len(),
        started.elapsed().as_millis()
    );

    Ok(ProbeReport {
        database: database.to_owned(),
        server: ServerIdentity {
            version,
            version_comment,
            flavor,
        },
        variables,
        grants,
        capabilities,
        tables,
        warnings,
    })
}

#[allow(clippy::too_many_lines)]
async fn probe_table(
    connection: &mut mysql_async::Conn,
    database: &str,
    table: String,
    engine: Option<String>,
    estimated_rows: Option<u64>,
    rows_are_exact: bool,
) -> Result<SourceTable, ProbeError> {
    type IndexRow = (String, u8, u32, Option<String>, Option<u64>);
    let column_rows: Vec<mysql_async::Row> = connection
        .exec(
            "SELECT COLUMN_NAME, ORDINAL_POSITION, IS_NULLABLE, DATA_TYPE, COLUMN_TYPE, \
                    NUMERIC_PRECISION, NUMERIC_SCALE, DATETIME_PRECISION, \
                    CHARACTER_SET_NAME, COLLATION_NAME, EXTRA, GENERATION_EXPRESSION, \
                    COLUMN_DEFAULT \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            (database, &table),
        )
        .await?;
    let raw_columns = column_rows
        .into_iter()
        .map(|mut row| {
            fn take<T>(
                row: &mut mysql_async::Row,
                index: usize,
                what: &str,
            ) -> Result<T, ProbeError>
            where
                T: mysql_async::prelude::FromValue,
            {
                row.take_opt(index)
                    .ok_or_else(|| {
                        ProbeError::InvalidMetadata(format!("column row is missing {what}"))
                    })?
                    .map_err(|error| {
                        ProbeError::InvalidMetadata(format!("column row {what}: {error:?}"))
                    })
            }
            let name: String = take(&mut row, 0, "COLUMN_NAME")?;
            let ordinal: u32 = take(&mut row, 1, "ORDINAL_POSITION")?;
            let nullable: String = take(&mut row, 2, "IS_NULLABLE")?;
            let data_type: String = take(&mut row, 3, "DATA_TYPE")?;
            let column_type: String = take(&mut row, 4, "COLUMN_TYPE")?;
            let numeric_precision: Option<u8> = take(&mut row, 5, "NUMERIC_PRECISION")?;
            let numeric_scale: Option<u8> = take(&mut row, 6, "NUMERIC_SCALE")?;
            let datetime_precision: Option<u8> = take(&mut row, 7, "DATETIME_PRECISION")?;
            let character_set: Option<String> = take(&mut row, 8, "CHARACTER_SET_NAME")?;
            let collation: Option<String> = take(&mut row, 9, "COLLATION_NAME")?;
            let extra: String = take(&mut row, 10, "EXTRA")?;
            let generation_expression: Option<String> =
                take(&mut row, 11, "GENERATION_EXPRESSION")?;
            let default_value: Option<String> = take(&mut row, 12, "COLUMN_DEFAULT")?;
            Ok(RawColumn {
                ordinal,
                name,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                data_type,
                column_type,
                numeric_precision,
                numeric_scale,
                datetime_precision,
                character_set,
                collation,
                extra,
                generation_expression: generation_expression.unwrap_or_default(),
                default_value,
            })
        })
        .collect::<Result<Vec<_>, ProbeError>>()?;
    if raw_columns.is_empty() {
        return Err(ProbeError::InvalidMetadata(format!(
            "table {table} has no columns"
        )));
    }

    let index_rows: Vec<IndexRow> = connection
        .exec(
            "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, SUB_PART \
             FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY CASE WHEN INDEX_NAME = 'PRIMARY' THEN 0 ELSE 1 END, \
                      INDEX_NAME, SEQ_IN_INDEX",
            (database, &table),
        )
        .await?;
    let index_parts = index_rows
        .into_iter()
        .filter_map(|(name, non_unique, sequence, column, prefix_length)| {
            column.map(|column| RawIndexPart {
                name,
                non_unique: non_unique != 0,
                sequence,
                column,
                prefix_length,
            })
        })
        .collect::<Vec<_>>();
    let key = choose_key(&raw_columns, &index_parts);
    let unique_keys = usable_unique_keys(&raw_columns, &index_parts);
    let cascade_rules: Vec<CascadeRuleRow> = connection
        .exec(
            "SELECT CONSTRAINT_NAME, DELETE_RULE, UPDATE_RULE, \
                    UNIQUE_CONSTRAINT_NAME, REFERENCED_TABLE_NAME \
             FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY CONSTRAINT_NAME",
            (database, &table),
        )
        .await?;
    let member_rows: Vec<(String, String, Option<String>)> = connection
        .exec(
            "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
               AND REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
            (database, &table),
        )
        .await?;
    let foreign_keys = cascade_rules
        .iter()
        .filter_map(
            |(name, delete_rule, update_rule, unique_constraint, referenced_table)| {
                let referenced_table = referenced_table.clone()?;
                let mut columns = Vec::new();
                let mut referenced_columns = Vec::new();
                for (member, column, referenced) in &member_rows {
                    if member == name {
                        columns.push(column.clone());
                        referenced_columns.push(referenced.clone()?);
                    }
                }
                (!columns.is_empty()).then(|| SourceForeignKey {
                    name: name.clone(),
                    columns,
                    referenced_table,
                    referenced_columns,
                    unique_constraint_name: unique_constraint.clone(),
                    update_rule: update_rule.clone(),
                    delete_rule: delete_rule.clone(),
                })
            },
        )
        .collect::<Vec<_>>();

    let mut columns = Vec::with_capacity(raw_columns.len());
    let mut warnings = Vec::new();
    for (constraint, delete_rule, update_rule, _, _) in &cascade_rules {
        if invisible_fk_rule(delete_rule) || invisible_fk_rule(update_rule) {
            warnings.push(format!(
                "foreign key {constraint} uses DELETE {delete_rule}/UPDATE {update_rule}; \
                 scheduling primary-key reconciliation because cascades are absent from row binlogs"
            ));
        }
    }
    for raw in raw_columns {
        let (generated, generated_stored) = generated_flags(&raw.generation_expression, &raw.extra);
        if generated && !generated_stored {
            warnings.push(format!(
                "skipping virtual generated column {} because it is absent from row binlog images",
                raw.name
            ));
            continue;
        }
        let mapping = map_mysql_type(&raw)?;
        if let Some(warning) = mapping.warning {
            warnings.push(format!("column {}: {warning}", raw.name));
        }
        // Named here because the alternative is discovering it months later.
        // A column whose collation the executor cannot compare still snapshots,
        // still replicates and still reads back, so nothing looks wrong until
        // the first WHERE, JOIN, GROUP BY or ORDER BY touches it - by which
        // point the source is in production and the report is "a filter that
        // works in MySQL returns an error here".
        if let Some(collation) = raw.collation.as_deref()
            && !is_supported_collation(collation)
        {
            warnings.push(format!(
                "column {}: text collation {collation} cannot be compared; the column \
                 replicates and reads back, but WHERE, JOIN, GROUP BY and ORDER BY on it \
                 are refused",
                raw.name
            ));
        }
        let auto_increment = raw.extra.to_ascii_lowercase().contains("auto_increment");
        let default_generated = raw.extra.to_ascii_lowercase().contains("default_generated");
        columns.push(SourceColumn {
            id: raw.ordinal,
            name: raw.name,
            mysql_data_type: raw.data_type,
            mysql_column_type: raw.column_type,
            pintail_type: mapping.data_type,
            nullable: raw.nullable,
            character_set: raw.character_set,
            collation: raw.collation,
            generated_stored,
            generation_expression: raw.generation_expression,
            extra: raw.extra,
            auto_increment,
            default_value: raw.default_value,
            default_generated,
        });
    }
    if columns.is_empty() {
        return Err(ProbeError::InvalidMetadata(format!(
            "table {table} has no materialized columns"
        )));
    }

    Ok(SourceTable {
        name: table,
        engine,
        estimated_rows,
        rows_are_exact,
        columns,
        key,
        unique_keys,
        requires_reconciliation: cascade_rules
            .iter()
            .any(|(_, delete_rule, update_rule, _, _)| {
                invisible_fk_rule(delete_rule) || invisible_fk_rule(update_rule)
            }),
        foreign_keys,
        warnings,
    })
}

fn derive_capabilities(
    variables: &BTreeMap<String, String>,
    grants: &[String],
    flavor: SourceFlavor,
) -> SourceCapabilities {
    let enabled = |name: &str| {
        variables
            .get(name)
            .is_some_and(|value| matches!(value.to_ascii_uppercase().as_str(), "ON" | "1"))
    };
    let equals = |name: &str, expected: &str| {
        variables
            .get(name)
            .is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    let normalized_grants = grants
        .iter()
        .map(|grant| grant.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let has_all = normalized_grants
        .iter()
        .any(|grant| grant.contains("ALL PRIVILEGES"));
    let has_grant =
        |name: &str| has_all || normalized_grants.iter().any(|grant| grant.contains(name));
    let replication_stream = has_grant("REPLICATION SLAVE") || has_grant("REPLICATION REPLICA");
    // MariaDB 10.5 renamed REPLICATION CLIENT to BINLOG MONITOR, and its
    // SHOW GRANTS reports the new name even when the old one was granted.
    let replication_position = has_grant("REPLICATION CLIENT") || has_grant("BINLOG MONITOR");
    let replication_grants = replication_stream && replication_position;
    let global_read_lock =
        has_all || (has_grant("RELOAD") && has_grant("LOCK TABLES")) || has_grant("FLUSH_TABLES");
    let log_bin = enabled("log_bin");
    let row_binlog = equals("binlog_format", "ROW");
    let full_row_image = equals("binlog_row_image", "FULL");
    let full_row_metadata = equals("binlog_row_metadata", "FULL")
        || (flavor == SourceFlavor::MariaDb && !variables.contains_key("binlog_row_metadata"));
    let gtid_available = enabled("gtid_mode")
        || enabled("gtid_strict_mode")
        || variables
            .get("gtid_mode")
            .is_some_and(|value| value.eq_ignore_ascii_case("ON_PERMISSIVE"));

    let mut reasons = Vec::new();
    if !log_bin {
        reasons.push("binary logging is disabled".to_owned());
    }
    if !row_binlog {
        reasons.push("binlog_format is not ROW".to_owned());
    }
    if !full_row_image {
        reasons.push("binlog_row_image is not FULL".to_owned());
    }
    if !full_row_metadata {
        // Informational, not disqualifying: the CDC decoder takes column
        // identity, signedness, enum/set labels, and charsets from the probed
        // schema, so MINIMAL row metadata decodes identically to FULL.
        reasons
            .push("binlog_row_metadata is MINIMAL; CDC decodes from the probed schema".to_owned());
    }
    if !replication_grants {
        reasons.push("replication stream/client grants are incomplete".to_owned());
    }
    if !global_read_lock {
        reasons.push(
            "global read-lock grants are unavailable; snapshot consistency will be degraded"
                .to_owned(),
        );
    }
    let cdc = log_bin && row_binlog && full_row_image && replication_grants;
    SourceCapabilities {
        log_bin,
        row_binlog,
        full_row_image,
        full_row_metadata,
        replication_grants,
        global_read_lock,
        gtid_available,
        recommended_mode: if cdc {
            RecommendedMode::Cdc
        } else {
            RecommendedMode::Polling
        },
        reasons,
    }
}

/// `(generated, stored)`: `MySQL` 8 reports `DEFAULT CURRENT_TIMESTAMP` columns
/// as `EXTRA='DEFAULT_GENERATED'`; those are ordinary stored columns and must
/// not be confused with `VIRTUAL GENERATED` / `STORED GENERATED` expressions,
/// which are the only ones absent from (virtual) or derivable in (stored)
/// row binlog images.
fn generated_flags(generation_expression: &str, extra: &str) -> (bool, bool) {
    let extra = extra.to_ascii_lowercase();
    let generated = !generation_expression.is_empty()
        || extra.contains("virtual generated")
        || extra.contains("stored generated");
    (generated, generated && extra.contains("stored generated"))
}

fn choose_key(columns: &[RawColumn], parts: &[RawIndexPart]) -> SourceKey {
    let mut indexes = BTreeMap::<&str, Vec<&RawIndexPart>>::new();
    for part in parts {
        indexes.entry(&part.name).or_default().push(part);
    }
    let mut index_names = indexes.keys().copied().collect::<Vec<_>>();
    index_names.sort_by_key(|name| (!name.eq_ignore_ascii_case("PRIMARY"), *name));
    for index_name in index_names {
        let index = &indexes[index_name];
        let primary = index_name.eq_ignore_ascii_case("PRIMARY");
        if !primary && index.iter().any(|part| part.non_unique) {
            continue;
        }
        if index.iter().any(|part| {
            part.prefix_length.is_some()
                || columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(&part.column))
                    .is_none_or(|column| {
                        column.nullable || column.extra.to_ascii_lowercase().contains("virtual")
                    })
        }) {
            continue;
        }
        let mut ordered = index.clone();
        ordered.sort_by_key(|part| part.sequence);
        return SourceKey {
            mode: if primary {
                KeyMode::Primary
            } else {
                KeyMode::Unique
            },
            index_name: Some(index_name.to_owned()),
            columns: ordered.iter().map(|part| part.column.clone()).collect(),
        };
    }
    SourceKey {
        mode: KeyMode::AppendRowId,
        index_name: None,
        columns: Vec::new(),
    }
}

fn usable_unique_keys(columns: &[RawColumn], parts: &[RawIndexPart]) -> Vec<Vec<String>> {
    let mut indexes = BTreeMap::<&str, Vec<&RawIndexPart>>::new();
    for part in parts {
        indexes.entry(&part.name).or_default().push(part);
    }
    indexes
        .into_values()
        .filter(|index| {
            index.iter().all(|part| {
                !part.non_unique
                    && part.prefix_length.is_none()
                    && columns
                        .iter()
                        .find(|column| column.name.eq_ignore_ascii_case(&part.column))
                        .is_some_and(|column| {
                            !column.nullable
                                && !column.extra.to_ascii_lowercase().contains("virtual")
                        })
            })
        })
        .map(|mut index| {
            index.sort_by_key(|part| part.sequence);
            index.iter().map(|part| part.column.clone()).collect()
        })
        .collect()
}

/// Collations the executor can compare.
///
/// Kept beside the probe rather than imported from the SQL crate so a source
/// can be assessed without the executor: the wizard runs this before anything
/// is snapshotted. It must be updated when the executor gains a collation.
fn is_supported_collation(collation: &str) -> bool {
    matches!(
        collation.to_ascii_lowercase().as_str(),
        "utf8mb4_0900_ai_ci" | "utf8mb4_general_ci" | "binary"
    )
}

fn invisible_fk_rule(rule: &str) -> bool {
    rule.eq_ignore_ascii_case("CASCADE") || rule.eq_ignore_ascii_case("SET NULL")
}

struct TypeMapping {
    data_type: DataType,
    warning: Option<String>,
}

#[allow(clippy::too_many_lines)]
fn map_mysql_type(column: &RawColumn) -> Result<TypeMapping, ProbeError> {
    let data_type = column.data_type.to_ascii_lowercase();
    let unsigned = column.column_type.to_ascii_lowercase().contains("unsigned");
    let integer = |signed, unsigned_type| if unsigned { unsigned_type } else { signed };
    let mapping = match data_type.as_str() {
        "tinyint"
            if column
                .column_type
                .to_ascii_lowercase()
                .starts_with("tinyint(1)") =>
        {
            TypeMapping {
                data_type: DataType::Boolean,
                warning: None,
            }
        }
        "tinyint" => TypeMapping {
            data_type: integer(DataType::Int8, DataType::UInt8),
            warning: None,
        },
        "smallint" => TypeMapping {
            data_type: integer(DataType::Int16, DataType::UInt16),
            warning: None,
        },
        "mediumint" | "int" | "integer" => TypeMapping {
            data_type: integer(DataType::Int32, DataType::UInt32),
            warning: None,
        },
        "bigint" => TypeMapping {
            data_type: integer(DataType::Int64, DataType::UInt64),
            warning: None,
        },
        "decimal" | "numeric" => {
            let precision = column.numeric_precision.ok_or_else(|| {
                ProbeError::InvalidMetadata(format!(
                    "decimal column {} is missing precision",
                    column.name
                ))
            })?;
            let scale = column.numeric_scale.unwrap_or(0);
            if precision <= 38 {
                TypeMapping {
                    data_type: DataType::Decimal { precision, scale },
                    warning: None,
                }
            } else {
                TypeMapping {
                    data_type: DataType::Utf8,
                    warning: Some(format!(
                        "DECIMAL({precision},{scale}) exceeds precision 38 and is stored as text"
                    )),
                }
            }
        }
        "float" => TypeMapping {
            data_type: DataType::Float32,
            warning: None,
        },
        "double" | "real" => TypeMapping {
            data_type: DataType::Float64,
            warning: None,
        },
        "bit" => TypeMapping {
            data_type: DataType::UInt64,
            warning: None,
        },
        "date" => TypeMapping {
            data_type: DataType::Date32,
            warning: None,
        },
        "datetime" | "timestamp" => TypeMapping {
            data_type: DataType::DateTime64 {
                fsp: column.datetime_precision.unwrap_or(0),
            },
            warning: None,
        },
        "time" => TypeMapping {
            data_type: DataType::Time64 {
                fsp: column.datetime_precision.unwrap_or(0),
            },
            warning: None,
        },
        "year" => TypeMapping {
            data_type: DataType::Year,
            warning: None,
        },
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            TypeMapping {
                data_type: DataType::Utf8,
                warning: None,
            }
        }
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "geometry"
        | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" => TypeMapping {
            data_type: DataType::Binary,
            warning: None,
        },
        "json" => TypeMapping {
            data_type: DataType::Json,
            warning: None,
        },
        other => TypeMapping {
            data_type: DataType::Utf8,
            warning: Some(format!(
                "unrecognized MySQL type {other} is stored as UTF-8 text"
            )),
        },
    };
    Ok(mapping)
}

#[cfg(test)]
mod tests {
    use super::{
        COUNT_BUDGET, COUNT_TOTAL_BUDGET, RawColumn, RawIndexPart, RecommendedMode, SourceColumn,
        SourceFlavor, SourceKey, SourceTable, choose_key, derive_capabilities, generated_flags,
        invisible_fk_rule, map_mysql_type, usable_unique_keys,
    };
    use pintail_types::{DataType, KeyMode};
    use std::collections::BTreeMap;

    fn column(name: &str, nullable: bool) -> RawColumn {
        RawColumn {
            ordinal: 1,
            name: name.to_owned(),
            nullable,
            data_type: "bigint".to_owned(),
            column_type: "bigint".to_owned(),
            numeric_precision: Some(19),
            numeric_scale: Some(0),
            datetime_precision: None,
            character_set: None,
            collation: None,
            extra: String::new(),
            generation_expression: String::new(),
            default_value: None,
        }
    }

    #[test]
    fn maps_exact_numeric_and_temporal_types() {
        let mut value = column("amount", false);
        value.data_type = "decimal".to_owned();
        value.column_type = "decimal(38,10)".to_owned();
        value.numeric_precision = Some(38);
        value.numeric_scale = Some(10);
        assert_eq!(
            map_mysql_type(&value).expect("mapping").data_type,
            DataType::Decimal {
                precision: 38,
                scale: 10
            }
        );

        value.data_type = "datetime".to_owned();
        value.datetime_precision = Some(6);
        assert_eq!(
            map_mysql_type(&value).expect("mapping").data_type,
            DataType::DateTime64 { fsp: 6 }
        );

        value.data_type = "year".to_owned();
        value.column_type = "year".to_owned();
        value.datetime_precision = None;
        assert_eq!(
            map_mysql_type(&value).expect("mapping").data_type,
            DataType::Year
        );
    }

    #[test]
    fn picks_primary_then_nonnullable_unique_then_append_mode() {
        let columns = vec![column("id", false), column("email", false)];
        let unique = RawIndexPart {
            name: "email_unique".to_owned(),
            non_unique: false,
            sequence: 1,
            column: "email".to_owned(),
            prefix_length: None,
        };
        assert_eq!(
            choose_key(&columns, std::slice::from_ref(&unique)).mode,
            KeyMode::Unique
        );

        let primary = RawIndexPart {
            name: "PRIMARY".to_owned(),
            column: "id".to_owned(),
            ..unique
        };
        assert_eq!(
            choose_key(&columns, std::slice::from_ref(&primary)).mode,
            KeyMode::Primary
        );
        let both = [unique.clone(), primary];
        assert_eq!(choose_key(&columns, &both).mode, KeyMode::Primary);

        let mut virtual_column = column("virtual_id", false);
        virtual_column.extra = "VIRTUAL GENERATED".to_owned();
        let virtual_unique = RawIndexPart {
            name: "virtual_unique".to_owned(),
            column: virtual_column.name.clone(),
            ..unique
        };
        assert_eq!(
            choose_key(&[virtual_column], &[virtual_unique]).mode,
            KeyMode::AppendRowId
        );
        assert_eq!(choose_key(&columns, &[]).mode, KeyMode::AppendRowId);
        assert_eq!(
            usable_unique_keys(&columns, &[unique]),
            vec![vec!["email".to_owned()]]
        );
        assert!(invisible_fk_rule("CASCADE"));
        assert!(invisible_fk_rule("set null"));
        assert!(!invisible_fk_rule("RESTRICT"));
    }

    #[test]
    fn default_generated_timestamps_are_not_virtual_columns() {
        // created_at DATETIME DEFAULT CURRENT_TIMESTAMP — replicated.
        assert_eq!(generated_flags("", "DEFAULT_GENERATED"), (false, false));
        assert_eq!(
            generated_flags("", "DEFAULT_GENERATED on update CURRENT_TIMESTAMP"),
            (false, false)
        );
        assert_eq!(generated_flags("", "auto_increment"), (false, false));
        // Real generated expressions keep their skip/keep behavior.
        assert_eq!(generated_flags("a + 1", "VIRTUAL GENERATED"), (true, false));
        assert_eq!(generated_flags("a + 1", "STORED GENERATED"), (true, true));
        assert_eq!(generated_flags("", "VIRTUAL GENERATED"), (true, false));
    }

    #[test]
    fn derives_cdc_only_from_complete_row_binlog_capabilities() {
        let variables = BTreeMap::from([
            ("log_bin".to_owned(), "ON".to_owned()),
            ("binlog_format".to_owned(), "ROW".to_owned()),
            ("binlog_row_image".to_owned(), "FULL".to_owned()),
            ("binlog_row_metadata".to_owned(), "FULL".to_owned()),
            ("gtid_mode".to_owned(), "ON".to_owned()),
        ]);
        let capabilities = derive_capabilities(
            &variables,
            &["GRANT REPLICATION SLAVE, REPLICATION CLIENT, RELOAD, LOCK TABLES ON *.* TO `p`@`%`"
                .to_owned()],
            SourceFlavor::Mysql,
        );
        assert_eq!(capabilities.recommended_mode, RecommendedMode::Cdc);
        assert!(capabilities.global_read_lock);
        assert!(capabilities.gtid_available);
    }

    #[test]
    fn internal_schema_requires_only_sort_key_components() {
        let table = SourceTable {
            name: "events".to_owned(),
            engine: Some("InnoDB".to_owned()),
            estimated_rows: None,
            rows_are_exact: false,
            columns: vec![
                SourceColumn {
                    id: 1,
                    name: "id".to_owned(),
                    mysql_data_type: "bigint".to_owned(),
                    mysql_column_type: "bigint unsigned".to_owned(),
                    pintail_type: DataType::UInt64,
                    nullable: false,
                    character_set: None,
                    collation: None,
                    generated_stored: false,
                    generation_expression: String::new(),
                    extra: String::new(),
                    auto_increment: true,
                    default_value: None,
                    default_generated: false,
                },
                SourceColumn {
                    id: 2,
                    name: "invalid_date".to_owned(),
                    mysql_data_type: "date".to_owned(),
                    mysql_column_type: "date".to_owned(),
                    pintail_type: DataType::Date32,
                    nullable: false,
                    character_set: None,
                    collation: None,
                    generated_stored: false,
                    generation_expression: String::new(),
                    extra: String::new(),
                    auto_increment: false,
                    default_value: None,
                    default_generated: false,
                },
            ],
            key: SourceKey {
                mode: KeyMode::Primary,
                index_name: Some("PRIMARY".to_owned()),
                columns: vec!["id".to_owned()],
            },
            unique_keys: vec![vec!["id".to_owned()]],
            requires_reconciliation: false,
            foreign_keys: Vec::new(),
            warnings: Vec::new(),
        };
        let schema = table.table_schema().expect("table schema");
        assert!(!schema.columns()[0].is_nullable());
        assert!(schema.columns()[1].is_nullable());
    }

    /// The server has to be the one that stops a runaway count.
    ///
    /// A client-side deadline alone abandons the caller but not the work: the
    /// scan runs to completion on a connection nobody is reading, holding a
    /// read view open. So the statement carries the bound with it, in the form
    /// each flavour actually honours.
    #[test]
    fn a_count_carries_a_server_side_deadline_for_each_flavor() {
        let budget_ms = COUNT_BUDGET.as_millis();
        assert_eq!(budget_ms, 30_000, "the documented budget is 30 seconds");

        let mysql = format!("SELECT /*+ MAX_EXECUTION_TIME({budget_ms}) */ COUNT(*) FROM `db`.`t`");
        assert!(
            mysql.contains("MAX_EXECUTION_TIME(30000)"),
            "MySQL takes the bound as an optimizer hint in milliseconds"
        );

        let maria = format!(
            "SET STATEMENT max_statement_time={} FOR SELECT COUNT(*) FROM `db`.`t`",
            COUNT_BUDGET.as_secs()
        );
        assert!(
            maria.starts_with("SET STATEMENT max_statement_time=30 FOR"),
            "MariaDB has no such hint and takes seconds through SET STATEMENT"
        );
    }

    /// Backticks in an identifier must not end the quoting, or a table named
    /// with one would change what is counted.
    #[test]
    fn count_targets_quote_identifiers_that_contain_backticks() {
        let database = "d`b";
        let table = "t`bl";
        let target = format!(
            "`{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        assert_eq!(target, "`d``b`.`t``bl`");
    }

    /// The per-table budget is not a bound on the wait; the total one is.
    #[test]
    fn the_whole_probe_stops_counting_before_it_stops_being_usable() {
        let worst_case_per_table = COUNT_BUDGET.as_secs();
        assert!(
            COUNT_TOTAL_BUDGET.as_secs() / worst_case_per_table <= 12,
            "a schema wide enough to spend the total budget must not be able to \
             hold the connection screen for more than a few minutes"
        );
        assert!(
            COUNT_TOTAL_BUDGET > COUNT_BUDGET,
            "the total budget must allow at least one table to be counted"
        );
    }
}
