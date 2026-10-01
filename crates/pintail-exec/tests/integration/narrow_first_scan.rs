//! A selective scan decodes its narrow predicate columns first and the rest
//! of its projection only for the rows they keep. Three shapes used to miss
//! that path and decode every projected column of every row: a table read
//! under an alias, a predicate that tests a wide column (a JSON document
//! tested for NULL) beside a selective narrow one, and a join whose key
//! span bounds a column that is not the scanned table's key. Every answer
//! here is checked against a model computed from the row generators.
//!
//! The measurement is `#[ignore]`d, and a repeat answered from the settled
//! memo measures nothing, so it runs with the memo off:
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p pintail-exec
//! --test integration narrow_first_scan:: -- --ignored --nocapture`.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const PARENT_ID: TableId = TableId::new(1);
const RECORD_ID: TableId = TableId::new(2);
const PARENTS: u64 = 5_000;
const RECORDS: u64 = 200_000;

fn parent_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, false),
        ],
    )
    .expect("parent schema")
}

fn record_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "owner", DataType::Int64, false),
            Column::new(3, "parent_id", DataType::Int64, true),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "doc", DataType::Json, true),
        ],
    )
    .expect("record schema")
}

/// Owners scatter across the whole table, so no block can be skipped on
/// them; parents follow the key, as ids assigned in insertion order do.
fn owner(id: u64) -> i64 {
    i64::try_from(id % 997).expect("owner")
}

fn parent(id: u64) -> Option<i64> {
    (!id.is_multiple_of(101)).then(|| i64::try_from(id / 50 + 1).expect("parent"))
}

fn amount(id: u64) -> i64 {
    i64::try_from(id % 89).expect("amount") - 30
}

fn doc(id: u64) -> Option<String> {
    (!id.is_multiple_of(3)).then(|| {
        format!(
            "{{\"id\": {id}, \"body\": \"{}\"}}",
            "a document wide enough to cost real decoding ".repeat(6)
        )
    })
}

fn record_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(owner(id)),
            parent(id).map_or(Value::Null, Value::Int64),
            Value::Int64(amount(id)),
            doc(id).map_or(Value::Null, Value::Utf8),
        ],
        1,
        false,
    )
}

fn parent_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % 40).expect("grp")),
        ],
        1,
        false,
    )
}

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    parents: TableStore,
    records: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let options = StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let parent_dir = tempfile::tempdir().expect("parent dir");
        let record_dir = tempfile::tempdir().expect("record dir");
        let mut parents =
            TableStore::open(parent_dir.path(), parent_schema(), options).expect("open");
        parents
            .bulk_ingest_snapshot((1..=PARENTS).map(parent_row).collect())
            .expect("ingest parents");
        let mut records =
            TableStore::open(record_dir.path(), record_schema(), options).expect("open");
        records
            .bulk_ingest_snapshot((1..=RECORDS).map(record_row).collect())
            .expect("ingest records");
        let parent_entry = TableEntry::new(
            PARENT_ID,
            "parents",
            parent_schema(),
            TableStatistics::with_row_count(PARENTS),
        )
        .expect("parent entry")
        .with_key_columns([1])
        .expect("parent key");
        let record_entry = TableEntry::new(
            RECORD_ID,
            "records",
            record_schema(),
            TableStatistics::with_row_count(RECORDS),
        )
        .expect("record entry")
        .with_key_columns([1])
        .expect("record key");
        let database =
            DatabaseEntry::new(DATABASE_ID, "app", [parent_entry, record_entry]).expect("database");
        Self {
            _dirs: (parent_dir, record_dir),
            parents,
            records,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let parent_snapshot = self.parents.snapshot();
        let record_snapshot = self.records.snapshot();
        let provider = SnapshotScanProvider::new([
            (DATABASE_ID, PARENT_ID, &parent_snapshot),
            (DATABASE_ID, RECORD_ID, &record_snapshot),
        ])
        .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for index in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(index).cloned().expect("value"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    }
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        Value::UInt64(value) => i64::try_from(*value).expect("fits"),
        Value::Utf8(text) => text.parse().expect("whole decimal"),
        other => panic!("not an integer: {other:?}"),
    }
}

/// `(count, sum of amount)` over the records one owner holds with a
/// document.
fn owner_model(wanted: i64) -> (i64, i64) {
    (1..=RECORDS)
        .filter(|id| owner(*id) == wanted && doc(*id).is_some())
        .fold((0, 0), |(count, sum), id| (count + 1, sum + amount(id)))
}

