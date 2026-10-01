//! A fixed, invented dataset and the query shapes counted over it.
//!
//! Two fixtures, both pure functions of the row id: a commerce one (orders,
//! users, products, the shapes of the timed benchmark) and an event table
//! whose time rises with its key (windowed aggregates, read as a `DATETIME`
//! and as a `TIMESTAMP` in a session zone). Every case is one statement run
//! through parse, bind, plan and execute over a real `TableStore`, with no
//! server and no source database.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

pub const ORDERS: u64 = 400_000;
pub const USERS: u64 = 20_000;
pub const PRODUCTS: u64 = 2_000;
pub const EVENTS: u64 = 400_000;

/// The event table's row count: `EVENTS`, or `PINTAIL_SUITE_EVENTS` for a
/// native run that wants statements long enough to profile. The counted
/// runs never see the variable.
#[must_use]
pub fn event_rows() -> u64 {
    std::env::var("PINTAIL_SUITE_EVENTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(EVENTS)
}
/// Seconds between consecutive events: the table covers about 92 days.
pub const STEP: u64 = 20;
/// 2024-01-01 00:00:00 UTC.
const EPOCH: u64 = 1_704_067_200;
const DAY: u64 = 86_400;
/// Days from 1970-01-01 to 2020-01-01.
const ORDER_EPOCH_DAYS: u64 = 18_262;
/// One memory ceiling for every case, far above what any of them reserves.
const QUERY_BYTES: usize = 1 << 31;

pub const STATUSES: [&str; 5] = ["pending", "processing", "shipped", "delivered", "cancelled"];
pub const REGIONS: [&str; 8] = [
    "us-east", "us-west", "eu-west", "eu-east", "ap-south", "ap-east", "sa-east", "af-south",
];
pub const CATEGORIES: [&str; 10] = [
    "electronics",
    "clothing",
    "food",
    "books",
    "toys",
    "sports",
    "home",
    "garden",
    "auto",
    "health",
];
pub const CHANNELS: [&str; 5] = ["email", "kiosk", "phone", "store", "web"];

/// Which fixture a case reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tables {
    Commerce,
    Events,
}

/// One measured statement.
pub struct Case {
    pub name: &'static str,
    pub tables: Tables,
    /// The session time zone the statement runs in, when not UTC.
    pub zone: Option<&'static str>,
    pub sql: &'static str,
}

const fn commerce(name: &'static str, sql: &'static str) -> Case {
    Case {
        name,
        tables: Tables::Commerce,
        zone: None,
        sql,
    }
}

const fn events(name: &'static str, zone: Option<&'static str>, sql: &'static str) -> Case {
    Case {
        name,
        tables: Tables::Events,
        zone,
        sql,
    }
}

