//! A pattern the automaton cannot write runs on a backtracking machine
//! whose stack and capture log are working memory of the statement. Each
//! such program is charged to the query's budget before a row is matched,
//! so a budget that cannot hold the machine refuses the statement rather
//! than letting the search grow outside it.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "word", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

/// What `sql` answers over a few short words under `limit` bytes, with the
/// query's peak, or the error that stopped it.
fn run(sql: &str, limit: usize) -> Result<(Vec<Value>, usize), ExecError> {
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema(), StoreOptions::default()).expect("table");
    table
        .bulk_ingest_snapshot(
            ["noon", "level", "abab", "plain"]
                .into_iter()
                .zip(1_u64..)
                .map(|(word, id)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(word.to_owned())],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    let snapshot = table.snapshot();
    let entry = TableEntry::new(
        TableId::new(1),
        "words",
        schema(),
        TableStatistics::with_row_count(4),
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
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = Execution::start(plan, &provider, limit, Collation::default())?;
    let mut answer = Vec::new();
    while let Some(batch) = execution.next_batch()? {
        for row in batch.selection().selected_rows() {
            answer.push(batch.columns()[0].value_owned(row).expect("value"));
        }
    }
    Ok((answer, execution.memory().peak()))
}

/// The smallest budget, to the kibibyte, under which `sql` answers.
fn smallest_budget(sql: &str) -> usize {
    let (mut refused, mut answered) = (0_usize, 64_usize << 20);
    assert!(
        run(sql, answered).is_ok(),
        "{sql} answers under a wide budget"
    );
    while answered - refused > 1024 {
        let middle = refused + (answered - refused) / 2;
        if run(sql, middle).is_ok() {
            answered = middle;
        } else {
            refused = middle;
        }
    }
    answered
}

const DOUBLED: &str = r"SELECT id FROM words WHERE REGEXP_LIKE(word, '(.)\\1') ORDER BY id";
const PLAIN: &str = "SELECT id FROM words WHERE REGEXP_LIKE(word, 'oo|bab') ORDER BY id";

#[test]
fn a_backtracking_pattern_answers_within_its_budget() {
    let (answer, peak) = run(DOUBLED, 64 << 20).expect("answers");
    assert_eq!(answer, vec![Value::UInt64(1)]);
    assert!(peak < 64 << 20, "peak {peak}");
    let (plain, _) = run(PLAIN, 64 << 20).expect("answers");
    assert_eq!(plain, vec![Value::UInt64(1), Value::UInt64(3)]);
}

/// The machine's working memory is part of what the statement needs: the
/// same query over a pattern the automaton runs fits a budget the
/// backtracking pattern does not, and under that budget the statement is
/// refused by the memory limit, not run.
#[test]
fn the_machine_is_charged_to_the_query() {
    let plain = smallest_budget(PLAIN);
    let doubled = smallest_budget(DOUBLED);
    assert!(
        doubled >= plain + (128 << 10),
        "the backtracking pattern needs {doubled} bytes, the plain one {plain}"
    );
    let error = run(DOUBLED, plain).expect_err("refused under the plain pattern's budget");
    assert!(
        error.to_string().contains("memory limit exceeded"),
        "{error}"
    );
}
