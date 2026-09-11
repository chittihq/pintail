//! A scan of a keyed table already yields its rows in key order, so the
//! planner leaves out a sort by the leading integer key columns. These
//! tests pin both halves of that: the plan carries no sort exactly when the
//! order is proven, and the rows then arrive as a real sort would put them,
//! through every path storage serves a scan by - overlapping segments, the
//! memtable's updates, deletes and inserts, and the small-table row path.
//!
//! Each query is compared with the same query forced through a sort (`id +
//! 0` is an expression, which no scan order satisfies) and with a model.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlan, PhysicalPlanner, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(
        schema: &TableSchema,
        keys: &[u32],
        table: TableStore,
        directory: tempfile::TempDir,
    ) -> Self {
        let entry = TableEntry::new(
            TableId::new(1),
            "t",
            schema.clone(),
            TableStatistics::with_row_count(1),
        )
        .expect("entry")
        .with_key_columns(keys.iter().copied())
        .expect("key");
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            table,
            catalog,
        }
    }

    fn plan(&self, sql: &str) -> PhysicalPlan {
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan")
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let mut execution =
            Execution::start(self.plan(sql), &provider, 1 << 30, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
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
        rows
    }
}

/// Whether the plan sorts anywhere along its single-input spine.
fn sorts(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::Sort { .. } => true,
        PhysicalPlan::Limit { input, .. }
        | PhysicalPlan::Project { input, .. }
        | PhysicalPlan::Filter { input, .. } => sorts(input),
        _ => false,
    }
}

fn open(schema: &TableSchema) -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("directory");
    let table = TableStore::open(
        directory.path(),
        schema.clone(),
        StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    (directory, table)
}

fn single_key_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "grp", DataType::Int64, true),
            Column::new(3, "note", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn single_row(id: i64, grp: i64, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            Value::Int64(grp),
            Value::Utf8(format!("n{}", id.rem_euclid(13))),
        ],
        version,
        deleted,
    )
}

/// A table of `span` keys either side of zero, written as two flushed
/// segments whose key ranges interleave (odd keys, then even), then
/// updated, deleted from and inserted into through the memtable. Returns
/// the fixture and the model of `id -> grp` it should answer.
fn single_key_fixture(span: i64) -> (Fixture, BTreeMap<i64, i64>) {
    let schema = single_key_schema();
    let (directory, mut table) = open(&schema);
    let mut model = BTreeMap::new();
    for parity in [1, 0] {
        let rows = (-span..=span)
            .filter(|id| id.rem_euclid(2) == parity)
            .map(|id| {
                model.insert(id, id.rem_euclid(7));
                single_row(id, id.rem_euclid(7), 1, false)
            })
            .collect();
        table.ingest(rows).expect("segment");
        table.flush().expect("flush");
    }
    let mut writes = Vec::new();
    for k in 0..200_i64 {
        let id = -span + k * (2 * span / 200).max(1) + 3;
        writes.push(single_row(id, 100 + k % 3, 2, false));
        model.insert(id, 100 + k % 3);
    }
    for k in 0..40_i64 {
        let id = -span + k * (2 * span / 40).max(1) + 1;
        writes.push(single_row(id, 0, 2, true));
        model.remove(&id);
    }
    for id in [-span - 9, span + 4, span + 11] {
        writes.push(single_row(id, 101, 2, false));
        model.insert(id, 101);
    }
    table.ingest(writes).expect("memtable writes");
    (Fixture::new(&schema, &[1], table, directory), model)
}

