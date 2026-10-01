//! Filtered reads of a table whose latest writes sit over its segments,
//! checked against a model of the rows at every stage of its life.
//!
//! Updates move rows across a predicate in both directions, deletes and
//! inserts interleave with them, the ENUM column's declaration is reordered
//! and extended, and the same questions are asked while the writes are in
//! the memtable, after a flush, over two generations of segments, and after
//! compaction - under ceilings from roomy down to ones that make the scan
//! read a segment in slices. Every surviving row must be read exactly once
//! and every filter must still apply.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 60_000;
const FIRST_LABELS: &[&str] = &["new", "packed", "sent", "lost"];
/// The declaration after the ALTER: two members swapped, one added.
const SECOND_LABELS: &[&str] = &["new", "sent", "packed", "lost", "held"];
const ZONES: &[&str] = &["north", "south", "east", "west", "dock"];

fn schema(version: u32, labels: &[&str]) -> TableSchema {
    TableSchema::new(
        version,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "state", DataType::Utf8, true)
                .with_enum_labels(Some(labels.iter().map(ToString::to_string).collect())),
            Column::new(3, "weight", DataType::Int64, true),
            Column::new(4, "zone", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

/// One row as the model holds it.
#[derive(Clone, Debug, PartialEq)]
struct Parcel {
    state: Option<&'static str>,
    weight: Option<i64>,
    zone: &'static str,
}

fn base(id: u64) -> Parcel {
    Parcel {
        state: (!id.is_multiple_of(23))
            .then(|| FIRST_LABELS[usize::try_from(id % 4).expect("label")]),
        weight: (!id.is_multiple_of(17)).then(|| i64::try_from(id % 1_000).expect("weight")),
        zone: ZONES[usize::try_from(id % 5).expect("zone")],
    }
}

fn stored(id: u64, parcel: &Parcel, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            parcel
                .state
                .map_or(Value::Null, |state| Value::Utf8(state.to_owned())),
            parcel.weight.map_or(Value::Null, Value::Int64),
            Value::Utf8(parcel.zone.to_owned()),
        ],
        version,
        deleted,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    labels: &'static [&'static str],
    schema_version: u32,
    model: BTreeMap<u64, Parcel>,
    version: u64,
    /// Per ceiling: statements answered, and statements refused for memory.
    tally: std::cell::RefCell<BTreeMap<usize, (usize, usize)>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(
            directory.path(),
            schema(1, FIRST_LABELS),
            StoreOptions {
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .expect("table");
        table
            .bulk_ingest_snapshot(
                (1..=ROWS)
                    .map(|id| stored(id, &base(id), 1, false))
                    .collect(),
            )
            .expect("ingest");
        Self {
            _directory: directory,
            table,
            labels: FIRST_LABELS,
            schema_version: 1,
            model: (1..=ROWS).map(|id| (id, base(id))).collect(),
            version: 1,
            tally: std::cell::RefCell::default(),
        }
    }

    /// One generation of writes, `salt` choosing which rows it touches:
    /// weights pushed over the 500 line and pulled back under it, states
    /// moved onto and off 'sent', rows deleted, deleted rows written again,
    /// and new rows between and past the old ones.
    fn write(&mut self, salt: u64) {
        self.version += 1;
        let version = self.version;
        let mut writes = Vec::new();
        let last = self.labels[self.labels.len() - 1];
        for step in 0..900_u64 {
            let id = 1 + (step * 61 + salt * 7) % ROWS;
            let Some(old) = self.model.get(&id).cloned() else {
                // Deleted by an earlier generation: write it again.
                let parcel = Parcel {
                    state: Some(last),
                    weight: Some(i64::try_from(400 + step % 200).expect("weight")),
                    zone: ZONES[usize::try_from((step + salt) % 5).expect("zone")],
                };
                writes.push(stored(id, &parcel, version, false));
                self.model.insert(id, parcel);
                continue;
            };
            if step % 11 == 0 {
                writes.push(stored(id, &old, version, true));
                self.model.remove(&id);
                continue;
            }
            let parcel = Parcel {
                // Across the weight line, in whichever direction it was not.
                weight: match (step % 7, old.weight) {
                    (0, _) => None,
                    (_, Some(weight)) if weight >= 500 => Some(weight - 500),
                    (_, Some(weight)) => Some(weight + 500),
                    (_, None) => Some(i64::try_from(499 + step % 3).expect("weight")),
                },
                // Onto 'sent' and off it.
                state: match (step % 5, old.state) {
                    (0, _) => None,
                    (_, Some("sent")) => Some(last),
                    _ => Some("sent"),
                },
                zone: if step % 3 == 0 { "dock" } else { old.zone },
            };
            writes.push(stored(id, &parcel, version, false));
            self.model.insert(id, parcel);
        }
        for step in 0..60_u64 {
            let id = ROWS + 1 + salt * 1_000 + step * 3;
            let parcel = Parcel {
                state: Some(self.labels[usize::try_from(step).expect("label") % self.labels.len()]),
                weight: Some(i64::try_from(470 + step).expect("weight")),
                zone: ZONES[usize::try_from(step % 5).expect("zone")],
            };
            writes.push(stored(id, &parcel, version, false));
            self.model.insert(id, parcel);
        }
        self.table.ingest(writes).expect("writes");
    }

    fn alter(&mut self) {
        self.schema_version += 1;
        self.labels = SECOND_LABELS;
        self.table
            .evolve_schema(schema(self.schema_version, SECOND_LABELS))
            .expect("evolve");
    }

    fn run(&self, sql: &str, memory: usize) -> Result<Vec<Vec<Value>>, String> {
        let outcome = self.execute(sql, memory);
        let mut tally = self.tally.borrow_mut();
        let entry = tally.entry(memory).or_default();
        match &outcome {
            Ok(_) => entry.0 += 1,
            Err(_) => entry.1 += 1,
        }
        outcome
    }

    fn execute(&self, sql: &str, memory: usize) -> Result<Vec<Vec<Value>>, String> {
        let entry = TableEntry::new(
            TableId::new(1),
            "parcels",
            schema(self.schema_version, self.labels),
            TableStatistics::with_row_count(ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&catalog, Some("app"))
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
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                        .collect::<Vec<_>>(),
                );
            }
        }
        Ok(rows)
    }

    fn ordinal(&self, state: Option<&str>) -> Option<usize> {
        state.map(|state| {
            self.labels
                .iter()
                .position(|label| *label == state)
                .expect("a declared label")
        })
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Utf8(text) | Value::Enum { label: text, .. } => Some(text.clone()),
        other => panic!("not text: {other:?}"),
    }
}