/// The 30-day window sits in the middle of the event table; the open-ended
/// one takes its last week.
pub const CASES: &[Case] = &[
    commerce("q1_count", "SELECT COUNT(*) AS cnt FROM orders"),
    commerce(
        "q2_filtered_count",
        "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'shipped'",
    ),
    commerce(
        "q3_group_status",
        "SELECT status, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt FROM orders \
         GROUP BY status ORDER BY cnt DESC, status",
    ),
    commerce(
        "q4_region_status",
        "SELECT region, status, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total \
         FROM orders GROUP BY region, status ORDER BY total DESC, region, status LIMIT 20",
    ),
    commerce(
        "q5_monthly",
        "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
         ROUND(SUM(total_amount), 2) AS revenue FROM orders \
         WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' \
         GROUP BY yr, mo ORDER BY yr, mo",
    ),
    commerce(
        "q6_top_spenders",
        "SELECT user_id, COUNT(*) AS order_count, ROUND(SUM(total_amount), 2) AS total_spent \
         FROM orders GROUP BY user_id ORDER BY total_spent DESC, user_id LIMIT 10",
    ),
    commerce(
        "q7_regional",
        "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
         ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
         ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
         FROM orders WHERE order_date BETWEEN '2022-01-01' AND '2023-12-31' \
         GROUP BY region ORDER BY total DESC",
    ),
    commerce(
        "q8_join_users",
        "SELECT u.region, COUNT(*) AS cnt, ROUND(SUM(o.total_amount), 2) AS total \
         FROM orders o JOIN users u ON o.user_id = u.id GROUP BY u.region ORDER BY total DESC",
    ),
    commerce(
        "n1_filtered_count_bounded",
        "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'delivered' AND id >= 3",
    ),
    commerce(
        "n2_group_region",
        "SELECT region, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt FROM orders \
         WHERE id >= 2 GROUP BY region ORDER BY cnt DESC, region",
    ),
    commerce(
        "n3_monthly_other_year",
        "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
         ROUND(SUM(total_amount), 2) AS revenue FROM orders \
         WHERE order_date >= '2020-07-01' AND order_date < '2021-07-01' \
         GROUP BY yr, mo ORDER BY yr, mo",
    ),
    commerce(
        "n4_regional_other_range",
        "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
         ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
         ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
         FROM orders WHERE order_date BETWEEN '2020-01-01' AND '2022-12-31' \
         GROUP BY region ORDER BY total DESC",
    ),
    commerce(
        "text_filter_two_columns",
        "SELECT COUNT(*) AS cnt FROM orders WHERE status <> 'cancelled' AND region = 'eu-west'",
    ),
    commerce(
        "many_groups",
        "SELECT user_id, quantity, COUNT(*) AS cnt, SUM(total_amount) AS total FROM orders \
         GROUP BY user_id, quantity ORDER BY cnt DESC, user_id, quantity LIMIT 10",
    ),
    commerce(
        "star_join",
        "SELECT u.region, p.category, COUNT(*) AS cnt, SUM(o.total_amount) AS total \
         FROM orders o JOIN users u ON o.user_id = u.id JOIN products p ON o.product_id = p.id \
         GROUP BY u.region, p.category ORDER BY u.region, p.category",
    ),
    commerce(
        "count_distinct",
        "SELECT COUNT(DISTINCT user_id) AS users, COUNT(DISTINCT product_id) AS products \
         FROM orders",
    ),
    commerce(
        "order_limit",
        "SELECT id, user_id, total_amount FROM orders ORDER BY total_amount DESC, id LIMIT 25",
    ),
    commerce(
        "correlated_scalar_page",
        "SELECT u.id, u.region, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS orders \
         FROM users u ORDER BY u.id LIMIT 50",
    ),
    events(
        "window_day",
        None,
        "SELECT DATE(occurred_at) AS k, COUNT(*), SUM(price) FROM events \
         WHERE occurred_at BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
    events(
        "window_hour",
        None,
        "SELECT HOUR(occurred_at) AS k, COUNT(*), SUM(price) FROM events \
         WHERE occurred_at BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_window_day",
        Some("+05:30"),
        "SELECT DATE(stamp) AS k, COUNT(*), SUM(price) FROM events \
         WHERE stamp BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_window_formatted_day",
        Some("+05:30"),
        "SELECT DATE_FORMAT(stamp, '%Y-%m-%d') AS k, COUNT(*), SUM(price) FROM events \
         WHERE stamp BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_window_hour",
        Some("-08:00"),
        "SELECT HOUR(stamp) AS k, COUNT(*), SUM(price) FROM events \
         WHERE stamp BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_since_day",
        Some("-08:00"),
        "SELECT DATE(stamp) AS k, COUNT(*), SUM(price) FROM events \
         WHERE stamp >= '2024-03-25 00:00:00' GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_all_days",
        Some("+05:30"),
        "SELECT DATE(stamp) AS k, COUNT(*), SUM(price) FROM events GROUP BY k ORDER BY k",
    ),
    events(
        "zoned_named_zone_hour",
        Some("America/New_York"),
        "SELECT HOUR(stamp) AS k, COUNT(*), SUM(price) FROM events \
         WHERE stamp BETWEEN '2024-02-01 00:00:00' AND '2024-03-02 00:00:00' \
         GROUP BY k ORDER BY k",
    ),
];

/// The case of that name.
///
/// # Panics
///
/// Panics when no case has the name.
#[must_use]
pub fn case(name: &str) -> &'static Case {
    CASES
        .iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("unknown case {name}"))
}

