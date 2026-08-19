//! The parked #258 oracle case, replicated exactly: a mixed-signedness join
//! key (`orders.user_id` `Int64` against `events.id` `UInt64`) under a `general_ci`
//! grouped fold with two-pass-eligible aggregate lanes. The oracle showed
//! users 1 and 2's orders vanishing from every group while user 3's
//! survived - this pins whether they die in the JOIN or in the GROUPING.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const GENERAL_CI: &str = "utf8mb4_general_ci";
const AI_CI: &str = "utf8mb4_0900_ai_ci";

fn events_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false).with_collation(Some(AI_CI.to_owned())),
            Column::new(3, "tag", DataType::Utf8, false)
                .with_collation(Some(GENERAL_CI.to_owned())),
        ],
    )
    .expect("schema")
}

fn orders_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::Int64, false),
            Column::new(
                3,
                "total",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                false,
            ),
            Column::new(4, "placed_at", DataType::DateTime64 { fsp: 0 }, false),
        ],
    )
    .expect("schema")
}

const TAGS: [&str; 10] = [
    "red", "RED", "red ", "blue", "BLUE", "blue", "Green", "green", "RED", "Blue",
];

const ORDERS: [(u64, i64, &str, &str); 12] = [
    (1, 1, "10.50", "2024-01-15 10:00:00"),
    (2, 1, "198.82", "2024-02-29 12:34:56"),
    (3, 2, "0.01", "2024-03-01 00:00:00"),
    (4, 2, "99999999.99", "2024-03-08 06:30:00"),
    (5, 3, "12.35", "2024-06-15 18:00:00"),
    (6, 3, "50.00", "2024-07-04 09:15:00"),
    (7, 4, "7.00", "2024-11-01 01:30:00"),
    (8, 5, "100.00", "2025-01-01 00:00:00"),
    (9, 9, "25.25", "2025-02-01 12:00:00"),
    (10, 1, "0.00", "2025-02-28 23:59:59"),
    (11, 6, "33.33", "2025-03-01 08:00:00"),
    (12, 7, "64.00", "2025-04-01 16:45:00"),
];

#[allow(clippy::too_many_lines)]
fn run(sql: &str) -> Vec<Vec<String>> {
    let events_dir = tempfile::tempdir().expect("events dir");
    let mut events = TableStore::open(events_dir.path(), events_schema(), StoreOptions::default())
        .expect("open events");
    events
        .bulk_ingest_snapshot(
            (1..=10_u64)
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![
                            Value::UInt64(id),
                            Value::Utf8(format!("event-{id:02}")),
                            Value::Utf8(TAGS[usize::try_from(id).expect("small") - 1].to_owned()),
                        ],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest events");
    let orders_dir = tempfile::tempdir().expect("orders dir");
    let mut orders = TableStore::open(orders_dir.path(), orders_schema(), StoreOptions::default())
        .expect("open orders");
    orders
        .bulk_ingest_snapshot(
            ORDERS
                .iter()
                .map(|(id, user, total, placed)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            Value::Int64(*user),
                            Value::Utf8((*total).to_owned()),
                            Value::Utf8((*placed).to_owned()),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest orders");

    let events_snapshot = events.snapshot();
    let orders_snapshot = orders.snapshot();
    let database_id = DatabaseId::new(1);
    let events_id = TableId::new(1);
    let orders_id = TableId::new(2);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                events_id,
                "events",
                events_schema(),
                TableStatistics::with_row_count(10),
            )
            .expect("events entry")
            .with_key_columns([1])
            .expect("events key"),
            TableEntry::new(
                orders_id,
                "orders",
                orders_schema(),
                TableStatistics::with_row_count(12),
            )
            .expect("orders entry")
            .with_key_columns([1])
            .expect("orders key"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, events_id, &events_snapshot),
        (database_id, orders_id, &orders_snapshot),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .expect("bind");
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("execute") {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| {
                        column
                            .value(row)
                            .map_or("MISSING".to_owned(), |value| match value {
                                Value::Null => "NULL".to_owned(),
                                Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
                                Value::UInt64(number) => number.to_string(),
                                Value::Int64(number) => number.to_string(),
                                other => format!("{other:?}"),
                            })
                    })
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

#[test]
fn the_raw_join_keeps_every_matched_order() {
    // 11 orders match (user 8 has no orders; every other user exists).
    let rows =
        run("SELECT o.id, e.tag FROM events e JOIN orders o ON o.user_id = e.id ORDER BY o.id");
    let ids: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
    assert_eq!(
        ids,
        [
            "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12"
        ],
        "every order matches an existing event; none may vanish in the join"
    );
}

#[test]
fn the_grouped_fold_matches_mysql() {
    // MySQL 8.4's answer for the parked oracle case: red-fold spans users
    // 1, 2, 3 and 9 (tags red/RED/'red '/RED), so its MIN is order 1's
    // datetime and its MAX is order 4's total.
    let rows = run(
        "SELECT e.tag, MIN(o.placed_at), MAX(o.total) FROM events e \
         JOIN orders o ON o.user_id = e.id GROUP BY e.tag \
         ORDER BY MIN(o.placed_at), MAX(o.total)",
    );
    assert_eq!(
        rows,
        [
            ["red", "2024-01-15 10:00:00", "99999999.99"],
            ["blue", "2024-11-01 01:30:00", "100.00"],
            ["Green", "2025-04-01 16:45:00", "64.00"],
        ]
    );
}

#[test]
fn bisect_count_star_fold() {
    let rows = run(
        "SELECT e.tag, COUNT(*) FROM events e JOIN orders o ON o.user_id = e.id \
         GROUP BY e.tag ORDER BY e.tag",
    );
    assert_eq!(
        rows,
        [["blue", "3"], ["Green", "1"], ["red", "8"]],
        "general_ci orders blue < Green < red; red-fold spans users 1,2,3,9"
    );
}

#[test]
fn bisect_min_datetime_lane_only() {
    let rows = run(
        "SELECT e.tag, MIN(o.placed_at) FROM events e JOIN orders o ON o.user_id = e.id \
         GROUP BY e.tag ORDER BY e.tag",
    );
    assert_eq!(
        rows,
        [
            ["blue", "2024-11-01 01:30:00"],
            ["Green", "2025-04-01 16:45:00"],
            ["red", "2024-01-15 10:00:00"],
        ]
    );
}

#[test]
fn bisect_max_decimal_lane_only() {
    let rows = run(
        "SELECT e.tag, MAX(o.total) FROM events e JOIN orders o ON o.user_id = e.id \
         GROUP BY e.tag ORDER BY e.tag",
    );
    assert_eq!(
        rows,
        [
            ["blue", "100.00"],
            ["Green", "64.00"],
            ["red", "99999999.99"]
        ]
    );
}

#[test]
fn bisect_min_int_lane_only() {
    let rows = run(
        "SELECT e.tag, MIN(o.id) FROM events e JOIN orders o ON o.user_id = e.id \
         GROUP BY e.tag ORDER BY e.tag",
    );
    assert_eq!(rows, [["blue", "7"], ["Green", "12"], ["red", "1"]]);
}
