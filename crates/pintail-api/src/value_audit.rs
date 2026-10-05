//! Replicated tables that may hold values change capture stored wrong
//! before the decoder fixes of copy generation 1
//! ([`pintail_meta::COPY_GENERATION`]).
//!
//! Two decoding defects stored a streamed value wrong while the copy stored
//! the same value right: a negative signed `MEDIUMINT` arrived 2^24 too
//! high, and a `BINARY(n)` value lost its trailing zero bytes. Only rows a
//! stream inserted or updated after the copy are affected, so a table is a
//! candidate when all of these hold:
//!
//! - its schema - the one Pintail stores and the decoder reads, which is
//!   where a signed `MEDIUMINT` gets its sign under minimal row metadata -
//!   has a column of either type;
//! - its copy was not completed by a binary with the fixes
//!   (`copy_generation` below 1);
//! - it replicates by change capture, not polling, which reads values as
//!   text;
//! - its files show a change applied since its copy
//!   ([`pintail_store::changes_applied_at_rest`]).
//!
//! The remedy is a resync of the table; nothing here starts one. A resync
//! records the current generation, which is what clears the advice.

use std::path::Path;

use pintail_meta::{DatabaseRecord, MetaStore, TableRecord};
use pintail_probe::{ProbeReport, SourceColumn};
use serde::Serialize;

/// The release whose binary first records a copy's generation.
const FIXED_IN: &str = "0.1.7-rc3";

/// One decoding defect a column's declared type exposed it to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValueDefect {
    /// A negative signed `MEDIUMINT` stored 2^24 (16777216) too high.
    NegativeMediumint,
    /// A `BINARY(n)` value stored without its trailing zero bytes.
    BinaryTrailingZeros,
}

impl ValueDefect {
    fn of(column: &SourceColumn) -> Option<Self> {
        if column.mysql_data_type.eq_ignore_ascii_case("mediumint") {
            let unsigned = column
                .mysql_column_type
                .to_ascii_lowercase()
                .contains("unsigned");
            return (!unsigned).then_some(Self::NegativeMediumint);
        }
        column
            .mysql_data_type
            .eq_ignore_ascii_case("binary")
            .then_some(Self::BinaryTrailingZeros)
    }

    const fn declared(self) -> &'static str {
        match self {
            Self::NegativeMediumint => "signed MEDIUMINT",
            Self::BinaryTrailingZeros => "BINARY(n)",
        }
    }

    const fn effect(self) -> &'static str {
        match self {
            Self::NegativeMediumint => "a negative signed MEDIUMINT was stored 16777216 too high",
            Self::BinaryTrailingZeros => "a BINARY(n) value lost its trailing zero bytes",
        }
    }
}

/// A column whose streamed values may be wrong.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AffectedColumn {
    pub(crate) name: String,
    pub(crate) defect: ValueDefect,
}

/// A table to resync, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SuspectTable {
    pub(crate) database_id: String,
    pub(crate) database_name: String,
    pub(crate) table: String,
    pub(crate) columns: Vec<AffectedColumn>,
}

/// What the table status reports for a suspect table.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResyncAdvice {
    pub(crate) reason: &'static str,
    pub(crate) columns: Vec<AffectedColumn>,
}

impl SuspectTable {
    pub(crate) fn advice(&self) -> ResyncAdvice {
        ResyncAdvice {
            reason: "stream_values_before_decoder_fixes",
            columns: self.columns.clone(),
        }
    }