fn check_single_key(fixture: &Fixture, model: &BTreeMap<i64, i64>) {
    let all = model
        .iter()
        .map(|(id, grp)| vec![Value::Int64(*id), Value::Int64(*grp)])
        .collect::<Vec<_>>();
    for sql in [
        "SELECT id, grp FROM t ORDER BY id",
        "SELECT id, grp FROM t ORDER BY id ASC",
    ] {
        assert!(!sorts(&fixture.plan(sql)), "{sql} sorts");
        assert_eq!(fixture.run(sql), all, "{sql}");
    }
    assert!(sorts(
        &fixture.plan("SELECT id, grp FROM t ORDER BY id + 0")
    ));
    assert_eq!(fixture.run("SELECT id, grp FROM t ORDER BY id + 0"), all);

    // A filter keeps the order of what it passes.
    let filtered = "SELECT id, grp FROM t WHERE grp = 101 OR id % 5 = 2 ORDER BY id";
    assert!(!sorts(&fixture.plan(filtered)));
    assert_eq!(
        fixture.run(filtered),
        fixture.run("SELECT id, grp FROM t WHERE grp = 101 OR id % 5 = 2 ORDER BY id + 0")
    );

    // A key the select list does not show is the sort's own column, and
    // leaves with the sort.
    let hidden = "SELECT grp, note FROM t ORDER BY id";
    assert!(!sorts(&fixture.plan(hidden)));
    let hidden_rows = fixture.run(hidden);
    assert_eq!(hidden_rows.first().map(Vec::len), Some(2));
    assert_eq!(
        hidden_rows,
        fixture.run("SELECT grp, note FROM t ORDER BY id + 0")
    );

    // A limit stops the scan after the first rows in key order.
    let limited = "SELECT id, grp FROM t WHERE id > -3 ORDER BY id LIMIT 7 OFFSET 2";
    assert!(!sorts(&fixture.plan(limited)));
    assert_eq!(
        fixture.run(limited),
        model
            .range(-2..)
            .skip(2)
            .take(7)
            .map(|(id, grp)| vec![Value::Int64(*id), Value::Int64(*grp)])
            .collect::<Vec<_>>()
    );

    // Orders no scan yields still sort.
    for sql in [
        "SELECT id, grp FROM t ORDER BY id DESC",
        "SELECT id, grp FROM t ORDER BY grp, id",
        "SELECT id, grp FROM t ORDER BY note",
        "SELECT id, grp FROM t ORDER BY id DESC LIMIT 3",
    ] {
        assert!(sorts(&fixture.plan(sql)), "{sql} does not sort");
    }
    assert_eq!(
        fixture.run("SELECT id, grp FROM t ORDER BY id DESC"),
        all.iter().rev().cloned().collect::<Vec<_>>()
    );
}

#[test]
fn a_large_table_is_scanned_in_key_order_through_its_segments_and_memtable() {
    let (fixture, model) = single_key_fixture(40_000);
    check_single_key(&fixture, &model);
}

#[test]
fn a_small_table_is_scanned_in_key_order_through_the_row_path() {
    let (fixture, model) = single_key_fixture(900);
    check_single_key(&fixture, &model);
}

