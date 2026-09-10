//! A cyclic join graph must not expand two dimensions before joining the facts.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn number(value: u64) -> Value {
    Value::UInt64(value)
}

#[test]
#[allow(clippy::too_many_lines)] // fixture, independent oracle, and profile assertion
fn cyclic_join_avoids_dimension_fanout_and_preserves_nullable_matches() {
    let directory = tempfile::tempdir().expect("directory");
    let definitions = [
        (
            "hubs",
            vec!["id", "grp"],
            (1..=100)
                .map(|id| vec![number(id), number(id % 5)])
                .collect::<Vec<_>>(),
        ),
        (
            "accounts",
            vec!["id", "grp"],
            (1..=1_000)
                .map(|id| {
                    vec![
                        number(id),
                        if id % 17 == 0 {
                            Value::Null
                        } else {
                            number(id % 5)
                        },
                    ]
                })
                .collect(),
        ),
        (
            "records",
            vec!["id", "account_id"],
            (1..=1_000).map(|id| vec![number(id), number(id)]).collect(),
        ),
        (
            "samples",
            vec!["id", "record_id", "hub_id", "amount"],
            (1..=2_000)
                .map(|id| {
                    vec![
                        number(id),
                        number((id - 1) % 1_000 + 1),
                        if id % 13 == 0 {
                            Value::Null
                        } else {
                            number((id * 7) % 100 + 1)
                        },
                        if id % 11 == 0 {
                            Value::Null
                        } else {
                            number(id % 19)
                        },
                    ]
                })
                .collect(),
        ),
    ];
    let mut stores = Vec::new();
    let mut tables = Vec::new();
    for (index, (name, columns, rows)) in definitions.into_iter().enumerate() {
        let schema = TableSchema::new(
            1,
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    Column::new(
                        u32::try_from(i + 1).expect("column"),
                        *name,
                        DataType::UInt64,
                        i != 0,
                    )
                })
                .collect(),
        )
        .expect("schema");
        let count = u64::try_from(rows.len()).expect("rows");
        let mut store = TableStore::open(
            directory.path().join(name),
            schema.clone(),
            StoreOptions::default(),
        )
        .expect("store");
        store
            .bulk_ingest_snapshot(
                rows.into_iter()
                    .enumerate()
                    .map(|(i, values)| {
                        let id = u64::try_from(i + 1).expect("id");
                        StoredRow::new(
                            PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                            values,
                            id,
                            false,
                        )
                    })
                    .collect(),
            )
            .expect("ingest");
        tables.push(
            TableEntry::new(
                TableId::new(u64::try_from(index + 1).expect("table")),
                name,
                schema,
                TableStatistics::with_row_count(count),
            )
            .expect("entry")
            .with_key_columns([1])
            .expect("key"),
        );
        stores.push(store);
    }
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", tables).expect("database")
    ])
    .expect("catalog");
    let snapshots: Vec<_> = stores.iter().map(TableStore::snapshot).collect();
    let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
        (
            DatabaseId::new(1),
            TableId::new(u64::try_from(i + 1).expect("table")),
            s,
        )
    }))
    .expect("provider");
    let sql = "SELECT a.grp, COUNT(*), SUM(s.amount) FROM hubs h, accounts a, records r, samples s WHERE h.grp = a.grp AND a.id = r.account_id AND r.id = s.record_id AND h.id = s.hub_id GROUP BY a.grp ORDER BY a.grp";
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start_profiled(plan, &provider, 64 << 20, None, Collation::default())
            .expect("execution");
    let mut actual = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        for row in batch.selection().selected_rows() {
            actual.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value(row).expect("value") {
                        Value::UInt64(value) => value.to_string(),
                        Value::Int64(value) => value.to_string(),
                        Value::Utf8(value) => value.clone(),
                        other => panic!("unexpected aggregate value: {other:?}"),
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    let mut groups = std::collections::BTreeMap::<u64, (u64, u64)>::new();
    for id in 1..=2_000_u64 {
        let account = (id - 1) % 1_000 + 1;
        let hub = (id * 7) % 100 + 1;
        if account % 17 != 0 && id % 13 != 0 && account % 5 == hub % 5 {
            let entry = groups.entry(account % 5).or_default();
            entry.0 += 1;
            if id % 11 != 0 {
                entry.1 += id % 19;
            }
        }
    }
    let expected: Vec<_> = groups
        .into_iter()
        .map(|(group, (count, sum))| vec![group.to_string(), count.to_string(), sum.to_string()])
        .collect();
    assert_eq!(actual, expected);
    let profile = execution.profile().expect("profile");
    assert!(
        profile.operators.iter().all(|op| op.rows <= 2_000),
        "the fact inputs bound useful join work; avoid dimension fanout:\n{}",
        profile.render()
    );
}
