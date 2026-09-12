//! What a join pays in `Value`s, per key kind.
//!
//! `#[ignore]`: this measures rather than asserts, and is the instrument for
//! the typed-key work. Run it with
//! `cargo test --profile recovery -p pintail-exec --test join_key_value_cost
//! -- --ignored --nocapture`.
//!
//! The fused join-aggregate already reads integer keys straight from the
//! packed column. The general probe does not: every call site normalizes
//! `key.evaluate(batch, row)`, which is one `Value` per row per side.
//!
//! The `values` column is reported but reads zero for every shape here, and
//! that is not a mistake in the measurement: `values_materialized` counts a
//! whole column being turned into `Value`s, which is a different thing from
//! a key expression building one `Value` per row. Nothing counts the
//! second, so the milliseconds are the instrument. The column stays because
//! a shape that starts materializing whole columns is worth seeing.
//!
//! Baseline on the build host, 200,000 probe rows against 20,000 build
//! rows: integer key 16ms joined and 8.7ms grouped, text key 130ms joined
//! and 100ms grouped, against a 0.8ms scan floor. Integer keys are already
//! typed; the eight- to elevenfold gap is text, at roughly 570ns a row,
//! which is a `String` allocation and a collation pass per row.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider, take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const BUILD_ROWS: u64 = 20_000;
const PROBE_ROWS: u64 = 200_000;

fn build_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn probe_schema() -> TableSchema {
    TableSchema::new(
        2,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "ref_id", DataType::UInt64, false),
            Column::new(3, "ref_label", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn label(id: u64) -> String {
    format!("label-{:06}", id % BUILD_ROWS)
}

struct Fixture {
    _directory: tempfile::TempDir,
    build: TableStore,
    probe: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut build = TableStore::open(
            directory.path().join("build"),
            build_schema(),
            StoreOptions::default(),
        )
        .expect("build table");
        build
            .bulk_ingest_snapshot(
                (0..BUILD_ROWS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![Value::UInt64(id), Value::Utf8(label(id))],
                            id + 1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("build rows");
        let mut probe = TableStore::open(
            directory.path().join("probe"),
            probe_schema(),
            StoreOptions::default(),
        )
        .expect("probe table");
        probe
            .bulk_ingest_snapshot(
                (0..PROBE_ROWS)
                    .map(|id| {
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            vec![
                                Value::UInt64(id),
                                Value::UInt64(id % BUILD_ROWS),
                                Value::Utf8(label(id)),
                                Value::Int64(i64::try_from(id % 97).expect("small")),
                            ],
                            id + 1,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("probe rows");
        let dimension = TableEntry::new(
            TableId::new(1),
            "dimension",
            build_schema(),
            TableStatistics::with_row_count(BUILD_ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let fact = TableEntry::new(
            TableId::new(2),
            "fact",
            probe_schema(),
            TableStatistics::with_row_count(PROBE_ROWS),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        Self {
            _directory: directory,
            build,
            probe,
            catalog: CatalogSnapshot::new([DatabaseEntry::new(
                DatabaseId::new(1),
                "app",
                [dimension, fact],
            )
            .expect("database")])
            .expect("catalog"),
        }
    }

    /// `sql`'s row count, the `Value`s materialized answering it, and how
    /// long it took.
    fn measure(&self, sql: &str) -> (usize, u64, std::time::Duration) {
        let build = self.build.snapshot();
        let probe = self.probe.snapshot();
        let provider = SnapshotScanProvider::new([
            (DatabaseId::new(1), TableId::new(1), &build),
            (DatabaseId::new(1), TableId::new(2), &probe),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let _ = take_exec_counters();
        let started = std::time::Instant::now();
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = 0;
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            rows += batch.selection().selected_rows().count();
        }
        let elapsed = started.elapsed();
        (rows, take_exec_counters().values_materialized, elapsed)
    }
}

#[test]
#[ignore = "measurement, not an assertion"]
fn what_each_join_shape_pays_in_values() {
    let fixture = Fixture::new();
    let cases = [
        (
            "integer key, aggregated (the fused path)",
            "SELECT COUNT(*) FROM fact JOIN dimension ON fact.ref_id = dimension.id",
        ),
        (
            "integer key, rows out",
            "SELECT fact.amount FROM fact JOIN dimension ON fact.ref_id = dimension.id",
        ),
        (
            "text key, aggregated",
            "SELECT COUNT(*) FROM fact JOIN dimension ON fact.ref_label = dimension.label",
        ),
        (
            "text key, rows out",
            "SELECT fact.amount FROM fact JOIN dimension ON fact.ref_label = dimension.label",
        ),
        (
            "integer group by",
            "SELECT ref_id, COUNT(*) FROM fact GROUP BY ref_id",
        ),
        (
            "text group by",
            "SELECT ref_label, COUNT(*) FROM fact GROUP BY ref_label",
        ),
        (
            "scan only, for the floor",
            "SELECT COUNT(*) FROM fact WHERE amount > 0",
        ),
    ];
    println!(
        "\n{:<44} {:>9} {:>14} {:>12} {:>10}",
        "shape", "rows", "values", "per row", "ms"
    );
    for (name, sql) in cases {
        let (rows, values, elapsed) = fixture.measure(sql);
        #[allow(clippy::cast_precision_loss)]
        let per_row = if rows == 0 {
            0.0
        } else {
            values as f64 / rows as f64
        };
        println!(
            "{name:<44} {rows:>9} {values:>14} {per_row:>12.2} {:>10.1}",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    println!();
}
