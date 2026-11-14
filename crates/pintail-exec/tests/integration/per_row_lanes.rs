//! Paths that used to pay a cost per row, pinned by the counters that say
//! which path ran, with answers computed here from the generated rows:
//! `SUM(DISTINCT)` of a DECIMAL column by packed units, `GROUP_CONCAT`
//! over many small groups, `TIME` functions over packed microseconds,
//! several JSON readers sharing one parse, and a spilled window reading
//! its rows back a batch at a time and ordering text by its weights.

use std::collections::{BTreeMap, BTreeSet};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecCounters, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 3_000;
const BUCKETS: u64 = 700;
const TAGS: [&str; 6] = ["amber", "Amber", "birch", "BIRCH", "cedar", "dune"];

fn mix(id: u64) -> u64 {
    let mut value = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value ^= value >> 29;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^ (value >> 32)
}

/// One generated reading.
struct Reading {
    id: u64,
    bucket: u64,
    /// Hundredths, NULL for some rows.
    amount: Option<i64>,
    tag: &'static str,
    /// Signed seconds, past a day either way.
    seconds: i64,
    n: u64,
    tags: u64,
}

fn reading(id: u64) -> Reading {
    let hash = mix(id);
    let seconds = i64::try_from((hash >> 9) % 400_000).expect("small");
    Reading {
        id,
        bucket: 1 + (hash >> 13) % BUCKETS,
        amount: (!(hash >> 3).is_multiple_of(10))
            .then(|| i64::try_from((hash >> 27) % 40).expect("small") * 25 - 300),
        tag: TAGS[usize::try_from((hash >> 41) % TAGS.len() as u64).expect("small")],
        seconds: if (hash >> 5).is_multiple_of(4) {
            -seconds
        } else {
            seconds
        },
        n: hash % 1_000,
        tags: (hash >> 7) % 4,
    }
}

fn cents(value: i64) -> String {
    format!(
        "{}{}.{:02}",
        if value < 0 { "-" } else { "" },
        value.abs() / 100,
        value.abs() % 100
    )
}

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "bucket", DataType::UInt64, false),
            Column::new(
                3,
                "amount",
                DataType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            Column::new(4, "tag", DataType::Utf8, false),
            Column::new(5, "elapsed", DataType::Time64 { fsp: 0 }, false),
            Column::new(6, "payload", DataType::Json, false),
        ],
    )
    .expect("schema")
}

fn stored(reading: &Reading) -> StoredRow {
    let seconds = reading.seconds.abs();
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(reading.id)]).expect("key"),
        vec![
            Value::UInt64(reading.id),
            Value::UInt64(reading.bucket),
            reading
                .amount
                .map_or(Value::Null, |amount| Value::Utf8(cents(amount))),
            Value::Utf8(reading.tag.to_owned()),
            Value::Utf8(format!(
                "{}{:02}:{:02}:{:02}",
                if reading.seconds < 0 { "-" } else { "" },
                seconds / 3_600,
                seconds / 60 % 60,
                seconds % 60
            )),
            Value::Utf8(format!(
                "{{\"n\": {}, \"tags\": [{}]}}",
                reading.n,
                (0..reading.tags)
                    .map(|tag| tag.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        ],
        1,
        false,
    )
}

/// The statement's rows and what its execution counted on this thread,
/// under a memory ceiling of `memory` bytes.
fn run(sql: &str, memory: usize) -> (Vec<Vec<String>>, ExecCounters) {
    let directory = tempfile::tempdir().expect("directory");
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema(), options).expect("table");
    table
        .bulk_ingest_snapshot((1..=ROWS).map(|id| stored(&reading(id))).collect())
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "readings",
        schema(),
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database")
    ])
    .expect("catalog");
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&parse_statement(sql).expect("parse"))
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = take_exec_counters();
    let mut execution =
        Execution::start(plan, &provider, memory, Collation::default()).expect("start");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| match column.value_owned(row).expect("value") {
                        Value::Null => "NULL".to_owned(),
                        Value::Utf8(text) => text,
                        Value::Int64(number) => number.to_string(),
                        Value::UInt64(number) => number.to_string(),
                        Value::DecimalAverage(average) => average.label.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    (rows, take_exec_counters())
}

