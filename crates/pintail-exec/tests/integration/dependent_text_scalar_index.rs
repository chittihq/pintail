//! The dependent subquery index over text keys, and for scalar subqueries.
//!
//! A text equality keys by the collation weight bytes of the collation the
//! equality compiles to, so it must answer exactly as `=` does under it:
//! `utf8mb4_0900_ai_ci` folds case and accents and is NO PAD,
//! `utf8mb4_bin` is case-sensitive and PAD SPACE, `utf8mb4_general_ci` folds
//! case and is PAD SPACE. A text-against-number equality converts through
//! a double in `MySQL` and is never a key. A scalar subquery answers with
//! the first qualifying row in scan order under `LIMIT 1`, NULL for none,
//! and the cardinality error for two without it. The expectations are
//! derived by hand from those rules for the data below.

use std::time::Instant;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn text_column(id: u32, name: &str, collation: &str) -> Column {
    Column::new(id, name, DataType::Utf8, true).with_collation(Some(collation.to_owned()))
}

/// The outer table: a word under `utf8mb4_0900_ai_ci` and a number.
fn probes_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            text_column(2, "word", "utf8mb4_0900_ai_ci"),
            Column::new(3, "num", DataType::Int64, false),
        ],
    )
    .expect("probes schema")
}

/// The inner table: one word under each of three collations.
fn words_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            text_column(2, "word_ai", "utf8mb4_0900_ai_ci"),
            text_column(3, "word_bin", "utf8mb4_bin"),
            text_column(4, "word_gen", "utf8mb4_general_ci"),
            text_column(5, "title", "utf8mb4_0900_ai_ci"),
            text_column(6, "kind", "utf8mb4_0900_ai_ci"),
        ],
    )
    .expect("words schema")
}

fn stored(id: u64, values: Vec<Value>) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        values,
        id,
        false,
    )
}

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Utf8(value.to_owned()))
}

type Word<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    &'a str,
);

/// Inner rows, ids from 1 in this order - which is their scan order.
const WORDS: [Word<'static>; 9] = [
    (Some("Abc"), Some("Abc"), Some("Abc"), "first-Abc", "quiz"),
    (Some("abç"), Some("abc "), Some("abc  "), "second", "quiz"),
    (Some("xyz "), Some("xyz"), Some("XYZ"), "third", "note"),
    (Some("dup"), Some("dup"), Some("dup"), "dup-1", "quiz"),
    (Some("dup"), Some("dup"), Some("dup"), "dup-2", "quiz"),
    (None, None, None, "nothing", "quiz"),
    (Some("12.0"), Some("12.0"), Some("12.0"), "twelve", "quiz"),
    (Some("3"), Some("3"), Some("3"), "three-note", "note"),
    (Some("3"), Some("3"), Some("3"), "three-quiz", "quiz"),
];

/// Outer rows, ids from 1: (word, num).
const PROBES: [(Option<&str>, i64); 8] = [
    (Some("abc"), 1),
    (Some("ABC"), 2),
    (Some("xyz"), 3),
    (Some("dup"), 4),
    (None, 5),
    (Some("abc "), 6),
    (Some("zzz"), 7),
    (Some("12"), 12),
];

fn small_tables() -> (Vec<StoredRow>, Vec<StoredRow>) {
    let probes = PROBES
        .iter()
        .zip(1_u64..)
        .map(|((word, num), id)| {
            stored(id, vec![Value::UInt64(id), text(*word), Value::Int64(*num)])
        })
        .collect();
    let words = WORDS
        .iter()
        .zip(1_u64..)
        .map(|((ai, bin, general, title, kind), id)| {
            stored(
                id,
                vec![
                    Value::UInt64(id),
                    text(*ai),
                    text(*bin),
                    text(*general),
                    text(Some(title)),
                    text(Some(kind)),
                ],
            )
        })
        .collect();
    (probes, words)
}

struct Outcome {
    rows: Result<Vec<Vec<String>>, ExecError>,
    builds: u64,
    probes: u64,
}

