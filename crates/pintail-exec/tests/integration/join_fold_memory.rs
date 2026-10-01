//! Joins folded into the aggregate above them, under ceilings from far too
//! small to roomy.
//!
//! The fused fold has several ways out once it has started: its build side
//! spills, its probe rounds meet the ceiling, a residual join meets a key
//! with a very long bucket. Whichever it takes, a run that answers must
//! answer the roomy rows - no row lost, none folded twice - and a run that
//! cannot fit must fail with the memory error and stay under its ceiling
//! on the way.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const SHELVES: u64 = 30_000;
const PICKS: u64 = 120_000;
const ECHOES: u64 = 40_000;

fn shelves_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "aisle", DataType::Utf8, false),
            Column::new(3, "zone", DataType::Int64, false),
        ],
    )
    .expect("shelves")
}

fn picks_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "shelf", DataType::UInt64, true),
            Column::new(3, "units", DataType::Int64, true),
            Column::new(4, "grams", DataType::Int64, true),
            Column::new(5, "picker", DataType::Int64, true),
        ],
    )
    .expect("picks")
}

/// Rows that all carry one of two join keys: the longest bucket a build
/// side can have.
fn echoes_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "tone", DataType::Int64, false),
            Column::new(3, "level", DataType::Int64, false),
        ],
    )
    .expect("echoes")
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id + 1,
        false,
    )
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).expect("fits")
}

