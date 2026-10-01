//! A grouped aggregate whose first batch shows a handful of keys and whose
//! tail shows a key per row.
//!
//! The small-group column fold is chosen on the first batch alone. Its
//! groups live in memory and it writes no run, so when the keys keep coming
//! it has to hand its groups and the rest of the input to the general
//! path, which spills. The answer must be the one the general path gives
//! from the first row, under a ceiling the general path fits.
//!
//! `cargo test --profile recovery -p pintail-exec --test integration small_group_handover::
//! -- --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "lane", DataType::Int64, true),
            Column::new(3, "seen_at", DataType::DateTime64 { fsp: 0 }, true),
            Column::new(4, "weight", DataType::Int64, true),
            Column::new(5, "ratio", DataType::Float64, true),
            Column::new(6, "tag", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

/// Rows whose first `quiet` hold four lanes and whose rest hold a lane per
/// row (every seventh repeats an earlier quiet lane, every 97th is NULL).
/// `reversed` stores the same rows under mirrored ids, so the busy rows
/// come first and the fold is never chosen.
fn rows(total: u64, quiet: u64, reversed: bool) -> Vec<StoredRow> {
    (0..total)
        .map(|position| {
            let id = if reversed {
                total - 1 - position
            } else {
                position
            };
            let source = if reversed { total - 1 - id } else { id };
            let lane = if source < quiet || source % 7 == 0 {
                Value::Int64(i64::try_from(source % 4).expect("lane"))
            } else if source % 97 == 0 {
                Value::Null
            } else {
                Value::Int64(i64::try_from(1_000 + source).expect("lane"))
            };
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                vec![
                    Value::UInt64(id),
                    lane,
                    if source % 31 == 0 {
                        Value::Null
                    } else {
                        Value::Utf8(format!(
                            "2026-{:02}-{:02} {:02}:{:02}:{:02}",
                            1 + source % 12,
                            1 + source % 28,
                            source % 24,
                            source % 60,
                            (source / 7) % 60
                        ))
                    },
                    Value::Int64(i64::try_from(source % 1_000).expect("weight") - 500),
                    Value::Float64(pintail_types::Float64::new(
                        f64::from(u32::try_from(source % 64).expect("ratio")) * 0.5,
                    )),
                    if source % 53 == 0 {
                        Value::Null
                    } else {
                        Value::Utf8(format!("tag-{:02}", source % 37))
                    },
                ],
                position + 1,
                false,
            )
        })
        .collect()
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

struct Answer {
    rows: Vec<String>,
    note: String,
    spill_files: u64,
    left_reserved: usize,
}

impl Fixture {
    fn new(total: u64, quiet: u64, reversed: bool) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table =
            TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
        table
            .bulk_ingest_snapshot(rows(total, quiet, reversed))
            .expect("rows");
        let entry = TableEntry::new(
            TableId::new(1),
            "sightings",
            schema(),
            TableStatistics::with_row_count(total),
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

    fn answer(&self, sql: &str, memory: usize) -> Result<Answer, String> {
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
        let note = execution
            .profile()
            .map(|profile| {
                profile
                    .operators
                    .iter()
                    .filter_map(|node| node.note.clone())
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();
        let spill_files = execution.spill_metrics().files;
        Ok(Answer {
            rows,
            note,
            spill_files,
            left_reserved: execution.memory().used(),
        })
    }
}

/// The fold's part of a profile's notes.
fn fold_note(note: &str) -> &str {
    note.split([';', '|'])
        .find(|part| part.contains("small-group"))
        .map_or("", str::trim)
}

const TOTAL: u64 = 300_000;
const QUIET: u64 = 140_000;
const ROOMY: usize = 1 << 32;
/// Ceilings from one the general path barely fits to one that holds most of
/// the groups: the fold stops at a different point under each.
const TIGHT: &[usize] = &[32 << 20, 96 << 20];

/// Queries the fold takes on a quiet first batch: a text MIN has no two-pass
/// lane, and an expression key is the fold's before it is the two-pass's.
const SHAPES: &[&str] = &[
    "SELECT lane, COUNT(*), MIN(tag), MAX(seen_at), SUM(weight) FROM sightings GROUP BY lane",
    "SELECT lane + 0, COUNT(*), MIN(seen_at), MAX(seen_at), SUM(weight) FROM sightings \
     GROUP BY lane + 0",
    // A float sum and a DISTINCT keep the fold serial.
    "SELECT lane, COUNT(DISTINCT weight), MAX(seen_at), SUM(ratio) FROM sightings GROUP BY lane",
];

#[test]
fn keys_that_keep_coming_leave_the_fold_and_answer_as_the_general_path() {
    let quiet_first = Fixture::new(TOTAL, QUIET, false);
    let busy_first = Fixture::new(TOTAL, QUIET, true);
    for sql in SHAPES {
        let reference = busy_first.answer(sql, ROOMY).expect("the general path");
        assert!(
            !reference.note.contains("small-group column fold:"),
            "{sql}: the reference took the fold: {}",
            reference.note
        );
        let roomy = quiet_first
            .answer(sql, ROOMY)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert!(
            roomy.note.contains("small-group column fold handed over"),
            "{sql}: the quiet first batch did not choose the fold: {}",
            roomy.note
        );
        assert_eq!(roomy.rows.len(), reference.rows.len(), "{sql}: groups");
        assert!(roomy.rows == reference.rows, "{sql}: roomy answer differs");

        for ceiling in TIGHT.iter().copied() {
            let general_tight = busy_first.answer(sql, ceiling);
            let tight = quiet_first.answer(sql, ceiling);
            eprintln!(
                "{sql} at {} MiB\n  general: {:?}\n  fold: {:?}",
                ceiling >> 20,
                general_tight.as_ref().map(|answer| (
                    answer.rows.len(),
                    answer.spill_files,
                    fold_note(&answer.note)
                )),
                tight.as_ref().map(|answer| (
                    answer.rows.len(),
                    answer.spill_files,
                    fold_note(&answer.note)
                )),
            );
            if let Ok(general) = general_tight {
                assert!(
                    general.rows == reference.rows,
                    "{sql}: general tight differs"
                );
                let tight = tight.unwrap_or_else(|error| {
                    panic!("{sql}: the general path fits this ceiling, the fold failed: {error}")
                });
                assert!(tight.rows == reference.rows, "{sql}: tight answer differs");
                assert!(
                    tight.note.contains("small-group column fold handed over"),
                    "{sql}: the tight run did not start in the fold: {}",
                    tight.note
                );
                assert_eq!(
                    tight.left_reserved, general.left_reserved,
                    "{sql}: reserved"
                );
            }
        }
    }
}
