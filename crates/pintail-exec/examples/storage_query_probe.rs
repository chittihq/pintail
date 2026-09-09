//! Full parse/bind/plan/execute measurements on `storage_scan_probe`'s fixture.
//! Set `PINTAIL_DISABLE_SETTLED_MEMO=1`; pass its data directory and row count.
//! Both binaries use identical files. Every iteration must decode blocks.
//! `PINTAIL_PROBE_ITERATIONS` sets measured iterations after two warmups.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    collation::Collation,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore, WalSync};
use pintail_types::{Column, DataType, TableSchema, Value};
use std::{path::PathBuf, time::Instant};

#[allow(clippy::too_many_lines)]
fn main() {
    assert!(
        std::env::var_os("PINTAIL_DISABLE_SETTLED_MEMO").is_some(),
        "set PINTAIL_DISABLE_SETTLED_MEMO=1 to measure execution rather than memo hits"
    );
    let mut args = std::env::args().skip(1);
    let directory = PathBuf::from(args.next().expect("data directory"));
    let rows: u64 = args.next().map_or(524_288, |v| v.parse().expect("rows"));
    assert!(rows > 0, "row count must be positive");
    let iterations: usize = std::env::var("PINTAIL_PROBE_ITERATIONS")
        .map_or(7, |value| value.parse().expect("iterations"));
    assert!(iterations > 0, "at least one measured iteration");
    let schema = TableSchema::new(
        1,
        (1..=24)
            .map(|id| {
                Column::new(
                    id,
                    format!("field_{id}"),
                    if id == 24 {
                        DataType::Utf8
                    } else {
                        DataType::UInt64
                    },
                    false,
                )
            })
            .collect(),
    )
    .expect("schema");
    let table = TableStore::open(
        &directory,
        schema.clone(),
        StoreOptions {
            wal_sync: WalSync::Off,
            background_compaction: false,
            ..StoreOptions::default()
        },
    )
    .expect("table");
    let snapshot = table.snapshot();
    assert_eq!(
        snapshot.physical_row_upper_bound(),
        rows,
        "fixture row count"
    );
    let db = DatabaseId::new(1);
    let id = TableId::new(1);
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        db,
        "lab",
        [
            TableEntry::new(id, "sample", schema, TableStatistics::with_row_count(rows))
                .expect("table entry"),
        ],
    )
    .expect("database")])
    .expect("catalog");
    let wide_expression = (1..24)
        .map(|id| format!("field_{id}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let queries = [
        (
            "numeric-filter",
            "SELECT COUNT(*) FROM sample WHERE field_23 > 1000000".to_owned(),
            (rows.saturating_sub((1_000_000 - 7) / 23 + 1)).to_string(),
        ),
        (
            "text-filter",
            "SELECT COUNT(*) FROM sample WHERE field_24 = 'label-0'".to_owned(),
            rows.div_ceil(8).to_string(),
        ),
        (
            "text-all",
            "SELECT COUNT(*) FROM sample WHERE field_24 <> 'missing'".to_owned(),
            rows.to_string(),
        ),
        (
            "wide-expression",
            format!("SELECT SUM({wide_expression}) FROM sample WHERE field_24 <> 'missing'"),
            (u128::from(rows) * (u128::from(rows - 1) * 276 / 2 + 161)).to_string(),
        ),
    ];
    for (label, sql, expected) in queries {
        let mut timings = Vec::new();
        for round in 0..iterations + 2 {
            let provider = SnapshotScanProvider::new([(db, id, &snapshot)]).expect("provider");
            let began = Instant::now();
            let statement = parse_statement(&sql).expect("parse");
            let bound = Binder::new(&catalog, Some("lab"))
                .bind(&statement)
                .expect("bind");
            let physical = PhysicalPlanner::plan(
                Optimizer::optimize(LogicalPlanner::plan(bound)),
                Collation::default(),
            )
            .expect("plan");
            let mut execution =
                Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                    .expect("execute");
            let mut result = Vec::new();
            while let Some(batch) = execution.next_batch().expect("batch") {
                for row in batch.selection().selected_rows() {
                    result.push(
                        match batch
                            .column(0)
                            .and_then(|column| column.value(row))
                            .expect("value")
                        {
                            Value::UInt64(v) => v.to_string(),
                            Value::Int64(v) => v.to_string(),
                            Value::Utf8(v) => v.clone(),
                            other => format!("{other:?}"),
                        },
                    );
                }
            }
            let ms = began.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                result.as_slice(),
                std::slice::from_ref(&expected),
                "{label}"
            );
            assert!(
                provider
                    .scan_stats(db, id)
                    .is_some_and(|stats| stats.blocks_decoded > 0),
                "{label} must execute a physical scan"
            );
            if round > 1 {
                timings.push(ms);
            }
        }
        timings.sort_by(f64::total_cmp);
        println!(
            "{label}: median_ms={:.3} min_ms={:.3} result={expected}",
            timings[timings.len() / 2],
            timings[0]
        );
    }
}
