//! Adversarial store API probe; does not establish reachability through CDC.
use core_engine_100::{
    Row,
    live::{schema, stored},
    merge::Version,
};
use pintail_store::{StoreOptions, TableStore};
fn main() {
    for compact in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut table = TableStore::open(
            dir.path(),
            schema(),
            StoreOptions {
                compaction_fan_in: 2,
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let row = Row {
            id: 1,
            key: 1,
            low: 1,
            value: 9,
            valid: true,
        };
        let base = Version {
            row,
            version: 1,
            dead: false,
        };
        table.bulk_ingest_snapshot(vec![stored(base)]).unwrap();
        table
            .ingest_cdc(vec![stored(Version {
                version: 2,
                dead: true,
                ..base
            })])
            .unwrap();
        table.flush().unwrap();
        assert!(table.snapshot().scan().unwrap().is_empty());
        if compact {
            table.compact().unwrap();
        }
        table.ingest_cdc(vec![stored(base)]).unwrap();
        let rows = table.snapshot().scan().unwrap();
        println!(
            "{}",
            serde_json::json!({"full_compaction":compact,"rows_after_old_replay":rows.len(),"cdc_reachability":"not established"})
        );
    }
}
