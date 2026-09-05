//! Deterministic executor workload for Callgrind and PGO training. No source server.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 4096;
const CASES: [(&str, &str); 4] = [
    (
        "filter",
        "SELECT id,n FROM samples WHERE n > 50 ORDER BY id",
    ),
    (
        "aggregate",
        "SELECT bucket,SUM(n),COUNT(*) FROM samples GROUP BY bucket ORDER BY bucket",
    ),
    (
        "join",
        "SELECT a.id,a.n+b.n FROM samples a JOIN samples b ON a.id=b.id WHERE a.id < 128 ORDER BY a.id",
    ),
    (
        "sort",
        "SELECT id,n FROM samples ORDER BY n DESC,id LIMIT 25",
    ),
];

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "all".to_owned());
    assert!(
        name == "all" || CASES.iter().any(|(case, _)| *case == name),
        "unknown workload"
    );
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "n", DataType::Int64, false),
            Column::new(3, "bucket", DataType::UInt64, false),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            (0..ROWS)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Int64(i64::try_from(id % 97).expect("small")),
                            Value::UInt64(id % 16),
                        ],
                        id + 1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let entry = TableEntry::new(
        TableId::new(1),
        "samples",
        schema,
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let snapshot = table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    for (case, sql) in CASES {
        if name != "all" && name != case {
            continue;
        }
        let actual = measured(sql, &catalog, &provider);
        assert_eq!(actual, expected(case), "workload {case} changed its answer");
        println!("{case}: OK rows={}", actual.len());
    }
}

// Callgrind toggles collection on entry/exit of this function, excluding setup
// and the independent result oracle. Keep it non-inlined in release/PGO builds.
#[inline(never)]
fn measured(
    sql: &str,
    catalog: &CatalogSnapshot,
    provider: &SnapshotScanProvider<'_>,
) -> Vec<Vec<String>> {
    let parsed = parse_statement(sql).expect("parse");
    let bound = Binder::new(catalog, Some("app"))
        .bind(&parsed)
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(plan, provider, 64 * 1024 * 1024, Collation::default()).expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|c| match c.value(row).expect("value") {
                        Value::UInt64(n) => n.to_string(),
                        Value::Int64(n) => n.to_string(),
                        Value::Utf8(n) => n.clone(),
                        other => panic!("unexpected workload value: {other:?}"),
                    })
                    .collect(),
            );
        }
    }
    rows
}

fn expected(case: &str) -> Vec<Vec<String>> {
    match case {
        "filter" => (0..ROWS)
            .filter(|id| id % 97 > 50)
            .map(|id| vec![id.to_string(), (id % 97).to_string()])
            .collect(),
        "aggregate" => (0..16)
            .map(|bucket| {
                let ids: Vec<_> = (0..ROWS).filter(|id| id % 16 == bucket).collect();
                vec![
                    bucket.to_string(),
                    ids.iter().map(|id| id % 97).sum::<u64>().to_string(),
                    ids.len().to_string(),
                ]
            })
            .collect(),
        "join" => (0..128)
            .map(|id| vec![id.to_string(), (2 * (id % 97)).to_string()])
            .collect(),
        "sort" => {
            let mut ids: Vec<_> = (0..ROWS).collect();
            ids.sort_by_key(|id| (std::cmp::Reverse(id % 97), *id));
            ids.into_iter()
                .take(25)
                .map(|id| vec![id.to_string(), (id % 97).to_string()])
                .collect()
        }
        _ => unreachable!("validated workload"),
    }
}