#[test]
fn an_aliased_scan_with_a_wide_null_test_answers_exactly() {
    let fixture = Fixture::new();
    let (count, sum) = owner_model(5);
    for sql in [
        "SELECT COUNT(*), SUM(amount) FROM records WHERE owner = 5 AND doc IS NOT NULL",
        "SELECT COUNT(*), SUM(r.amount) FROM records r WHERE r.owner = 5 AND r.doc IS NOT NULL",
        "SELECT COUNT(*), SUM(r.amount) FROM records AS r \
         WHERE r.doc IS NOT NULL AND r.owner = 5 AND r.amount > -1000",
    ] {
        let rows = fixture.run(sql);
        assert_eq!(rows.len(), 1, "{sql}");
        assert_eq!((int(&rows[0][0]), int(&rows[0][1])), (count, sum), "{sql}");
    }

    let rows = fixture
        .run("SELECT r.id, r.doc FROM records r WHERE r.owner = 7 AND r.doc IS NULL ORDER BY r.id");
    let expected = (1..=RECORDS)
        .filter(|id| owner(*id) == 7 && doc(*id).is_none())
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter()
            .map(|row| u64::try_from(int(&row[0])).expect("id"))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(rows.iter().all(|row| matches!(row[1], Value::Null)));

    let rows = fixture.run(
        "SELECT r.id, r.doc FROM records r WHERE r.owner = 11 AND r.doc IS NOT NULL ORDER BY r.id",
    );
    let expected = (1..=RECORDS)
        .filter(|id| owner(*id) == 11)
        .filter_map(|id| doc(id).map(|doc| (id, doc)))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), expected.len());
    for (row, (id, doc)) in rows.iter().zip(&expected) {
        assert_eq!(u64::try_from(int(&row[0])).expect("id"), *id);
        assert!(
            matches!(&row[1], Value::Utf8(text) if text.contains(&format!("\"id\": {id}"))
                && text.len() + 2 >= doc.len()),
            "{row:?}"
        );
    }
}

#[test]
fn a_join_key_span_on_a_non_key_column_keeps_every_join_kind_exact() {
    let fixture = Fixture::new();
    let matching = |low: u64, high: u64| {
        (1..=RECORDS)
            .filter(|id| {
                parent(*id).is_some_and(|parent| {
                    (i64::try_from(low).expect("low")..=i64::try_from(high).expect("high"))
                        .contains(&parent)
                })
            })
            .fold((0, 0), |(count, sum), id| (count + 1, sum + amount(id)))
    };
    let (count, sum) = matching(3_000, 3_010);
    for sql in [
        "SELECT COUNT(*), SUM(r.amount) FROM parents p JOIN records r ON r.parent_id = p.id \
         WHERE p.id BETWEEN 3000 AND 3010",
        "SELECT COUNT(*), SUM(r.amount) FROM records r JOIN parents p ON r.parent_id = p.id \
         WHERE p.id BETWEEN 3000 AND 3010",
        "SELECT COUNT(*), SUM(r.amount) FROM parents p JOIN records r ON r.parent_id = p.id \
         WHERE p.id BETWEEN 3000 AND 3010 AND r.owner >= 0",
    ] {
        let rows = fixture.run(sql);
        assert_eq!((int(&rows[0][0]), int(&rows[0][1])), (count, sum), "{sql}");
    }

    // A LEFT join keeps parents without records; the span must not drop
    // either side's rows from the answer.
    let rows = fixture.run(
        "SELECT p.id, COUNT(r.id) FROM parents p LEFT JOIN records r ON r.parent_id = p.id \
         WHERE p.id BETWEEN 3995 AND 4005 GROUP BY p.id ORDER BY p.id",
    );
    let expected = (3_995_u64..=4_005)
        .map(|parent_id| {
            let found = (1..=RECORDS)
                .filter(|id| parent(*id) == Some(i64::try_from(parent_id).expect("id")))
                .count();
            (parent_id, i64::try_from(found).expect("count"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter()
            .map(|row| (u64::try_from(int(&row[0])).expect("id"), int(&row[1])))
            .collect::<Vec<_>>(),
        expected
    );

    // Parents scattered over the whole key range leave a span covering
    // nearly everything: the answer holds when the span cannot skip.
    let rows = fixture
        .run("SELECT COUNT(*) FROM parents p JOIN records r ON r.parent_id = p.id WHERE p.grp = 3");
    let expected = (1..=RECORDS)
        .filter(|id| {
            parent(*id).is_some_and(|parent| {
                parent <= i64::try_from(PARENTS).expect("parents") && parent % 40 == 3
            })
        })
        .count();
    assert_eq!(int(&rows[0][0]), i64::try_from(expected).expect("count"));
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_narrow_first_scans() {
    let fixture = Fixture::new();
    for sql in [
        "SELECT COUNT(*) FROM records WHERE parent_id = 1234 AND doc IS NOT NULL",
        "SELECT r.id, r.doc FROM records r WHERE r.parent_id = 1234",
        "SELECT COUNT(*), SUM(r.amount) FROM parents p JOIN records r ON r.parent_id = p.id \
         WHERE p.id BETWEEN 3000 AND 3010",
    ] {
        fixture.run(sql);
        let mut samples = (0..7)
            .map(|_| {
                let started = std::time::Instant::now();
                fixture.run(sql);
                started.elapsed().as_secs_f64() * 1_000.0
            })
            .collect::<Vec<_>>();
        samples.sort_by(f64::total_cmp);
        println!("{:>8.2} ms median  {sql}", samples[samples.len() / 2]);
    }
}
