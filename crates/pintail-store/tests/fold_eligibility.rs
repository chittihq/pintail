//! When the per-segment fold can serve a query at all. Ignored: a
//! measurement of eligibility, not a gate.
//!
//! e78 measured a grouped aggregate served from per-segment partials at
//! seventy-three times the scan. That number is only worth building
//! against if the fold engages on the tables that matter, and the fold's
//! precondition is stricter than "the segments are immutable": every
//! memtable row must be an insert ABOVE the segment key space. An update
//! to a row a segment already holds is not that.

use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "status", DataType::Utf8, false),
            Column::new(3, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn row(id: u64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Utf8(format!("status-{}", id % 5)),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        version,
        false,
    )
}

/// One memtable shape to try, and what to call it in the table.
type Case = (&'static str, Box<dyn Fn(&mut TableStore)>);

fn store_with(live: impl Fn(&mut TableStore)) -> (tempfile::TempDir, TableStore) {
    let directory = tempfile::tempdir().expect("directory");
    let mut store =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("store");
    store
        .bulk_ingest_snapshot((1..=100_000).map(|id| row(id, 1)).collect())
        .expect("base");
    live(&mut store);
    (directory, store)
}

#[test]
#[ignore = "an eligibility measurement, not a gate"]
fn which_memtable_shapes_the_fold_can_serve() {
    let cases: [Case; 5] = [
        ("nothing in the memtable", Box::new(|_: &mut TableStore| {})),
        (
            "one insert above the segment",
            Box::new(|store: &mut TableStore| {
                store.ingest_cdc(vec![row(100_001, 2)]).expect("insert");
            }),
        ),
        (
            "fifty thousand inserts above the segment",
            Box::new(|store: &mut TableStore| {
                store
                    .ingest_cdc((100_001..=150_000).map(|id| row(id, 2)).collect())
                    .expect("inserts");
            }),
        ),
        (
            "ONE update of a row the segment holds",
            Box::new(|store: &mut TableStore| {
                store.ingest_cdc(vec![row(50_000, 2)]).expect("update");
            }),
        ),
        (
            "one delete of a row the segment holds",
            Box::new(|store: &mut TableStore| {
                let mut deleted = row(50_000, 2);
                deleted = StoredRow::new(deleted.key().clone(), deleted.values().to_vec(), 2, true);
                store.ingest_cdc(vec![deleted]).expect("delete");
            }),
        ),
    ];
    println!();
    println!("100,000 rows in one segment; can the per-segment fold serve it?");
    for (label, live) in cases {
        let (_directory, store) = store_with(live);
        let snapshot = store.snapshot();
        let eligible = snapshot.sma_fold_state().is_some();
        println!("  {label:>44} : {}", if eligible { "yes" } else { "NO" });
    }
}
