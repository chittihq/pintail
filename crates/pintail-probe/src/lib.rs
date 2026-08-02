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
    /// Approximate source row count.
    pub estimated_rows: Option<u64>,
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
    /// Table-specific mapping warnings.
    pub warnings: Vec<String>,
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
    /// Whether the source declares this column `AUTO_INCREMENT`.
    pub auto_increment: bool,
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
}

#[derive(Clone, Debug)]
struct RawIndexPart {
    name: String,
    non_unique: bool,
    sequence: u32,
    column: String,
    prefix_length: Option<u64>,
}

/// Probes one database through a real `mysql_async` connection.
///
/// # Errors
///
/// Returns a protocol error or rejects inconsistent source metadata.
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

    let raw_tables: Vec<(String, Option<String>, Option<u64>)> = connection
        .exec(
            "SELECT TABLE_NAME, ENGINE, TABLE_ROWS \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
             ORDER BY TABLE_NAME",
            (database,),
        )
        .await?;
    let mut tables = Vec::with_capacity(raw_tables.len());
    let mut warnings = Vec::new();
    for (name, engine, estimated_rows) in raw_tables {
        let table = probe_table(&mut connection, database, name, engine, estimated_rows).await?;
        warnings.extend(
            table
                .warnings
                .iter()
                .map(|warning| format!("{}: {warning}", table.name)),
        );
        tables.push(table);
    }

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
) -> Result<SourceTable, ProbeError> {
    type ColumnRow = (
        String,
        u32,
        String,
        String,
        String,
        Option<u8>,
        Option<u8>,
        Option<u8>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
    );
    type IndexRow = (String, u8, u32, Option<String>, Option<u64>);
    let column_rows: Vec<ColumnRow> = connection
        .exec(
            "SELECT COLUMN_NAME, ORDINAL_POSITION, IS_NULLABLE, DATA_TYPE, COLUMN_TYPE, \
                    NUMERIC_PRECISION, NUMERIC_SCALE, DATETIME_PRECISION, \
                    CHARACTER_SET_NAME, COLLATION_NAME, EXTRA, GENERATION_EXPRESSION \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            (database, &table),
        )
        .await?;
    let raw_columns = column_rows
        .into_iter()
        .map(
            |(
                name,
                ordinal,
                nullable,
                data_type,
                column_type,
                numeric_precision,
                numeric_scale,
                datetime_precision,
                character_set,
                collation,
                extra,
                generation_expression,
            )| RawColumn {
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
            },
        )
        .collect::<Vec<_>>();
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
    let cascade_rules: Vec<(String, String, String)> = connection
        .exec(
            "SELECT CONSTRAINT_NAME, DELETE_RULE, UPDATE_RULE \
             FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY CONSTRAINT_NAME",
            (database, &table),
        )
        .await?;

    let mut columns = Vec::with_capacity(raw_columns.len());
    let mut warnings = Vec::new();
    for (constraint, delete_rule, update_rule) in &cascade_rules {
        if invisible_fk_rule(delete_rule) || invisible_fk_rule(update_rule) {
            warnings.push(format!(
                "foreign key {constraint} uses DELETE {delete_rule}/UPDATE {update_rule}; \
                 scheduling primary-key reconciliation because cascades are absent from row binlogs"
            ));
        }
    }
    for raw in raw_columns {
        let (generated, generated_stored) =
            generated_flags(&raw.generation_expression, &raw.extra);
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
        let auto_increment = raw.extra.to_ascii_lowercase().contains("auto_increment");
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
            auto_increment,
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
        columns,
        key,
        unique_keys,
        requires_reconciliation: cascade_rules.iter().any(|(_, delete_rule, update_rule)| {
            invisible_fk_rule(delete_rule) || invisible_fk_rule(update_rule)
        }),
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
        reasons.push(
            "binlog_row_metadata is MINIMAL; CDC decodes from the probed schema".to_owned(),
        );
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

/// (generated, stored): MySQL 8 reports `DEFAULT CURRENT_TIMESTAMP` columns
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
            data_type: DataType::UInt16,
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
        RawColumn, RawIndexPart, RecommendedMode, SourceColumn, SourceFlavor, SourceKey,
        SourceTable, choose_key, derive_capabilities, generated_flags, invisible_fk_rule,
        map_mysql_type, usable_unique_keys,
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
                    auto_increment: true,
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
                    auto_increment: false,
                },
            ],
            key: SourceKey {
                mode: KeyMode::Primary,
                index_name: Some("PRIMARY".to_owned()),
                columns: vec!["id".to_owned()],
            },
            unique_keys: vec![vec!["id".to_owned()]],
            requires_reconciliation: false,
            warnings: Vec::new(),
        };
        let schema = table.table_schema().expect("table schema");
        assert!(!schema.columns()[0].is_nullable());
        assert!(schema.columns()[1].is_nullable());
    }
}