fn mix(id: u64) -> u64 {
    let mut value = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value ^= value >> 29;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^ (value >> 32)
}

fn pick<'a>(names: &[&'a str], value: u64) -> &'a str {
    names[usize::try_from(value % names.len() as u64).expect("small")]
}

#[must_use]
pub fn order_status(id: u64) -> &'static str {
    pick(&STATUSES, mix(id) >> 7)
}

fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

fn date(days: u64) -> String {
    let (year, month, day) = civil(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The event epoch plus `seconds`, as a date-time's text.
#[must_use]
pub fn timestamp(seconds: u64) -> String {
    let total = EPOCH + seconds;
    let within = total % DAY;
    format!(
        "{} {:02}:{:02}:{:02}",
        date(total / DAY),
        within / 3600,
        within / 60 % 60,
        within % 60
    )
}

fn cents(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        1,
        false,
    )
}

fn text(value: &str) -> Value {
    Value::Utf8(value.to_owned())
}

fn decimal(precision: u8) -> DataType {
    DataType::Decimal {
        precision,
        scale: 2,
    }
}

fn orders_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "user_id", DataType::UInt64, false),
            Column::new(3, "product_id", DataType::UInt64, false),
            Column::new(4, "quantity", DataType::UInt64, false),
            Column::new(5, "unit_price", decimal(10), false),
            Column::new(6, "total_amount", decimal(12), false),
            Column::new(7, "status", DataType::Utf8, false),
            Column::new(8, "region", DataType::Utf8, false),
            Column::new(9, "order_date", DataType::Date32, false),
        ],
    )
    .expect("orders schema")
}

fn order(id: u64) -> StoredRow {
    let hash = mix(id);
    let quantity = 1 + hash % 5;
    let unit = 500 + (id * 7919) % 50_000;
    stored(
        id,
        vec![
            Value::UInt64(id),
            Value::UInt64(1 + (hash >> 11) % USERS),
            Value::UInt64(1 + (hash >> 23) % PRODUCTS),
            Value::UInt64(quantity),
            text(&cents(unit)),
            text(&cents(unit * quantity)),
            text(order_status(id)),
            text(pick(&REGIONS, hash >> 37)),
            text(&date(ORDER_EPOCH_DAYS + (id * 17) % 1825)),
        ],
    )
}

fn users_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "region", DataType::Utf8, false),
        ],
    )
    .expect("users schema")
}

fn products_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "category", DataType::Utf8, false),
        ],
    )
    .expect("products schema")
}

fn events_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "occurred_at", DataType::DateTime64 { fsp: 0 }, false),
            Column::new(3, "price", decimal(10), false),
            Column::new(4, "channel", DataType::Utf8, false),
            Column::new(5, "amount", DataType::Int64, false),
            // The same instant as a source TIMESTAMP, stored UTC: a session
            // time zone moves what it reads as.
            Column::new(6, "stamp", DataType::DateTime64 { fsp: 0 }, false).with_timestamp(true),
        ],
    )
    .expect("events schema")
}

fn event(id: u64) -> StoredRow {
    let at = timestamp(id * STEP);
    stored(
        id,
        vec![
            Value::UInt64(id),
            text(&at),
            text(&cents(1 + (id * 7919) % 99_999)),
            text(pick(&CHANNELS, mix(id) >> 40)),
            Value::Int64(i64::try_from(id % 1_000).expect("small") - 300),
            text(&at),
        ],
    )
}

/// The stores of one fixture and the catalog that names them.
pub struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

