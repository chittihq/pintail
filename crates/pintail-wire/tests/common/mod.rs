//! A replicated database for engine tests: two probed tables, `a` and `b`,
//! each an unsigned key and a text body, whose copies are complete and
//! whose stores the test writes the way replication does.

// Each test binary uses its own part of the fixture.
#![allow(dead_code)]

use std::collections::BTreeMap;

use pintail_meta::MetaStore;
use pintail_probe::{
    ProbeReport, RecommendedMode, ServerIdentity, SourceCapabilities, SourceColumn, SourceFlavor,
    SourceKey, SourceTable,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};
use pintail_wire::ReplicaEngine;

pub const DATABASE: &str = "db-replicated";

fn column(id: u32, name: &str, data_type: DataType) -> SourceColumn {
    let (mysql_data_type, mysql_column_type) = match data_type {
        DataType::UInt64 => ("bigint", "bigint unsigned"),
        DataType::Int64 => ("bigint", "bigint"),
        _ => ("varchar", "varchar(64)"),
    };
    SourceColumn {
        id,
        name: name.to_owned(),
        mysql_data_type: mysql_data_type.to_owned(),
        mysql_column_type: mysql_column_type.to_owned(),
        pintail_type: data_type,
        nullable: false,
        character_set: None,
        collation: None,
        generated_stored: false,
        generation_expression: String::new(),
        extra: String::new(),
        auto_increment: false,
        default_value: None,
        default_generated: false,
        ordinal: 0,
    }
}

/// `name` with an unsigned key and a column `body` of `body_type`.
pub fn source_table_with(name: &str, body_type: DataType) -> SourceTable {
    SourceTable {
        name: name.to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(0),
        rows_are_exact: false,
        columns: vec![
            column(1, "id", DataType::UInt64),
            column(2, "body", body_type),
        ],
        key: SourceKey {
            mode: KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: vec!["id".to_owned()],
        },
        unique_keys: Vec::new(),
        requires_reconciliation: false,
        foreign_keys: Vec::new(),
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
        source_column_count: 0,
    }
}

/// `name` as the fixture creates it, with a text body.
pub fn source_table(name: &str) -> SourceTable {
    source_table_with(name, DataType::Utf8)
}

fn probe_report(tables: Vec<SourceTable>) -> ProbeReport {
    ProbeReport {
        database: "inventory".to_owned(),
        server: ServerIdentity {
            version: "8.4.0".to_owned(),
            version_comment: "MySQL Community Server".to_owned(),
            flavor: SourceFlavor::Mysql,
            time_zone: None,
        },
        variables: BTreeMap::new(),
        grants: Vec::new(),
        capabilities: SourceCapabilities {
            log_bin: true,
            row_binlog: true,
            full_row_image: true,
            full_row_metadata: false,
            replication_grants: true,
            global_read_lock: true,
            gtid_available: false,
            recommended_mode: RecommendedMode::Cdc,
            reasons: Vec::new(),
        },
        tables,
        warnings: Vec::new(),
    }
}

/// One stored row of either table.
pub fn row(id: u64, body: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![Value::UInt64(id), Value::Utf8(body.to_owned())],
        version,
        deleted,
    )
}

/// Small, inline-compacting stores, so a test can make every kind of
/// change on demand.
pub fn options() -> StoreOptions {
    StoreOptions {
        compaction_fan_in: 2,
        compaction_disk_reserve_bytes: 0,
        background_compaction: false,
        ..StoreOptions::default()
    }
}

pub struct Replica {
    _directory: tempfile::TempDir,
    pub data_dir: std::path::PathBuf,
    pub metadata_path: std::path::PathBuf,
}

impl Replica {
    /// Both tables probed, copied and empty.
    pub fn seed() -> Self {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let data_dir = directory.path().to_path_buf();
        let metadata_path = data_dir.join("pintail-meta.db");
        let metadata = MetaStore::open(&metadata_path).expect("metadata");
        metadata
            .upsert_database(DATABASE, "inventory", b"unused", "2026-09-11T00:00:00Z")
            .expect("database");
        drop(metadata);
        let replica = Self {
            _directory: directory,
            data_dir,
            metadata_path,
        };
        replica.probe(vec![source_table("a"), source_table("b")]);
        let mut metadata = MetaStore::open(&replica.metadata_path).expect("metadata");
        for table in ["a", "b"] {
            metadata
                .upsert_snapshot_table(DATABASE, table, Some(r#"["id"]"#), Some(r#"["id"]"#))
                .expect("table");
            metadata
                .start_snapshot_chunk(DATABASE, table, "all", None, None)
                .expect("chunk");
            metadata
                .complete_snapshot_chunk(DATABASE, table, "all", 0)
                .expect("chunk complete");
            metadata
                .complete_snapshot_table(DATABASE, table)
                .expect("copy complete");
        }
        metadata
            .set_database_replication_state(DATABASE, "cdc", "2026-09-11T00:00:02Z")
            .expect("state");
        // Both copies are complete, so both stores exist before any query.
        for table in ["a", "b"] {
            drop(replica.writer(table));
        }
        replica
    }

    /// Records a probe of the source that found `tables`.
    pub fn probe(&self, tables: Vec<SourceTable>) {
        let report = probe_report(tables);
        MetaStore::open(&self.metadata_path)
            .expect("metadata")
            .update_database_probe(
                DATABASE,
                &serde_json::to_string(&report).expect("report json"),
                "cdc",
                "2026-09-11T00:00:01Z",
            )
            .expect("probe");
    }

    /// Opens `table`'s writer and holds it, as replication does.
    pub fn writer(&self, table: &str) -> TableStore {
        let root = self
            .data_dir
            .join("databases")
            .join(DATABASE)
            .join("tables");
        TableStore::open(
            pintail_wire::table_directory(&root, table),
            source_table(table).table_schema().expect("schema"),
            options(),
        )
        .expect("open writer")
    }

    pub fn engine(&self) -> ReplicaEngine {
        ReplicaEngine::new(&self.data_dir, &self.metadata_path)
    }
}

/// `SELECT COUNT(*)` of `table`, or the engine's error.
pub fn count(engine: &ReplicaEngine, table: &str) -> Result<u64, String> {
    let output = engine
        .execute(DATABASE, &format!("SELECT COUNT(*) FROM {table}"), 10)
        .map_err(|error| error.to_string())?;
    match output.rows[0][0] {
        Value::UInt64(count) => Ok(count),
        ref other => panic!("COUNT(*) is not an unsigned integer: {other:?}"),
    }
}

/// Every body of `table` in key order.
pub fn bodies(engine: &ReplicaEngine, table: &str) -> Vec<String> {
    engine
        .execute(
            DATABASE,
            &format!("SELECT body FROM {table} ORDER BY id"),
            100,
        )
        .expect("bodies")
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            Value::Utf8(body) => body.clone(),
            other => panic!("body is not text: {other:?}"),
        })
        .collect()
}