#[test]
fn a_composite_key_orders_by_its_leading_columns() {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "a", DataType::Int64, false),
            Column::new(2, "b", DataType::UInt64, false),
            Column::new(3, "v", DataType::Int64, true),
            Column::new(4, "tag", DataType::Utf8, true),
        ],
    )
    .expect("schema");
    let row = |a: i64, b: u64, v: i64, version: u64, deleted: bool| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::Int64(a), KeyPart::UInt64(b)]).expect("key"),
            vec![
                Value::Int64(a),
                Value::UInt64(b),
                Value::Int64(v),
                Value::Utf8("x".to_owned()),
            ],
            version,
            deleted,
        )
    };
    let (directory, mut table) = open(&schema);
    let mut model = BTreeMap::new();
    let mut rows = Vec::new();
    for a in -150..=150_i64 {
        for b in 1..=300_u64 {
            let v = (a * 31 + i64::try_from(b).expect("b")) % 97;
            rows.push(row(a, b, v, 1, false));
            model.insert((a, b), v);
        }
    }
    table.bulk_ingest_snapshot(rows).expect("ingest");
    let mut writes = Vec::new();
    for k in 0..400_i64 {
        let (a, b) = (
            -150 + (k * 37) % 301,
            1 + u64::try_from((k * 53) % 300).expect("b"),
        );
        writes.push(row(a, b, 1_000 + k, 2, false));
        model.insert((a, b), 1_000 + k);
    }
    for k in 0..50_i64 {
        let (a, b) = (
            -150 + (k * 91) % 301,
            1 + u64::try_from((k * 17) % 300).expect("b"),
        );
        writes.push(row(a, b, 0, 2, true));
        model.remove(&(a, b));
    }
    for b in 301..=305 {
        writes.push(row(7, b, -7, 2, false));
        model.insert((7, b), -7);
    }
    table.ingest_cdc(writes).expect("cdc");
    let fixture = Fixture::new(&schema, &[1, 2], table, directory);
    let all = model
        .iter()
        .map(|((a, b), v)| vec![Value::Int64(*a), Value::UInt64(*b), Value::Int64(*v)])
        .collect::<Vec<_>>();

    for sql in [
        "SELECT a, b, v FROM t ORDER BY a, b",
        "SELECT a, b, v FROM t ORDER BY a",
    ] {
        assert!(!sorts(&fixture.plan(sql)), "{sql} sorts");
        assert_eq!(fixture.run(sql), all, "{sql}");
    }
    assert_eq!(
        fixture.run("SELECT a, b, v FROM t ORDER BY a + 0, b + 0"),
        all
    );
    let limited = "SELECT a, b, v FROM t WHERE a >= 7 ORDER BY a, b LIMIT 9";
    assert!(!sorts(&fixture.plan(limited)));
    assert_eq!(
        fixture.run(limited),
        all.iter()
            .filter(|row| matches!(row[0], Value::Int64(a) if a >= 7))
            .take(9)
            .cloned()
            .collect::<Vec<_>>()
    );
    // The second key column alone, or out of turn, is not the scan's order.
    for sql in [
        "SELECT a, b, v FROM t ORDER BY b",
        "SELECT a, b, v FROM t ORDER BY b, a",
        "SELECT a, b, v FROM t ORDER BY a, b DESC",
        "SELECT a, b, v FROM t ORDER BY a, v",
    ] {
        assert!(sorts(&fixture.plan(sql)), "{sql} does not sort");
    }
}

#[test]
fn a_nullable_or_text_key_is_never_claimed() {
    for (data_type, nullable) in [(DataType::Int64, true), (DataType::Utf8, false)] {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", data_type, nullable),
                Column::new(2, "grp", DataType::Int64, true),
                Column::new(3, "note", DataType::Utf8, true),
            ],
        )
        .expect("schema");
        let (directory, table) = open(&schema);
        let fixture = Fixture::new(&schema, &[1], table, directory);
        assert!(
            sorts(&fixture.plan("SELECT id, grp FROM t ORDER BY id")),
            "{data_type:?} nullable={nullable} does not sort"
        );
    }
}

/// Shapes around a keyed scan answer as the same query forced through a
/// sort, whether or not the planner found the order proven.
#[test]
fn shapes_over_a_keyed_scan_answer_as_a_sort_would() {
    let (fixture, _) = single_key_fixture(50);
    for sql in [
        "SELECT id FROM (SELECT id, grp FROM t WHERE grp > 1) AS d ORDER BY id",
        "SELECT x.id, y.id FROM t AS x JOIN t AS y ON y.grp = x.grp ORDER BY x.id, y.id",
        "SELECT id, (SELECT y.grp FROM t AS y WHERE y.id > t.id ORDER BY y.id LIMIT 1) FROM t ORDER BY id",
        "SELECT id, (SELECT MAX(y.id) FROM t AS y WHERE y.id < t.id) FROM t ORDER BY id",
        "SELECT grp, COUNT(*) FROM t GROUP BY grp ORDER BY grp",
        "SELECT id, grp FROM t WHERE id IN (SELECT id FROM t WHERE grp = 3) ORDER BY id",
    ] {
        assert_eq!(
            fixture.run(sql),
            fixture.run(&sql.replacen("ORDER BY ", "ORDER BY 0 + ", 1)),
            "{sql}"
        );
    }
}