    /// The warning logged for this table at startup. Names, never values.
    pub(crate) fn warning(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| format!("{} ({})", column.name, column.defect.declared()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut effects = Vec::new();
        for column in &self.columns {
            if !effects.contains(&column.defect.effect()) {
                effects.push(column.defect.effect());
            }
        }
        format!(
            "replication.resync_advised database={} ({}) table={} columns={columns}: \
             rows inserted or updated through change capture by a binary older than \
             {FIXED_IN} may be stored wrong ({}); rows copied by a snapshot are right. \
             Resync this table to repair it: its Resync action in the dashboard, or \
             POST /api/databases/{}/tables/{}/resync",
            self.database_name,
            self.database_id,
            self.table,
            effects.join("; "),
            self.database_id,
            self.table,
        )
    }
}

/// The summary logged after the per-table warnings, when there are any.
pub(crate) fn summary(count: usize) -> String {
    format!(
        "replication.resync_advised {count} replicated table(s) may hold values change \
         capture stored wrong before {FIXED_IN}, each named above; resync them to repair. \
         A table warns until it is copied again, including one copied by {FIXED_IN} itself, \
         which did not record the binary that copied it"
    )
}

/// Every suspect table of `database`, in name order.
///
/// # Errors
///
/// Returns an error when the metadata cannot be read.
pub(crate) fn suspect_tables(
    data_dir: &Path,
    metadata: &MetaStore,
    database: &DatabaseRecord,
) -> anyhow::Result<Vec<SuspectTable>> {
    // A local database is written through the wire, never streamed; a
    // polling one reads values as text.
    let mode = database
        .effective_mode
        .as_deref()
        .unwrap_or(database.mode.as_str());
    if database.kind != "replicated" || mode == "polling" {
        return Ok(Vec::new());
    }
    let mut probe: Option<Option<ProbeReport>> = None;
    let root = data_dir.join("databases").join(&database.id).join("tables");
    let mut suspects = Vec::new();
    for table in metadata.tables(&database.id)? {
        if !may_hold_stream_values(&table) {
            continue;
        }
        let columns = match metadata.schema_history(&database.id, &table.name)?.last() {
            // The status page asks this on every refresh: a schema that
            // names neither type anywhere is not decoded at all.
            Some(record) if !mentions_affected_type(&record.columns_json) => continue,
            Some(record) => serde_json::from_str::<Vec<SourceColumn>>(&record.columns_json).ok(),
            None => probe
                .get_or_insert_with(|| {
                    database
                        .probe_json
                        .as_deref()
                        .and_then(|json| serde_json::from_str(json).ok())
                })
                .as_ref()
                .and_then(|report| {
                    report
                        .tables
                        .iter()
                        .find(|source| source.name.eq_ignore_ascii_case(&table.name))
                        .map(|source| source.columns.clone())
                }),
        };
        let affected = columns
            .unwrap_or_default()
            .into_iter()
            .filter_map(|column| {
                ValueDefect::of(&column).map(|defect| AffectedColumn {
                    name: column.name,
                    defect,
                })
            })
            .collect::<Vec<_>>();
        if affected.is_empty() {
            continue;
        }
        // A file that cannot be read cannot clear the table.
        let directory = crate::snapshot::table_directory(&root, &table.name);
        if !pintail_store::changes_applied_at_rest(&directory).unwrap_or(true) {
            continue;
        }
        suspects.push(SuspectTable {
            database_id: database.id.clone(),
            database_name: database.name.clone(),
            table: table.name,
            columns: affected,
        });
    }
    Ok(suspects)
}

fn mentions_affected_type(columns_json: &str) -> bool {
    let lower = columns_json.to_ascii_lowercase();
    lower.contains("mediumint") || lower.contains("binary")
}

/// Whether a table's rows may include ones a stream decoded before the
/// fixes, as far as its control-plane record can say.
fn may_hold_stream_values(table: &TableRecord) -> bool {
    table.copy_generation < pintail_meta::COPY_GENERATION
        // An owed copy replaces every row before the table answers again.
        && !table.copy_pending
        // Excluded tables are not replicated; polled ones read text. A
        // table whose source dropped it keeps its rows but cannot be
        // resynced.
        && !matches!(table.state.as_str(), "excluded" | "polling")
        && table.orphaned_at.is_none()
}

/// Logs one warning per suspect table across every database, then a
/// summary when there was any. Returns how many tables it named.
pub(crate) fn warn_at_startup(data_dir: &Path, metadata: &MetaStore) -> usize {
    let databases = match metadata.databases() {
        Ok(databases) => databases,
        Err(error) => {
            pintail_log::log_error!(
                "replication.resync_advised could not list databases: {error:#}"
            );
            return 0;
        }
    };
    let mut count = 0;
    for database in databases {
        match suspect_tables(data_dir, metadata, &database) {
            Ok(suspects) => {
                for suspect in &suspects {
                    pintail_log::log_warn!("{}", suspect.warning());
                }
                count += suspects.len();
            }
            Err(error) => pintail_log::log_error!(
                "replication.resync_advised could not check database {}: {error:#}",
                database.id
            ),
        }
    }
    if count > 0 {
        pintail_log::log_warn!("{}", summary(count));
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use pintail_store::{StoreOptions, TableStore};
    use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

    const DB: &str = "db-1";

    fn column(id: u32, name: &str, data_type: &str, column_type: &str) -> SourceColumn {
        SourceColumn {
            id,
            name: name.to_owned(),
            mysql_data_type: data_type.to_owned(),
            mysql_column_type: column_type.to_owned(),
            pintail_type: DataType::Int64,
            nullable: true,
            character_set: None,
            collation: None,
            generated_stored: false,
            generation_expression: String::new(),
            generation_captured: true,
            extra: String::new(),
            auto_increment: false,
            default_value: None,
            default_generated: false,
            absent_fill: None,
            ordinal: id,
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        metadata: MetaStore,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let metadata = MetaStore::open(&directory.path().join("pintail-meta.db")).unwrap();
            metadata
                .upsert_database(DB, "inventory", b"secret", "2026-10-03T00:00:00Z")
                .unwrap();
            Self {
                directory,
                metadata,
            }
        }

        fn store_directory(&self, table: &str) -> std::path::PathBuf {
            crate::snapshot::table_directory(
                &self
                    .directory
                    .path()
                    .join("databases")
                    .join(DB)
                    .join("tables"),
                table,
            )
        }

        /// Copies `table` the way the current binary does: a version-zero
        /// copy, then the copy marker and this binary's generation.
        fn copy(&self, table: &str, columns: &[SourceColumn]) {
            self.metadata
                .upsert_snapshot_table(DB, table, Some("[\"id\"]"), Some("[\"id\"]"))
                .unwrap();
            self.metadata
                .record_first_schema_generation(
                    DB,
                    table,
                    &serde_json::to_string(columns).unwrap(),
                    "2026-10-03T00:00:00Z",
                )
                .unwrap();
            let mut store = self.open(table);
            store.reset_for_resnapshot().unwrap();
            store.bulk_ingest_snapshot(vec![row(1, 0)]).unwrap();
            self.metadata.complete_snapshot_table(DB, table).unwrap();
        }

        /// What a binary older than the generation record left behind.
        fn forget_generation(&self, table: &str) {
            let connection =
                rusqlite::Connection::open(self.directory.path().join("pintail-meta.db")).unwrap();
            connection
                .execute(
                    "UPDATE tables SET copy_generation = 0 WHERE db_id = ?1 AND name = ?2",
                    (DB, table),
                )
                .unwrap();
        }

        fn open(&self, table: &str) -> TableStore {
            let schema =
                TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap();
            TableStore::open(self.store_directory(table), schema, StoreOptions::default()).unwrap()
        }

        /// One change applied by the stream after the copy.
        fn stream_change(&self, table: &str) {
            let mut store = self.open(table);
            store.ingest_cdc_in_order(vec![row(2, 5)]).unwrap();
            store.flush().unwrap();
        }

        fn suspects(&self) -> Vec<SuspectTable> {
            let database = self.metadata.database(DB).unwrap().unwrap();
            suspect_tables(self.directory.path(), &self.metadata, &database).unwrap()
        }
    }

    fn row(id: u64, version: u64) -> StoredRow {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
            vec![Value::UInt64(id)],
            version,
            false,
        )
    }

    fn affected_columns() -> Vec<SourceColumn> {
        vec![
            column(1, "id", "int", "int"),
            column(2, "delta", "mediumint", "mediumint"),
            column(3, "stock", "mediumint", "mediumint unsigned"),
            column(4, "token", "binary", "binary(16)"),
            column(5, "label", "varbinary", "varbinary(16)"),
        ]
    }

    #[test]
    fn an_old_copy_with_streamed_changes_is_advised_until_it_is_resynced() {
        let fixture = Fixture::new();
        fixture.copy("ledger", &affected_columns());
        fixture.forget_generation("ledger");
        fixture.stream_change("ledger");

        let suspects = fixture.suspects();
        assert_eq!(suspects.len(), 1);
        assert_eq!(suspects[0].table, "ledger");
        // Only the signed MEDIUMINT and the BINARY(n): the unsigned one
        // and VARBINARY were decoded right all along.
        assert_eq!(
            suspects[0].columns,
            vec![
                AffectedColumn {
                    name: "delta".to_owned(),
                    defect: ValueDefect::NegativeMediumint,
                },
                AffectedColumn {
                    name: "token".to_owned(),
                    defect: ValueDefect::BinaryTrailingZeros,
                },
            ]
        );
        let warning = suspects[0].warning();
        assert!(warning.contains("database=inventory (db-1) table=ledger"));
        assert!(warning.contains("columns=delta (signed MEDIUMINT), token (BINARY(n)):"));
        assert!(warning.contains("16777216 too high; a BINARY(n) value lost its trailing zero"));
        assert!(warning.contains("POST /api/databases/db-1/tables/ledger/resync"));
        assert_eq!(
            warn_at_startup(fixture.directory.path(), &fixture.metadata),
            1
        );

        // The resync: begin, recopy, finish.
        fixture
            .metadata
            .begin_table_resnapshot(DB, "ledger")
            .unwrap();
        let mut store = fixture.open("ledger");
        store.reset_for_resnapshot().unwrap();
        store.bulk_ingest_snapshot(vec![row(1, 0)]).unwrap();
        drop(store);
        fixture
            .metadata
            .finish_table_resnapshot(DB, "ledger", "streaming")
            .unwrap();
        // Changes applied after it were decoded by this binary.
        fixture.stream_change("ledger");
        assert!(fixture.suspects().is_empty());
        assert_eq!(
            warn_at_startup(fixture.directory.path(), &fixture.metadata),
            0
        );
    }

    #[test]
    fn a_table_without_the_column_types_is_not_advised() {
        let fixture = Fixture::new();
        fixture.copy(
            "plain",
            &[
                column(1, "id", "int", "int"),
                column(2, "stock", "mediumint", "mediumint unsigned"),
                column(3, "label", "varbinary", "varbinary(16)"),
            ],
        );
        fixture.forget_generation("plain");
        fixture.stream_change("plain");
        assert!(fixture.suspects().is_empty());
    }

    #[test]
    fn a_fresh_copy_by_this_binary_is_not_advised() {
        let fixture = Fixture::new();
        fixture.copy("ledger", &affected_columns());
        fixture.stream_change("ledger");
        assert!(fixture.suspects().is_empty());
    }

    #[test]
    fn an_old_copy_that_never_applied_a_change_is_not_advised() {
        let fixture = Fixture::new();
        fixture.copy("ledger", &affected_columns());
        fixture.forget_generation("ledger");
        assert!(fixture.suspects().is_empty());
        // The first applied change makes it one.
        fixture.stream_change("ledger");
        assert_eq!(fixture.suspects().len(), 1);
    }

    #[test]
    fn an_old_copy_that_replicates_by_polling_is_not_advised() {
        let fixture = Fixture::new();
        fixture.copy("ledger", &affected_columns());
        fixture.forget_generation("ledger");
        fixture.stream_change("ledger");
        fixture
            .metadata
            .update_database_probe(DB, "{}", "polling", "2026-10-03T00:00:00Z")
            .unwrap();
        assert!(fixture.suspects().is_empty());
    }
}