fn id_of(value: &Value) -> u64 {
    match value {
        Value::UInt64(id) => *id,
        Value::Int64(id) => u64::try_from(*id).expect("id"),
        other => panic!("not an id: {other:?}"),
    }
}

fn whole(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        Value::UInt64(value) => i64::try_from(*value).expect("fits"),
        Value::Utf8(text) => text.parse().expect("whole decimal"),
        other => panic!("not an integer: {other:?}"),
    }
}

/// The ids of a filtered read, each exactly once, against the model's.
fn expect_ids(
    fixture: &Fixture,
    stage: &str,
    memory: usize,
    predicate: &str,
    keep: impl Fn(&Parcel) -> bool,
    failures: &mut Vec<String>,
) {
    for projection in ["id", "id, state, weight, zone"] {
        let sql = format!("SELECT {projection} FROM parcels WHERE {predicate}");
        let rows = match fixture.run(&sql, memory) {
            Ok(rows) => rows,
            Err(error) if error.contains("memory limit exceeded") => continue,
            Err(error) => {
                failures.push(format!("{stage} at {memory}: {sql}: {error}"));
                continue;
            }
        };
        let mut seen = rows.iter().map(|row| id_of(&row[0])).collect::<Vec<_>>();
        seen.sort_unstable();
        let expected = fixture
            .model
            .iter()
            .filter(|(_, parcel)| keep(parcel))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if seen != expected {
            let twice = seen.windows(2).filter(|pair| pair[0] == pair[1]).count();
            let missing = expected
                .iter()
                .filter(|id| seen.binary_search(id).is_err())
                .count();
            let extra = seen
                .iter()
                .filter(|id| expected.binary_search(id).is_err())
                .count();
            failures.push(format!(
                "{stage} at {memory}: {sql}: {} rows against {}, {twice} twice, {missing} \
                 missing, {extra} that the filter should have dropped",
                seen.len(),
                expected.len()
            ));
            continue;
        }
        if projection != "id" {
            for row in &rows {
                let parcel = &fixture.model[&id_of(&row[0])];
                let read = (
                    text(&row[1]),
                    (!matches!(row[2], Value::Null)).then(|| whole(&row[2])),
                    text(&row[3]),
                );
                let held = (
                    parcel.state.map(str::to_owned),
                    parcel.weight,
                    Some(parcel.zone.to_owned()),
                );
                if read != held {
                    failures.push(format!(
                        "{stage} at {memory}: {sql}: id {} read {read:?}, holds {held:?}",
                        id_of(&row[0])
                    ));
                    break;
                }
            }
        }
    }
}

