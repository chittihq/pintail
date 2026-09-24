//! A scan that evaluated its predicates to choose rows hands the Filter
//! above a batch it may pass untested. That is only sound when every row
//! of the batch passed: ranges widened across rejected rows, NULLs, and
//! memtable rows interleaved into the chunk must all still meet the Filter.
//! Every answer here is checked against a model of the table.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 300_000;
const BLOCK: u64 = 50_000;

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "w", DataType::Int64, true),
            Column::new(4, "x", DataType::Int64, false),
            Column::new(5, "tag", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

#[derive(Clone)]
struct Row {
    name: String,
    w: Option<i64>,
    x: i64,
    tag: Option<String>,
}

fn base(id: u64) -> Row {
    let position = id % BLOCK;
    // The first fifth of every block holds short passing runs of `w < 100`
    // separated by gaps narrower than the scan's range merge; the rest fails
    // it, with a NULL now and then.
    let w = if position < 10_000 {
        Some(i64::try_from(position % 700).expect("small"))
    } else if position.is_multiple_of(997) {
        None
    } else {
        Some(5_000)
    };
    Row {
        name: format!("name-{position:06}"),
        w,
        x: i64::try_from(id % 997).expect("small"),
        // NULL for a whole stretch that would otherwise sort first.
        tag: (!(20_000..24_000).contains(&position)).then(|| format!("t{:03}", id % 400)),
    }
}

fn stored(id: u64, row: &Row, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(row.name.clone()),
            row.w.map_or(Value::Null, Value::Int64),
            Value::Int64(row.x),
            row.tag.clone().map_or(Value::Null, Value::Utf8),
        ],
        version,
        deleted,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
    model: BTreeMap<u64, Row>,
}

impl Fixture {
    fn new(memtable_writes: bool) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(
            directory.path(),
            schema(),
            StoreOptions {
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .expect("table");
        let mut model = (0..ROWS)
            .map(|id| (id, base(id)))
            .collect::<BTreeMap<_, _>>();
        table
            .bulk_ingest_snapshot(
                model
                    .iter()
                    .map(|(id, row)| stored(*id, row, 1, false))
                    .collect(),
            )
            .expect("ingest");
        if memtable_writes {
            // Updates that turn passing rows into failing ones and back over
            // the first half, deletes inside passing runs everywhere - so
            // the second half's slices carry tombstones and no live rows -
            // and inserts past the segments.
            let mut writes = Vec::new();
            for k in 0..400_u64 {
                let id = 3 + k * 373;
                let mut row = model[&id].clone();
                if k % 2 == 0 {
                    "zzz".clone_into(&mut row.name);
                    row.w = Some(9_999);
                    row.tag = None;
                } else {
                    "aaa".clone_into(&mut row.name);
                    row.w = Some(1);
                    row.tag = Some("t000".to_owned());
                }
                row.x = -7;
                writes.push(stored(id, &row, 2, false));
                model.insert(id, row);
            }
            for k in 0..60_u64 {
                let id = 11 + k * 4_999;
                writes.push(stored(id, &model[&id], 2, true));
                model.remove(&id);
            }
            for id in [ROWS + 1, ROWS + 9, ROWS + 20_000] {
                let row = Row {
                    name: "name-000001".to_owned(),
                    w: Some(3),
                    x: 11,
                    tag: Some("t001".to_owned()),
                };
                writes.push(stored(id, &row, 2, false));
                model.insert(id, row);
            }
            table.ingest(writes).expect("memtable writes");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "t",
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
            model,
        }
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value_owned(row).expect("value"))
                        .collect(),
                );
            }
        }
        rows
    }

    /// `(COUNT(*), SUM(x), SUM(id))` over the rows `keep` accepts.
    fn expected(&self, keep: impl Fn(&Row) -> bool) -> (i128, Option<i128>, Option<i128>) {
        let mut count = 0;
        let (mut x, mut ids) = (0, 0);
        for (id, row) in &self.model {
            if keep(row) {
                count += 1;
                x += i128::from(row.x);
                ids += i128::from(*id);
            }
        }
        (count, (count > 0).then_some(x), (count > 0).then_some(ids))
    }
}

fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Null => None,
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        Value::Utf8(text) => Some(text.parse().expect("integral sum")),
        other => panic!("unexpected aggregate value {other:?}"),
    }
}

type Case = (&'static str, fn(&Row) -> bool);

const CASES: [Case; 8] = [
    // Survivors in long runs: the scan's ranges are exact.
    ("name < 'name-002500'", |row| {
        row.name.as_str() < "name-002500"
    }),
    // Short runs merged across rejected rows: the ranges are not exact.
    ("w < 100", |row| row.w.is_some_and(|w| w < 100)),
    ("w < 100 AND name < 'name-005000'", |row| {
        row.w.is_some_and(|w| w < 100) && row.name.as_str() < "name-005000"
    }),
    // NULL is neither true nor false.
    ("tag < 't050'", |row| {
        row.tag.as_deref().is_some_and(|tag| tag < "t050")
    }),
    ("NOT (w >= 100)", |row| row.w.is_some_and(|w| w < 100)),
    ("w IS NULL", |row| row.w.is_none()),
    ("tag IS NULL", |row| row.tag.is_none()),
    ("name >= 'name-049000' AND tag IS NOT NULL", |row| {
        row.name.as_str() >= "name-049000" && row.tag.is_some()
    }),
];

fn check(fixture: &Fixture) {
    for (predicate, keep) in CASES {
        let (count, x, ids) = fixture.expected(keep);
        let rows = fixture.run(&format!(
            "SELECT COUNT(*), SUM(x), SUM(id) FROM t WHERE {predicate}"
        ));
        let actual = rows[0].iter().map(integer).collect::<Vec<_>>();
        assert_eq!(actual, vec![Some(count), x, ids], "WHERE {predicate}");
        // The same rows through a grouped consumer and a plain projection.
        let grouped = fixture.run(&format!(
            "SELECT x % 3, COUNT(*) FROM t WHERE {predicate} GROUP BY x % 3"
        ));
        let grouped_total: i128 = grouped.iter().filter_map(|row| integer(&row[1])).sum();
        assert_eq!(grouped_total, count, "grouped WHERE {predicate}");
        let listed = fixture.run(&format!("SELECT id, x FROM t WHERE {predicate}"));
        assert_eq!(
            i128::try_from(listed.len()).expect("small"),
            count,
            "listed WHERE {predicate}"
        );
    }
}

#[test]
fn prefiltered_scans_answer_as_the_filter_would() {
    check(&Fixture::new(false));
}

#[test]
fn prefiltered_scans_answer_as_the_filter_would_over_memtable_rows() {
    check(&Fixture::new(true));
}
