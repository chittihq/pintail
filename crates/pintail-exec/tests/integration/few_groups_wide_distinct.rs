//! A report of a few groups whose COUNT(DISTINCT) sets are wide, under a
//! ceiling smaller than the sets.
//!
//! With so few groups the two-pass keeps all of them outside its partition
//! maps: in the dense slots of a text key, in the workers' pooled partials,
//! or in the integer range of a numeric key. Its spill valve has to see
//! those, or the sets grow to the ceiling and the query fails where it
//! should have gone to disk.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 800_000;
const TEAMS: u64 = 25;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "team", DataType::Utf8, true),
            Column::new(3, "squad", DataType::Int64, true),
            Column::new(4, "member", DataType::Int64, true),
            Column::new(5, "ticket", DataType::Int64, true),
            Column::new(6, "device", DataType::Int64, true),
            Column::new(7, "seen_at", DataType::DateTime64 { fsp: 0 }, true),
        ],
    )
    .expect("schema")
}

/// A value spread over the whole signed range, so a group's distinct set
/// cannot settle into a bitmap.
fn spread(id: u64, salt: u64) -> i64 {
    (id.wrapping_add(salt))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .cast_signed()
        >> 2
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        let rows = (0..ROWS)
            .map(|id| {
                let team = id % TEAMS;
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        if team == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(format!("team-{team:02}"))
                        },
                        Value::Int64(i64::try_from(team).expect("squad") * 3 - 20),
                        Value::Int64(spread(id, 1)),
                        // Every third ticket repeats, and some are NULL.
                        if id % 17 == 0 {
                            Value::Null
                        } else {
                            Value::Int64(spread(id - id % 3, 2))
                        },
                        Value::Int64(spread(id / 2, 3)),
                        Value::Utf8(format!(
                            "2026-{:02}-{:02} {:02}:{:02}:00",
                            1 + id % 12,
                            1 + id % 28,
                            id % 24,
                            id % 60
                        )),
                    ],
                    id + 1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        table.bulk_ingest_snapshot(rows).expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "visits",
            schema(),
            TableStatistics::with_row_count(ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog"),
        }
    }

    /// The sorted rows, the spill files written, and the most the query reserved.
    fn answer(&self, sql: &str, memory: usize) -> Result<(Vec<String>, u64, usize), String> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution = Execution::start(physical, &provider, memory, Collation::default())
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
        Ok((
            rows,
            execution.spill_metrics().files,
            execution.memory().peak(),
        ))
    }
}

const SETS: &str = "COUNT(DISTINCT member), COUNT(DISTINCT ticket), COUNT(DISTINCT device)";

/// Each shape, with the smallest ceiling it has to answer under. Below
/// that a batch folded whole can still outgrow the ceiling before anything
/// is there to spill; such a run may fail with a memory error and must
/// never answer differently.
#[test]
fn wide_distinct_sets_of_a_few_groups_spill_under_a_tight_ceiling() {
    let fixture = Fixture::new();
    for (label, columns, key, answers_from) in [
        // Dense slots and pooled partials: the two-pass with no map entry.
        ("a text key and distinct sets alone", "", "team", 64_usize),
        ("a text key beside a count", ", COUNT(ticket)", "team", 128),
        // Shapes the two-pass leaves to the general path today.
        (
            "a text key beside a temporal lane",
            ", MAX(seen_at)",
            "team",
            16,
        ),
        (
            "an integer key beside temporal lanes",
            ", MIN(seen_at), MAX(seen_at)",
            "squad",
            16,
        ),
        (
            "an expression key beside a temporal lane",
            ", MAX(seen_at)",
            "squad + 1",
            16,
        ),
    ] {
        let sql =
            format!("SELECT {key}, {SETS}{columns} FROM visits WHERE id < $EDGE GROUP BY {key}");
        let (roomy, _, _) = fixture
            .answer(&sql.replace("$EDGE", &ROWS.to_string()), 1 << 32)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            roomy.len(),
            usize::try_from(TEAMS).expect("teams"),
            "{label}: groups"
        );
        for mib in [16_usize, 64, 128] {
            // Another statement with the same answer, so the run is not a
            // replay of the roomy one.
            let edge = ROWS + u64::try_from(mib).expect("ceiling");
            match fixture.answer(&sql.replace("$EDGE", &edge.to_string()), mib << 20) {
                Ok((tight, files, peak)) => {
                    assert!(tight == roomy, "{label} at {mib} MiB: answer");
                    assert!(files > 0, "{label} at {mib} MiB: the sets fit, widen them");
                    assert!(peak <= mib << 20, "{label} at {mib} MiB: peak {peak}");
                    eprintln!("{label} at {mib} MiB: {files} spill files, peak {peak}");
                }
                Err(error) => {
                    assert!(mib < answers_from, "{label} at {mib} MiB: {error}");
                    assert!(
                        error.contains("memory limit exceeded"),
                        "{label} at {mib} MiB: {error}"
                    );
                    eprintln!("{label} at {mib} MiB: {error}");
                }
            }
        }
    }
}