fn check(fixture: &Fixture, stage: &str, failures: &mut Vec<String>) {
    for memory in [1_usize << 30, 8 << 20, 3 << 20, 1 << 20, 600 << 10] {
        expect_ids(
            fixture,
            stage,
            memory,
            "weight >= 500",
            |p| p.weight >= Some(500),
            failures,
        );
        expect_ids(
            fixture,
            stage,
            memory,
            "weight < 500",
            |p| p.weight.is_some_and(|weight| weight < 500),
            failures,
        );
        expect_ids(
            fixture,
            stage,
            memory,
            "weight IS NULL",
            |p| p.weight.is_none(),
            failures,
        );
        expect_ids(
            fixture,
            stage,
            memory,
            "state = 'sent'",
            |p| p.state == Some("sent"),
            failures,
        );
        // An ENUM against a string constant compares as text.
        expect_ids(
            fixture,
            stage,
            memory,
            "state > 'lost'",
            |p| p.state.is_some_and(|state| state > "lost"),
            failures,
        );
        expect_ids(
            fixture,
            stage,
            memory,
            "zone = 'dock' AND weight BETWEEN 480 AND 520",
            |p| p.zone == "dock" && p.weight.is_some_and(|weight| (480..=520).contains(&weight)),
            failures,
        );
        check_folds(fixture, stage, memory, failures);
    }
}

