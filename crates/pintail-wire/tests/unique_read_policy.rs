//! The secondary-UNIQUE read policy hides the lower-versioned side of a
//! unique-value collision, and finding one needs every projected row in
//! memory. So it applies only where a collision can exist - polling, and
//! CDC tables flagged for reconciliation - and a native CDC table streams:
//! the policy once made a one-row COUNT over a large mirrored table fail
//! with the query memory limit, on a table whose answer needs almost nothing.

use std::collections::BTreeMap;

use pintail_meta::MetaStore;
use pintail_probe::{
    ProbeReport, RecommendedMode, ServerIdentity, SourceCapabilities, SourceColumn, SourceFlavor,
    SourceKey, SourceTable,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{DataType, KeyMode, KeyPart, PrimaryKey, StoredRow, Value};
use pintail_wire::ReplicaEngine;

const DATABASE: &str = "db-1";
/// Enough rows that the projection the policy materializes is several
/// times the ceiling below, while one streamed batch fits it easily.
const ROWS: u64 = 60_000;
const CEILING: usize = 4 * 1024 * 1024;

fn column(id: u32, name: &str, data_type: DataType) -> SourceColumn {
    let (mysql_data_type, mysql_column_type) = match data_type {
        DataType::UInt64 => ("bigint", "bigint unsigned"),
        _ => ("varchar", "varchar(255)"),
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
        auto_increment: id == 1,
        default_value: None,
        default_generated: false,
    }
}

fn source_table(requires_reconciliation: bool) -> SourceTable {
    SourceTable {
        name: "reports".to_owned(),
        engine: Some("InnoDB".to_owned()),
        estimated_rows: Some(ROWS),
        rows_are_exact: false,
        columns: vec![
            column(1, "id", DataType::UInt64),
            column(2, "message_id", DataType::Utf8),
            column(3, "template", DataType::Utf8),
        ],
        key: SourceKey {
            mode: KeyMode::Primary,
            index_name: Some("PRIMARY".to_owned()),
            columns: vec!["id".to_owned()],
        },
        unique_keys: vec![vec!["message_id".to_owned()]],
        requires_reconciliation,
        foreign_keys: Vec::new(),
        secondary_indexes: Vec::new(),
        warnings: Vec::new(),
    }
}

fn probe_report(table: SourceTable, mode: RecommendedMode) -> ProbeReport {
    ProbeReport {
        database: "analytics".to_owned(),
        server: ServerIdentity {
            version: "8.4.0".to_owned(),
            version_comment: "MySQL Community Server".to_owned(),
            flavor: SourceFlavor::Mysql,
        },
        variables: BTreeMap::new(),
        grants: Vec::new(),
        capabilities: SourceCapabilities {
            log_bin: mode == RecommendedMode::Cdc,
            row_binlog: mode == RecommendedMode::Cdc,
            full_row_image: mode == RecommendedMode::Cdc,
            full_row_metadata: false,
            replication_grants: mode == RecommendedMode::Cdc,
            global_read_lock: true,
            gtid_available: false,
            recommended_mode: mode,
            reasons: Vec::new(),
        },
        tables: vec![table],
        warnings: Vec::new(),
    }
}

/// Row `id` carries message `m<id>`; the collision rows share `m1` with
/// row 1 under a higher version, the way a polled source that deleted row 1
/// and reused its message id looks before reconciliation.
fn row(id: u64, message: &str, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(message.to_owned()),
            Value::Utf8(format!("template_{:02}", id % 40)),
        ],
        version,
        false,
    )
}

struct Replica {
    _directory: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    metadata_path: std::path::PathBuf,
}

impl Replica {
    fn engine(&self, memory_limit: Option<usize>) -> ReplicaEngine {
        let engine = ReplicaEngine::new(&self.data_dir, &self.metadata_path);
        match memory_limit {
            Some(limit) => engine.with_memory_limit(limit),
            None => engine,
        }
    }
}

fn seed(mode: &str, requires_reconciliation: bool, collision: bool) -> Replica {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let source = source_table(requires_reconciliation);
    let recommended = if mode == "cdc" {
        RecommendedMode::Cdc
    } else {
        RecommendedMode::Polling
    };
    let report = probe_report(source.clone(), recommended);
    let mut metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .upsert_database(DATABASE, "analytics", b"unused", "2026-09-05T00:00:00Z")
        .expect("database");
    metadata
        .update_database_probe(
            DATABASE,
            &serde_json::to_string(&report).expect("report json"),
            mode,
            "2026-09-05T00:00:01Z",
        )
        .expect("probe");
    metadata
        .upsert_snapshot_table(DATABASE, "reports", Some(r#"["id"]"#), Some(r#"["id"]"#))
        .expect("table");
    metadata
        .start_snapshot_chunk(DATABASE, "reports", "all", None, None)
        .expect("chunk");
    metadata
        .complete_snapshot_chunk(DATABASE, "reports", "all", ROWS)
        .expect("chunk complete");
    metadata
        .set_database_replication_state(DATABASE, mode, "2026-09-05T00:00:02Z")
        .expect("state");
    drop(metadata);

    let root = data_dir.join("databases").join(DATABASE).join("tables");
    let mut store = TableStore::open(
        pintail_wire::table_directory(&root, "reports"),
        source.table_schema().expect("schema"),
        StoreOptions::default(),
    )
    .expect("open table");
    store
        .ingest((1..=ROWS).map(|id| row(id, &format!("m{id}"), 1)).collect())
        .expect("ingest");
    if collision {
        store
            .ingest(vec![row(ROWS + 1, "m1", 2)])
            .expect("ingest collision");
    }
    // Segments only: a memtable-resident table under 64K rows resolves its
    // visibility by materializing anyway, which is not the path under test.
    store.flush().expect("flush");
    drop(store);
    Replica {
        _directory: directory,
        data_dir,
        metadata_path,
    }
}

fn scalar(engine: &ReplicaEngine, sql: &str) -> Result<Value, String> {
    engine
        .execute(DATABASE, sql, 10)
        .map(|output| output.rows[0][0].clone())
        .map_err(|error| error.to_string())
}

#[test]
fn a_cdc_table_with_a_secondary_unique_key_streams_under_the_ceiling() {
    let replica = seed("cdc", false, false);
    let engine = replica.engine(Some(CEILING));
    assert_eq!(
        scalar(&engine, "SELECT COUNT(DISTINCT template) FROM reports"),
        Ok(Value::UInt64(40))
    );
    assert_eq!(
        scalar(
            &engine,
            "SELECT COUNT(*) FROM reports WHERE template = 'template_07'"
        ),
        Ok(Value::UInt64(1500))
    );
}

#[test]
fn a_polling_table_still_hides_the_older_side_of_a_collision() {
    let replica = seed("polling", false, true);
    let roomy = replica.engine(None);
    assert_eq!(
        scalar(
            &roomy,
            "SELECT COUNT(*) FROM reports WHERE message_id = 'm1'"
        ),
        Ok(Value::UInt64(1)),
        "the reused message id resolves to its newer row"
    );
    assert_eq!(
        scalar(&roomy, "SELECT id FROM reports WHERE message_id = 'm1'"),
        Ok(Value::UInt64(ROWS + 1))
    );
}

#[test]
fn a_cdc_table_flagged_for_reconciliation_keeps_the_policy() {
    let replica = seed("cdc", true, true);
    let roomy = replica.engine(None);
    assert_eq!(
        scalar(
            &roomy,
            "SELECT COUNT(*) FROM reports WHERE message_id = 'm1'"
        ),
        Ok(Value::UInt64(1))
    );
}