fn run(probes: Vec<StoredRow>, words: Vec<StoredRow>, sql: &str) -> Outcome {
    let directories = [(); 2].map(|()| tempfile::tempdir().expect("table dir"));
    let counts = [probes.len() as u64, words.len() as u64];
    let mut outer = TableStore::open(
        directories[0].path(),
        probes_schema(),
        StoreOptions::default(),
    )
    .expect("open probes");
    outer.bulk_ingest_snapshot(probes).expect("probes snapshot");
    let mut inner = TableStore::open(
        directories[1].path(),
        words_schema(),
        StoreOptions::default(),
    )
    .expect("open words");
    inner.bulk_ingest_snapshot(words).expect("words snapshot");
    let snapshots = [outer.snapshot(), inner.snapshot()];
    let database_id = DatabaseId::new(8);
    let ids = [TableId::new(81), TableId::new(82)];
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                ids[0],
                "probes",
                probes_schema(),
                TableStatistics::with_row_count(counts[0]),
            )
            .expect("probes entry"),
            TableEntry::new(
                ids[1],
                "words",
                words_schema(),
                TableStatistics::with_row_count(counts[1]),
            )
            .expect("words entry"),
        ],
    )
    .expect("database");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, ids[0], &snapshots[0]),
        (database_id, ids[1], &snapshots[1]),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let _ = take_exec_counters();
    let rows = collect(physical, &provider);
    let counters = take_exec_counters();
    Outcome {
        rows,
        builds: counters.dependent_index_builds,
        probes: counters.dependent_index_probes,
    }
}

fn collect(
    physical: pintail_exec::PhysicalPlan,
    provider: &SnapshotScanProvider<'_>,
) -> Result<Vec<Vec<String>>, ExecError> {
    let mut execution =
        Execution::start(physical, provider, 256 * 1024 * 1024, Collation::default())?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch()? {
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..batch.columns().len())
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|values| values.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect(),
            );
        }
    }
    Ok(rows)
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(value) => u8::from(*value).to_string(),
        Value::Int64(value) => value.to_string(),
        Value::UInt64(value) => value.to_string(),
        Value::Utf8(value) => value.clone(),
        other => format!("{other:?}"),
    }
}

/// The second column of every row, in id order.
fn answers(outcome: &Outcome) -> Vec<String> {
    outcome
        .rows
        .as_ref()
        .unwrap_or_else(|error| panic!("query failed: {error}"))
        .iter()
        .map(|row| row[1].clone())
        .collect()
}

fn scalar_title(condition: &str) -> String {
    format!(
        "SELECT p.id, (SELECT w.title FROM words w WHERE {condition} LIMIT 1) AS title \
         FROM probes p ORDER BY p.id"
    )
}

#[test]
fn ai_ci_folds_case_and_accents_and_does_not_pad() {
    // 'abc' and 'ABC' equal 'Abc' and 'abç'; the first in scan order wins.
    // 'xyz' is not 'xyz ' and 'abc ' is not 'Abc' under a NO PAD collation.
    let (probes, words) = small_tables();
    let outcome = run(probes, words, &scalar_title("w.word_ai = p.word"));
    assert_eq!(
        answers(&outcome),
        [
            "first-Abc",
            "first-Abc",
            "NULL",
            "dup-1",
            "NULL",
            "NULL",
            "NULL",
            "NULL"
        ]
    );
    assert_eq!(outcome.builds, 1, "a text key builds the index");
    assert_eq!(outcome.probes, 8);
}

#[test]
fn bin_is_case_sensitive_and_pads() {
    // 'abc' and 'abc ' both equal 'abc ' under PAD SPACE; 'ABC' equals
    // nothing; 'xyz' equals 'xyz'.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        &scalar_title("w.word_bin = p.word COLLATE utf8mb4_bin"),
    );
    assert_eq!(
        answers(&outcome),
        [
            "second", "NULL", "third", "dup-1", "NULL", "second", "NULL", "NULL"
        ]
    );
    assert_eq!(outcome.builds, 1);
}

#[test]
fn general_ci_folds_case_and_pads() {
    // 'abc', 'ABC' and 'abc ' all equal 'Abc' (first) and 'abc  '; 'xyz'
    // equals 'XYZ'.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        &scalar_title("w.word_gen = p.word COLLATE utf8mb4_general_ci"),
    );
    assert_eq!(
        answers(&outcome),
        [
            "first-Abc",
            "first-Abc",
            "third",
            "dup-1",
            "NULL",
            "first-Abc",
            "NULL",
            "NULL"
        ]
    );
    assert_eq!(outcome.builds, 1);
}

#[test]
fn exists_over_a_text_key_matches_the_same_rows() {
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        "SELECT p.id, CASE WHEN EXISTS (SELECT 1 FROM words w WHERE w.word_ai = p.word) \
         THEN 1 ELSE 0 END FROM probes p ORDER BY p.id",
    );
    assert_eq!(answers(&outcome), ["1", "1", "0", "1", "0", "0", "0", "0"]);
    assert_eq!(outcome.builds, 1);
}

#[test]
fn a_cast_outer_number_under_an_explicit_collation_with_a_filter() {
    // Only probe 3's number is a word, and of its two rows only the second
    // is a quiz; everything else falls through to the COALESCE default.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        "SELECT p.id, COALESCE((SELECT w.title FROM words w \
           WHERE w.word_ai = CAST(p.num AS CHAR) COLLATE utf8mb4_0900_ai_ci \
             AND w.kind = 'quiz' LIMIT 1), CONCAT('#', p.num)) AS name \
         FROM probes p ORDER BY p.id",
    );
    assert_eq!(
        answers(&outcome),
        ["#1", "#2", "three-quiz", "#4", "#5", "#6", "#7", "#12"]
    );
    assert_eq!(outcome.builds, 1);
}