/// Aggregates over the same filters: the folds read the overlay too.
fn check_folds(fixture: &Fixture, stage: &str, memory: usize, failures: &mut Vec<String>) {
    {
        let sql = "SELECT zone, COUNT(*), SUM(weight), MIN(weight), MAX(weight), COUNT(state) \
                   FROM parcels WHERE weight >= 500 OR state = 'sent' GROUP BY zone";
        match fixture.run(sql, memory) {
            Ok(rows) => {
                let mut read = rows
                    .iter()
                    .map(|row| {
                        (
                            text(&row[0]).expect("zone"),
                            whole(&row[1]),
                            (!matches!(row[2], Value::Null)).then(|| whole(&row[2])),
                            (!matches!(row[3], Value::Null)).then(|| whole(&row[3])),
                            (!matches!(row[4], Value::Null)).then(|| whole(&row[4])),
                            whole(&row[5]),
                        )
                    })
                    .collect::<Vec<_>>();
                read.sort();
                let mut zones = BTreeMap::<&str, Vec<&Parcel>>::new();
                for parcel in fixture.model.values() {
                    if parcel.weight >= Some(500) || parcel.state == Some("sent") {
                        zones.entry(parcel.zone).or_default().push(parcel);
                    }
                }
                let expected = zones
                    .into_iter()
                    .map(|(zone, parcels)| {
                        let weights = parcels.iter().filter_map(|p| p.weight).collect::<Vec<_>>();
                        (
                            zone.to_owned(),
                            i64::try_from(parcels.len()).expect("count"),
                            (!weights.is_empty()).then(|| weights.iter().sum::<i64>()),
                            weights.iter().min().copied(),
                            weights.iter().max().copied(),
                            i64::try_from(parcels.iter().filter(|p| p.state.is_some()).count())
                                .expect("count"),
                        )
                    })
                    .collect::<Vec<_>>();
                if read != expected {
                    failures.push(format!(
                        "{stage} at {memory}: {sql}: {read:?} against {expected:?}"
                    ));
                }
            }
            Err(error) if error.contains("memory limit exceeded") => {}
            Err(error) => failures.push(format!("{stage} at {memory}: {sql}: {error}")),
        }
        // ENUM order is the declaration's, before and after the ALTER.
        let sql = "SELECT state, COUNT(*) FROM parcels WHERE weight < 500 GROUP BY state \
                   ORDER BY state";
        match fixture.run(sql, memory) {
            Ok(rows) => {
                let read = rows
                    .iter()
                    .map(|row| (text(&row[0]), whole(&row[1])))
                    .collect::<Vec<_>>();
                let mut counts = BTreeMap::<Option<usize>, i64>::new();
                for parcel in fixture.model.values() {
                    if parcel.weight.is_some_and(|weight| weight < 500) {
                        *counts.entry(fixture.ordinal(parcel.state)).or_default() += 1;
                    }
                }
                let expected = counts
                    .into_iter()
                    .map(|(ordinal, count)| {
                        (
                            ordinal.map(|ordinal| fixture.labels[ordinal].to_owned()),
                            count,
                        )
                    })
                    .collect::<Vec<_>>();
                if read != expected {
                    failures.push(format!(
                        "{stage} at {memory}: {sql}: {read:?} against {expected:?}"
                    ));
                }
            }
            Err(error) if error.contains("memory limit exceeded") => {}
            Err(error) => failures.push(format!("{stage} at {memory}: {sql}: {error}")),
        }
    }
}

#[test]
fn filtered_reads_match_the_model_through_writes_flushes_an_alter_and_compaction() {
    let mut fixture = Fixture::new();
    let mut failures = Vec::new();
    check(&fixture, "snapshot only", &mut failures);
    fixture.write(1);
    check(&fixture, "writes in the memtable", &mut failures);
    fixture.table.flush().expect("flush");
    check(&fixture, "writes flushed", &mut failures);
    fixture.write(2);
    check(
        &fixture,
        "a second generation over two segments",
        &mut failures,
    );
    fixture.alter();
    check(&fixture, "after the ENUM is redeclared", &mut failures);
    fixture.write(3);
    check(&fixture, "writes under the new declaration", &mut failures);
    fixture.table.flush().expect("flush");
    fixture.write(4);
    fixture.table.compact().expect("compact");
    check(&fixture, "compacted, with writes over it", &mut failures);
    fixture.table.flush().expect("flush");
    fixture.table.compact().expect("compact");
    check(&fixture, "compacted whole", &mut failures);
    let tally = fixture.tally.borrow();
    eprintln!("answered and refused per ceiling: {tally:?}");
    assert_eq!(tally[&(1 << 30)].1, 0, "a roomy read was refused");
    assert!(
        tally
            .iter()
            .any(|(_, (answered, refused))| *answered > 0 && *refused > 0),
        "no ceiling straddles what the scan needs: {tally:?}"
    );
    failures.truncate(12);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