struct Fixture {
    _directory: tempfile::TempDir,
    tables: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

struct Run {
    rows: Vec<String>,
    notes: String,
    spill_files: u64,
    peak: usize,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let open = |name: &str, schema: TableSchema, rows: Vec<StoredRow>| {
            let mut table =
                TableStore::open(directory.path().join(name), schema, StoreOptions::default())
                    .expect("table");
            table.bulk_ingest_snapshot(rows).expect("rows");
            table
        };
        let shelves = open(
            "shelves",
            shelves_schema(),
            (0..SHELVES)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            // Ten thousand aisles of three shelves each.
                            Value::Utf8(format!("aisle-{:06}", id / 3)),
                            Value::Int64(signed(id % 16)),
                        ],
                    )
                })
                .collect(),
        );
        let picks = open(
            "picks",
            picks_schema(),
            (0..PICKS)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            // Some picks name no shelf, some a shelf that is gone.
                            if id % 41 == 0 {
                                Value::Null
                            } else {
                                Value::UInt64((id * 7) % (SHELVES + 500))
                            },
                            Value::Int64(signed(id % 9) - 4),
                            if id % 13 == 0 {
                                Value::Null
                            } else {
                                Value::Int64(signed(id % 5_000))
                            },
                            Value::Int64(signed((id * 31) % 20_000)),
                        ],
                    )
                })
                .collect(),
        );
        let echoes = open(
            "echoes",
            echoes_schema(),
            (0..ECHOES)
                .map(|id| {
                    stored(
                        id,
                        vec![
                            Value::UInt64(id),
                            Value::Int64(if id % 1_000 == 0 { 2 } else { 1 }),
                            Value::Int64(signed(id % 1_000)),
                        ],
                    )
                })
                .collect(),
        );
        let entry = |id: u64, name: &str, schema: TableSchema, rows: u64| {
            TableEntry::new(
                TableId::new(id),
                name,
                schema,
                TableStatistics::with_row_count(rows),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key")
        };
        let catalog = CatalogSnapshot::new([DatabaseEntry::new(
            DatabaseId::new(1),
            "app",
            [
                entry(1, "shelves", shelves_schema(), SHELVES),
                entry(2, "picks", picks_schema(), PICKS),
                entry(3, "echoes", echoes_schema(), ECHOES),
            ],
        )
        .expect("database")])
        .expect("catalog");
        Self {
            _directory: directory,
            tables: vec![shelves, picks, echoes],
            catalog,
        }
    }

    fn run(&self, sql: &str, memory: usize) -> Result<Run, String> {
        let snapshots = self
            .tables
            .iter()
            .map(TableStore::snapshot)
            .collect::<Vec<_>>();
        let provider = SnapshotScanProvider::new(
            snapshots
                .iter()
                .zip(1_u64..)
                .map(|(snapshot, id)| (DatabaseId::new(1), TableId::new(id), snapshot)),
        )
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(physical, &provider, memory, None, Collation::default())
                .map_err(|error| error.to_string())?;
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().map_err(|error| error.to_string())? {
            for row in batch.selection().selected_rows() {
                rows.push(format!(
                    "{:?}",
                    (0..batch.columns().len())
                        .map(|column| batch
                            .column(column)
                            .and_then(|column| column.value(row))
                            .cloned())
                        .collect::<Vec<_>>()
                ));
            }
        }
        rows.sort();
        let notes = execution
            .profile()
            .map(|profile| {
                profile
                    .operators
                    .iter()
                    .filter_map(|node| node.note.as_deref())
                    .filter(|note| !note.starts_with("decompressed"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();
        Ok(Run {
            rows,
            notes,
            spill_files: execution.spill_metrics().files,
            peak: execution.memory().peak(),
        })
    }
}

const LANES: &str = "COUNT(*), SUM(p.units), MIN(p.units), MAX(p.grams), SUM(p.grams), \
     COUNT(p.grams), AVG(p.units), MAX(p.picker)";

#[test]
fn every_way_out_of_a_fused_join_answers_the_same_or_fails_cleanly() {
    let fixture = Fixture::new();
    let shapes = [
        (
            "many build-side groups, many lanes",
            format!(
                "SELECT s.aisle, {LANES} FROM picks p JOIN shelves s ON s.id = p.shelf \
                 WHERE p.id < $EDGE GROUP BY s.aisle"
            ),
        ),
        (
            "many build-side groups kept by an outer join",
            format!(
                "SELECT s.aisle, s.zone, {LANES} FROM picks p LEFT JOIN shelves s \
                 ON s.id = p.shelf WHERE p.id < $EDGE GROUP BY s.aisle, s.zone"
            ),
        ),
        (
            "sets that grow per row beside the lanes",
            "SELECT s.zone, COUNT(DISTINCT p.picker), COUNT(DISTINCT p.grams), SUM(p.units) \
             FROM picks p JOIN shelves s ON s.id = p.shelf WHERE p.id < $EDGE GROUP BY s.zone"
                .to_owned(),
        ),
        (
            "one key with a very long bucket, under a residual",
            "SELECT p.units, COUNT(*), SUM(e.level), MIN(e.id) FROM picks p JOIN echoes e \
             ON e.tone = p.units AND e.level > p.grams WHERE p.id < 600 AND p.id < $EDGE \
             GROUP BY p.units"
                .to_owned(),
        ),
    ];
    let mut failures = Vec::new();
    for (label, sql) in &shapes {
        let roomy = fixture
            .run(&sql.replace("$EDGE", &PICKS.to_string()), 1 << 32)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert!(!roomy.rows.is_empty(), "{label}: no rows");
        eprintln!(
            "{label}: {} rows, roomy peak {} MiB, {}",
            roomy.rows.len(),
            roomy.peak >> 20,
            roomy.notes
        );
        for mib in [4_usize, 6, 16, 64, 256] {
            // Another statement with the same answer: not a replay.
            let edge = PICKS + u64::try_from(mib).expect("ceiling");
            let run = fixture.run(&sql.replace("$EDGE", &edge.to_string()), mib << 20);
            match run {
                Ok(run) => {
                    eprintln!(
                        "  {mib} MiB: answered, {} spill files, tracked peak {} MiB, {}",
                        run.spill_files,
                        run.peak >> 20,
                        run.notes
                    );
                    if run.rows != roomy.rows {
                        failures.push(format!(
                            "{label} at {mib} MiB: {} rows against {} ({})",
                            run.rows.len(),
                            roomy.rows.len(),
                            run.notes
                        ));
                    }
                    if run.peak > mib << 20 {
                        failures.push(format!("{label} at {mib} MiB: peak {}", run.peak));
                    }
                    // Thirty thousand groups at most: the runs the aggregate
                    // writes follow what its map holds, not how often the
                    // ceiling is close.
                    if run.spill_files > 3_000 {
                        failures.push(format!(
                            "{label} at {mib} MiB: {} spill files",
                            run.spill_files
                        ));
                    }
                }
                Err(error) => {
                    eprintln!("  {mib} MiB: {error}");
                    // Every shape answers from 16 MiB up; below that a clean
                    // memory error is the other acceptable end.
                    if mib >= 16 || !error.contains("memory limit exceeded") {
                        failures.push(format!("{label} at {mib} MiB: {error}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