impl Fixture {
    /// Loads the fixture's tables.
    ///
    /// # Panics
    ///
    /// Panics when a store cannot be written.
    #[must_use]
    pub fn build(tables: Tables) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let options = StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let plan: Vec<(&str, TableSchema, Vec<StoredRow>)> = match tables {
            Tables::Commerce => vec![
                ("orders", orders_schema(), (1..=ORDERS).map(order).collect()),
                (
                    "users",
                    users_schema(),
                    (1..=USERS)
                        .map(|id| {
                            stored(
                                id,
                                vec![
                                    Value::UInt64(id),
                                    text(&format!("user {id}")),
                                    text(pick(&REGIONS, id)),
                                ],
                            )
                        })
                        .collect(),
                ),
                (
                    "products",
                    products_schema(),
                    (1..=PRODUCTS)
                        .map(|id| {
                            stored(
                                id,
                                vec![
                                    Value::UInt64(id),
                                    text(&format!("product {id}")),
                                    text(pick(&CATEGORIES, id)),
                                ],
                            )
                        })
                        .collect(),
                ),
            ],
            Tables::Events => vec![(
                "events",
                events_schema(),
                (0..event_rows()).map(event).collect(),
            )],
        };
        let mut stores = Vec::new();
        let mut entries = Vec::new();
        for (index, (name, schema, rows)) in plan.into_iter().enumerate() {
            let count = rows.len() as u64;
            let mut store = TableStore::open(directory.path().join(name), schema.clone(), options)
                .expect("store");
            store.bulk_ingest_snapshot(rows).expect("ingest");
            entries.push(
                TableEntry::new(
                    TableId::new(index as u64 + 1),
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
        let catalog =
            CatalogSnapshot::new([
                DatabaseEntry::new(DatabaseId::new(1), "app", entries).expect("database")
            ])
            .expect("catalog");
        Self {
            _directory: directory,
            stores,
            catalog,
        }
    }

    /// Parses, binds, plans and executes the case, and renders its answer.
    ///
    /// # Panics
    ///
    /// Panics when the statement fails at any stage.
    #[must_use]
    pub fn run(&self, case: &Case) -> Vec<String> {
        let snapshots: Vec<_> = self.stores.iter().map(TableStore::snapshot).collect();
        let provider =
            SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(index, snapshot)| {
                (DatabaseId::new(1), TableId::new(index as u64 + 1), snapshot)
            }))
            .expect("provider");
        assert!(
            pintail_exec::set_session_time_zone(case.zone),
            "zone {:?}",
            case.zone
        );
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(case.sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {}: {error}", case.name));
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start(plan, &provider, QUERY_BYTES, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                let values: Vec<String> = batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Int64(value) => value.to_string(),
                        Value::UInt64(value) => value.to_string(),
                        Value::Utf8(text) => text,
                        Value::Null => "NULL".to_owned(),
                        Value::DecimalAverage(average) => average.label.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                rows.push(values.join("|"));
            }
        }
        assert!(pintail_exec::set_session_time_zone(None));
        rows
    }
}

/// What the generator says a case answers, for the cases simple enough to
/// compute directly; the rest are pinned by comparing answers between
/// builds.
#[must_use]
pub fn expected(case: &Case) -> Option<Vec<String>> {
    match case.name {
        "q1_count" => Some(vec![ORDERS.to_string()]),
        "q2_filtered_count" => Some(vec![
            (1..=ORDERS)
                .filter(|id| order_status(*id) == "shipped")
                .count()
                .to_string(),
        ]),
        "zoned_window_day" | "zoned_window_formatted_day" => {
            // 2024-02-01 and 2024-03-02 as the +05:30 session reads them.
            let offset = 19_800;
            let (from, to) = (31 * DAY - offset, 61 * DAY - offset);
            let mut days = std::collections::BTreeMap::<String, (u64, u64)>::new();
            for id in from.div_ceil(STEP)..=(to / STEP).min(event_rows() - 1) {
                let key = timestamp(id * STEP + offset)[..10].to_owned();
                let entry = days.entry(key).or_default();
                entry.0 += 1;
                entry.1 += 1 + (id * 7919) % 99_999;
            }
            Some(
                days.iter()
                    .map(|(day, (count, price))| format!("{day}|{count}|{}", cents(*price)))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A short stable digest of an answer, for comparing builds.
#[must_use]
pub fn digest(rows: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in rows
        .iter()
        .flat_map(|row| row.bytes().chain(std::iter::once(b'\n')))
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