#[test]
fn text_against_a_number_is_not_a_key() {
    // MySQL compares these as doubles: '3' equals 3 and '12.0' equals 12,
    // where as text neither would. The index must not key by it: the few
    // rows of the table are read once and every one is compared, as
    // doubles, for every outer row.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        "SELECT p.id, CASE WHEN EXISTS (SELECT 1 FROM words w WHERE w.word_ai = p.num) \
         THEN 1 ELSE 0 END FROM probes p ORDER BY p.id",
    );
    assert_eq!(answers(&outcome), ["0", "0", "1", "0", "0", "0", "0", "1"]);
    assert_eq!((outcome.builds, outcome.probes), (1, 8));
}

#[test]
fn a_scalar_without_limit_raises_for_two_rows() {
    // 'dup' matches two rows; without LIMIT 1 that is the cardinality error.
    // LIMIT 2 keeps the subquery correlated (a bare one decorrelates into a
    // join) while still letting a second row through to raise it.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        "SELECT p.id, (SELECT w.title FROM words w WHERE w.word_ai = p.word LIMIT 2) \
         FROM probes p WHERE p.num = 4",
    );
    assert!(
        matches!(outcome.rows, Err(ExecError::ScalarSubqueryRows { rows: 2 })),
        "expected the cardinality error, got {:?}",
        outcome.rows
    );
    assert_eq!(outcome.builds, 1, "the index raised it, not a join");
}

#[test]
fn a_scalar_without_limit_answers_one_row_or_null() {
    // Probes 3 and 7 find nothing; probe 8 is '12', not '12.0'.
    let (probes, words) = small_tables();
    let outcome = run(
        probes,
        words,
        "SELECT p.id, (SELECT w.title FROM words w \
           WHERE w.word_bin = p.word COLLATE utf8mb4_bin LIMIT 2) \
         FROM probes p WHERE p.num IN (3, 7, 12) ORDER BY p.id",
    );
    assert_eq!(answers(&outcome), ["third", "NULL", "NULL"]);
    assert_eq!(outcome.builds, 1);
}

/// Three hundred outer rows over sixteen thousand inner rows with about 240
/// distinct correlation values: the memo cannot share them, and each
/// per-row execution reads the table.
#[test]
fn three_hundred_outer_rows_over_sixteen_thousand_answer_from_one_read() {
    const OUTER: u64 = 300;
    const INNER: u64 = 16_000;
    const SOURCES: u64 = 4_000;
    const DISTINCT: u64 = 240;
    let probes = (1..=OUTER)
        .map(|id| {
            let num = i64::try_from(id % DISTINCT + 1).expect("small");
            stored(id, vec![Value::UInt64(id), Value::Null, Value::Int64(num)])
        })
        .collect();
    let words = (1..=INNER)
        .map(|id| {
            let source = (id % SOURCES).to_string();
            stored(
                id,
                vec![
                    Value::UInt64(id),
                    text(Some(&source)),
                    text(Some(&source)),
                    text(Some(&source)),
                    text(Some(&format!("t{id}"))),
                    text(Some(if id.is_multiple_of(2) { "quiz" } else { "note" })),
                ],
            )
        })
        .collect();
    let sql = "SELECT p.id, COALESCE((SELECT w.title FROM words w \
                 WHERE w.word_ai = CAST(p.num AS CHAR) COLLATE utf8mb4_0900_ai_ci \
                   AND w.kind = 'quiz' LIMIT 1), CONCAT('Quiz #', p.num)) AS name \
               FROM probes p ORDER BY p.id";
    let started = Instant::now();
    let outcome = run(probes, words, sql);
    let elapsed = started.elapsed();
    println!("{OUTER} outer rows over {INNER} inner rows: {elapsed:?}");
    // The first row whose source is `num` is row `num` itself; SOURCES is
    // even, so every later one has the same parity and only an even `num`
    // finds a quiz.
    let expected = (1..=OUTER)
        .map(|id| {
            let num = id % DISTINCT + 1;
            if num.is_multiple_of(2) {
                format!("t{num}")
            } else {
                format!("Quiz #{num}")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(answers(&outcome), expected);
    assert_eq!(outcome.builds, 1, "the index engaged");
    // Sixteen thousand estimated rows wait out one per-row execution.
    assert_eq!(outcome.probes, OUTER - 1);
    assert!(elapsed.as_secs() < 5, "{OUTER} outer rows took {elapsed:?}");
}