const AMPLE: usize = 256 << 20;

#[test]
fn a_distinct_decimal_sum_keys_and_adds_packed_units() {
    let (rows, counters) = run(
        "SELECT bucket, SUM(DISTINCT amount), COUNT(DISTINCT amount) FROM readings \
         GROUP BY bucket ORDER BY bucket",
        AMPLE,
    );
    let mut expected = BTreeMap::<u64, BTreeSet<i64>>::new();
    for reading in (1..=ROWS).map(reading) {
        let amounts = expected.entry(reading.bucket).or_default();
        amounts.extend(reading.amount);
    }
    let expected = expected
        .into_iter()
        .map(|(bucket, amounts)| {
            vec![
                bucket.to_string(),
                // A group whose amounts are all NULL sums to NULL.
                if amounts.is_empty() {
                    "NULL".to_owned()
                } else {
                    cents(amounts.iter().sum())
                },
                amounts.len().to_string(),
            ]
        })
        .collect::<Vec<_>>();
    assert!(
        expected.iter().any(|row| row[1] == "NULL"),
        "an empty group"
    );
    assert_eq!(rows, expected);
    assert!(
        counters.distinct_units_summed > 0,
        "the sum read packed units: {counters:?}"
    );
}

#[test]
fn group_concat_over_many_small_groups_stays_in_memory_and_in_order() {
    let sql = "SELECT bucket, GROUP_CONCAT(tag ORDER BY id DESC SEPARATOR '|'), \
               GROUP_CONCAT(DISTINCT tag ORDER BY tag DESC, id), GROUP_CONCAT(amount) \
               FROM readings GROUP BY bucket ORDER BY bucket";
    let (rows, _) = run(sql, AMPLE);
    let mut expected = BTreeMap::<u64, Vec<Reading>>::new();
    for reading in (1..=ROWS).map(reading) {
        expected.entry(reading.bucket).or_default().push(reading);
    }
    assert_eq!(rows.len(), expected.len());
    for (row, (bucket, readings)) in rows.iter().zip(&expected) {
        assert_eq!(row[0], bucket.to_string());
        let newest_first = readings
            .iter()
            .rev()
            .map(|reading| reading.tag)
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(row[1], newest_first, "bucket {bucket}");
        // One spelling of each word the collation holds equal, the words
        // descending.
        let words = row[2].split(',').map(str::to_lowercase).collect::<Vec<_>>();
        let mut distinct = readings
            .iter()
            .map(|reading| reading.tag.to_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        distinct.reverse();
        assert_eq!(words, distinct, "bucket {bucket}");
        // NULL amounts join nothing; a group of only NULLs is NULL.
        let amounts = readings
            .iter()
            .filter_map(|reading| reading.amount.map(cents))
            .collect::<Vec<_>>();
        if amounts.is_empty() {
            assert_eq!(row[3], "NULL");
        } else {
            let mut joined = row[3].split(',').map(str::to_owned).collect::<Vec<_>>();
            let mut amounts = amounts;
            joined.sort();
            amounts.sort();
            assert_eq!(joined, amounts, "bucket {bucket}");
        }
    }
    // A ceiling a few times the rows' own size holds every group: each
    // used to reserve room for sixty-four rows and its text twice, and the
    // statement failed or spilled under it. The answer is the same.
    let (tight, _) = run(sql, 4 << 20);
    assert_eq!(tight, rows);
}

#[test]
fn time_functions_read_a_time_column_as_packed_units() {
    let (rows, counters) = run(
        "SELECT SUM(TIME_TO_SEC(elapsed)), SUM(HOUR(elapsed)), SUM(MINUTE(elapsed)), \
         SUM(SECOND(elapsed)), MIN(TIME_TO_SEC(elapsed)), MAX(HOUR(elapsed)) FROM readings",
        AMPLE,
    );
    let all = (1..=ROWS).map(reading).collect::<Vec<_>>();
    let sum = |part: fn(i64) -> i64| -> String {
        all.iter()
            .map(|reading| part(reading.seconds))
            .sum::<i64>()
            .to_string()
    };
    assert_eq!(
        rows,
        [vec![
            sum(|seconds| seconds),
            sum(|seconds| seconds.abs() / 3_600),
            sum(|seconds| seconds.abs() / 60 % 60),
            sum(|seconds| seconds.abs() % 60),
            all.iter()
                .map(|reading| reading.seconds)
                .min()
                .expect("rows")
                .to_string(),
            all.iter()
                .map(|reading| reading.seconds.abs() / 3_600)
                .max()
                .expect("rows")
                .to_string(),
        ]]
    );
    assert!(
        counters.time_kernel_batches >= 6,
        "each function read packed units: {counters:?}"
    );
}

#[test]
fn json_readers_of_one_column_share_its_parse() {
    let (rows, counters) = run(
        "SELECT MAX(CAST(JSON_EXTRACT(payload, '$.n') AS UNSIGNED)), \
         SUM(JSON_LENGTH(payload, '$.tags')), \
         SUM(JSON_LENGTH(payload)), COUNT(JSON_EXTRACT(payload, '$.absent')) FROM readings",
        AMPLE,
    );
    let all = (1..=ROWS).map(reading).collect::<Vec<_>>();
    assert_eq!(
        rows,
        [vec![
            all.iter()
                .map(|reading| reading.n)
                .max()
                .expect("rows")
                .to_string(),
            all.iter()
                .map(|reading| reading.tags)
                .sum::<u64>()
                .to_string(),
            (2 * ROWS).to_string(),
            "0".to_owned(),
        ]]
    );
    // Four readers a row: the first parses, the other three share.
    // (A row whose document is the row before's shares that parse too.)
    assert!(
        (3 * ROWS..4 * ROWS).contains(&counters.json_documents_reused),
        "one parse a row: {counters:?}"
    );
}

#[test]
fn a_spilled_window_reads_batches_and_orders_text_as_the_memory_path_does() {
    // Ties in the text key under the collation (`amber` and `Amber`) fall
    // back to the next key, and then to arrival order.
    for sql in [
        "SELECT id, payload, ROW_NUMBER() OVER (PARTITION BY tag ORDER BY bucket, id), \
         RANK() OVER (ORDER BY tag DESC), \
         SUM(amount) OVER (PARTITION BY tag ORDER BY bucket) FROM readings ORDER BY id",
        "SELECT id, payload, elapsed, DENSE_RANK() OVER (ORDER BY tag), \
         LAG(tag) OVER (PARTITION BY bucket ORDER BY tag DESC, id) FROM readings ORDER BY id",
    ] {
        let (memory, in_memory) = run(sql, AMPLE);
        assert_eq!(in_memory.window_spill_rows_batched, 0, "{sql}");
        assert!(
            in_memory.sort_keys_prepared > 0,
            "the memory path orders text by weights built once a row: {in_memory:?}"
        );
        // Some ceiling in this range is too small to hold the rows and
        // large enough to run: which one depends on the row width.
        let mut batched = 0;
        for ceiling in [5 << 18, 3 << 19, 7 << 18, 2 << 20] {
            let Ok((spilled, counters)) = std::panic::catch_unwind(|| run(sql, ceiling)) else {
                continue;
            };
            assert_eq!(spilled, memory, "{sql} under {ceiling} bytes");
            batched = batched.max(counters.window_spill_rows_batched);
        }
        assert!(
            batched >= ROWS,
            "the window spilled and read its rows back in batches: {batched}"
        );
    }
}
