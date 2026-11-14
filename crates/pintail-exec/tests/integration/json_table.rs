//! `JSON_TABLE` end to end over a stored table: the row production the
//! differential oracle cannot see from its answers alone.
//!
//! The oracle compares rows with `MySQL`. These tests pin what rows do not
//! show: the errors a table function raises and their texts, that a
//! predicate on the table the document comes from runs before the document
//! is read, where the planner puts the function, and that producing rows is
//! charged to the query's memory ceiling and ends at its deadline.

use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecError, Execution, LogicalPlan, LogicalPlanner, Optimizer, PhysicalPlanner,
    SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// (id, payload, notes). `payload` is a JSON column; `notes` is text that
/// is JSON on some rows and not on the last.
const PARCELS: [(u64, Option<&str>, Option<&str>); 5] = [
    (
        1,
        Some(r#"[{"sku": "A1", "qty": 2, "tags": ["x", "y"]}, {"sku": "B2", "qty": 5}]"#),
        Some("[1,2]"),
    ),
    (2, Some("[]"), Some("[]")),
    (3, None, None),
    (4, Some(r#"{"sku": "Z9"}"#), Some(r#"{"k":1}"#)),
    (
        5,
        Some(r#"[{"sku": "C3", "qty": "7", "tags": []}]"#),
        Some("oops"),
    ),
];

const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

fn parcels_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "payload", DataType::Json, true),
            Column::new(3, "notes", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn depots_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "label", DataType::Utf8, true),
        ],
    )
    .expect("schema")
}

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::Utf8(text.to_owned()))
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) | Value::Enum { label: text, .. } => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

/// What a statement is asked to run under.
struct Limits {
    memory: usize,
    deadline: Option<Instant>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: MEMORY_LIMIT,
            deadline: None,
        }
    }
}

/// Binds, plans and runs `sql`, handing the optimized logical plan to
/// `inspect` on the way. A refusal at any stage is its text.
#[allow(clippy::too_many_lines)] // one fixture, bound, planned and run
fn run_with(
    sql: &str,
    limits: &Limits,
    inspect: &mut dyn FnMut(&LogicalPlan),
) -> Result<Vec<Vec<String>>, String> {
    let parcels_dir = tempfile::tempdir().expect("temporary parcels table");
    let mut parcels = TableStore::open(
        parcels_dir.path(),
        parcels_schema(),
        StoreOptions::default(),
    )
    .expect("open parcels");
    parcels
        .bulk_ingest_snapshot(
            PARCELS
                .iter()
                .map(|(id, payload, notes)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![Value::UInt64(*id), text(*payload), text(*notes)],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk parcels");
    let depots_dir = tempfile::tempdir().expect("temporary depots table");
    let mut depots = TableStore::open(depots_dir.path(), depots_schema(), StoreOptions::default())
        .expect("open depots");
    depots
        .bulk_ingest_snapshot(
            [(1_u64, "north"), (2, "south"), (5, "east")]
                .into_iter()
                .map(|(id, label)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                        vec![Value::UInt64(id), Value::Utf8(label.to_owned())],
                        id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk depots");

    let parcels_snapshot = parcels.snapshot();
    let depots_snapshot = depots.snapshot();
    let database_id = DatabaseId::new(21);
    let parcels_id = TableId::new(31);
    let depots_id = TableId::new(32);
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [
            TableEntry::new(
                parcels_id,
                "parcels",
                parcels_schema(),
                TableStatistics::with_row_count(PARCELS.len() as u64),
            )
            .expect("parcels entry"),
            TableEntry::new(
                depots_id,
                "depots",
                depots_schema(),
                TableStatistics::with_row_count(3),
            )
            .expect("depots entry"),
        ],
    )
    .expect("database entry");
    let catalog = CatalogSnapshot::new([database]).expect("catalog");
    let provider = SnapshotScanProvider::new([
        (database_id, parcels_id, &parcels_snapshot),
        (database_id, depots_id, &depots_snapshot),
    ])
    .expect("provider");

    let statement = parse_statement(sql).map_err(|error| format!("parse: {error}"))?;
    let bound = Binder::new(&catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| format!("bind: {error}"))?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    inspect(&logical);
    let physical = PhysicalPlanner::plan(logical, Collation::default())
        .map_err(|error| format!("plan: {error}"))?;
    let mut execution = Execution::start_with_deadline(
        physical,
        &provider,
        limits.memory,
        limits.deadline,
        Collation::default(),
    )
    .map_err(|error| describe(&error))?;
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().map_err(|error| describe(&error))? {
        let columns = batch.columns().len();
        for row in batch.selection().selected_rows() {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                let value = batch
                    .column(column)
                    .and_then(|column| column.value(row))
                    .cloned()
                    .expect("selected value");
                values.push(render(&value));
            }
            rows.push(values);
        }
    }
    Ok(rows)
}

/// An execution failure by its kind where the kind is the point, and by
/// its text otherwise.
fn describe(error: &ExecError) -> String {
    match error {
        ExecError::MemoryLimitExceeded { .. } => "memory limit exceeded".to_owned(),
        ExecError::QueryTimedOut => "timed out".to_owned(),
        other => other.to_string(),
    }
}

fn rows(sql: &str) -> Vec<Vec<String>> {
    run_with(sql, &Limits::default(), &mut |_| {}).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn refusal(sql: &str) -> String {
    match run_with(sql, &Limits::default(), &mut |_| {}) {
        Ok(rows) => panic!("{sql}: expected a refusal, got {} row(s)", rows.len()),
        Err(error) => error,
    }
}

/// The table function of an optimized plan and what it reads, under the
/// row-preserving operators a query puts above it.
fn json_table(plan: &LogicalPlan) -> Option<(&LogicalPlan, bool)> {
    match plan {
        LogicalPlan::JsonTable { input, apply } => Some((input.as_ref(), apply.outer)),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. } => json_table(input),
        _ => None,
    }
}

const ITEMS: &str = "JSON_TABLE(p.payload, '$[*]' COLUMNS (pos FOR ORDINALITY, \
                     sku VARCHAR(8) PATH '$.sku', qty INT PATH '$.qty')) AS jt";

#[test]
fn v1_a_document_column_expands_once_per_row_of_its_table() {
    assert_eq!(
        rows(&format!(
            "SELECT p.id, jt.pos, jt.sku, jt.qty FROM parcels p, {ITEMS} ORDER BY p.id, jt.pos"
        )),
        [
            ["1", "1", "A1", "2"],
            ["1", "2", "B2", "5"],
            ["5", "1", "C3", "7"]
        ]
    );
}

#[test]
fn v2_a_left_join_keeps_a_row_whose_document_yields_none() {
    // An empty array, a NULL document and an object the row path does not
    // match all yield no rows, and each parcel is kept once.
    assert_eq!(
        rows(&format!(
            "SELECT p.id, jt.pos, jt.sku FROM parcels p LEFT JOIN {ITEMS} ON TRUE \
             ORDER BY p.id, jt.pos"
        )),
        [
            ["1", "1", "A1"],
            ["1", "2", "B2"],
            ["2", "NULL", "NULL"],
            ["3", "NULL", "NULL"],
            ["4", "NULL", "NULL"],
            ["5", "1", "C3"]
        ]
    );
}

#[test]
fn v3_a_left_join_condition_is_tested_before_a_row_counts_as_unmatched() {
    assert_eq!(
        rows(&format!(
            "SELECT p.id, jt.sku, jt.qty FROM parcels p LEFT JOIN {ITEMS} ON jt.qty > 2 \
             ORDER BY p.id, jt.pos"
        )),
        [
            ["1", "B2", "5"],
            ["2", "NULL", "NULL"],
            ["3", "NULL", "NULL"],
            ["4", "NULL", "NULL"],
            ["5", "C3", "7"]
        ]
    );
    // The condition reads the preserved side as well.
    assert_eq!(
        rows(&format!(
            "SELECT p.id, jt.sku FROM parcels p LEFT JOIN {ITEMS} ON jt.qty = p.id + 1 \
             ORDER BY p.id"
        )),
        [
            ["1", "A1"],
            ["2", "NULL"],
            ["3", "NULL"],
            ["4", "NULL"],
            ["5", "NULL"]
        ]
    );
}

#[test]
fn v4_sibling_nested_lists_take_turns_and_an_empty_parent_keeps_one_row() {
    assert_eq!(
        rows(
            r#"SELECT * FROM JSON_TABLE(
                 '[{"a":1,"b":[10,20],"c":[100,200]},{"a":2,"b":[],"c":[7]},{"a":3}]',
                 '$[*]' COLUMNS (
                   o FOR ORDINALITY, a INT PATH '$.a',
                   NESTED PATH '$.b[*]' COLUMNS (bo FOR ORDINALITY, b INT PATH '$'),
                   NESTED PATH '$.c[*]' COLUMNS (co FOR ORDINALITY, c INT PATH '$'))) AS jt"#
        ),
        [
            ["1", "1", "1", "10", "NULL", "NULL"],
            ["1", "1", "2", "20", "NULL", "NULL"],
            ["1", "1", "NULL", "NULL", "1", "100"],
            ["1", "1", "NULL", "NULL", "2", "200"],
            ["2", "2", "NULL", "NULL", "1", "7"],
            ["3", "3", "NULL", "NULL", "NULL", "NULL"]
        ]
    );
    assert_eq!(
        rows(
            "SELECT p.id, jt.sku, jt.tag FROM parcels p, JSON_TABLE(p.payload, '$[*]' COLUMNS (\
             sku VARCHAR(8) PATH '$.sku', \
             NESTED PATH '$.tags[*]' COLUMNS (tag VARCHAR(4) PATH '$'))) AS jt \
             ORDER BY p.id, jt.sku, jt.tag"
        ),
        [
            ["1", "A1", "x"],
            ["1", "A1", "y"],
            ["1", "B2", "NULL"],
            ["5", "C3", "NULL"]
        ]
    );
}

#[test]
fn v5_a_predicate_on_the_documents_table_runs_before_the_document_is_read() {
    let notes = "JSON_TABLE(p.notes, '$[*]' COLUMNS (v INT PATH '$')) AS jt";
    // Parcel 5's notes are not JSON, and reading them fails the statement.
    let error = refusal(&format!("SELECT jt.v FROM parcels p, {notes}"));
    assert!(
        error.starts_with("Invalid JSON text in argument 1 to function json_table"),
        "{error}"
    );
    // A filter that excludes the parcel is applied to the scan beneath the
    // table function, so the text is never read: the statement answers,
    // and the plan shows where the predicate went.
    let mut pushed = false;
    let answered = run_with(
        &format!("SELECT p.id, jt.v FROM parcels p, {notes} WHERE p.id < 5 ORDER BY 1, 2"),
        &Limits::default(),
        &mut |plan| {
            pushed = matches!(
                json_table(plan),
                Some((LogicalPlan::Scan(scan), false)) if !scan.predicates.is_empty()
            );
        },
    )
    .expect("filtered statement");
    assert_eq!(answered, [["1", "1"], ["1", "2"]]);
    assert!(
        pushed,
        "the predicate should sit on the scan under the table function"
    );
}

#[test]
fn v6_a_condition_between_two_earlier_tables_joins_them_under_the_function() {
    // Both tables precede the table function, so they are its input; the
    // equality between them is their join condition there, not a filter on
    // every expanded pair.
    let mut joined = false;
    let answered = run_with(
        &format!(
            "SELECT d.label, jt.sku FROM depots d, parcels p, {ITEMS} \
             WHERE d.id = p.id ORDER BY 1, 2"
        ),
        &Limits::default(),
        &mut |plan| {
            joined = matches!(
                json_table(plan),
                Some((
                    LogicalPlan::Join {
                        condition: Some(_),
                        ..
                    },
                    false
                ))
            );
        },
    )
    .expect("three relations");
    assert_eq!(answered, [["east", "C3"], ["north", "A1"], ["north", "B2"]]);
    assert!(
        joined,
        "the two tables should be joined beneath the table function"
    );
}

#[test]
fn v7_a_constant_document_is_a_relation_the_join_planner_orders_freely() {
    assert_eq!(
        rows(
            "SELECT d.label, jt.v FROM JSON_TABLE('[5, 1, 9]', '$[*]' COLUMNS (v INT PATH '$')) \
             AS jt LEFT JOIN depots d ON d.id = jt.v ORDER BY jt.v"
        ),
        [["north", "1"], ["east", "5"], ["NULL", "9"]]
    );
    assert_eq!(
        rows(
            "SELECT p.id FROM parcels p WHERE p.id IN (SELECT v FROM JSON_TABLE('[5, 1, 9]', \
             '$[*]' COLUMNS (v INT PATH '$')) AS jt) ORDER BY 1"
        ),
        [["1"], ["5"]]
    );
}

#[test]
fn v8_error_clauses_fail_the_statement_with_the_reason() {
    let table = |columns: &str| {
        refusal(&format!(
            r#"SELECT * FROM JSON_TABLE('[{{"a": "abcdef", "b": [1, 2], "c": 300}}]', '$[*]' COLUMNS ({columns})) AS jt"#
        ))
    };
    assert_eq!(
        table("v INT PATH '$.missing' ERROR ON EMPTY"),
        "Missing value for JSON_TABLE column 'v'"
    );
    assert_eq!(
        table("v INT PATH '$.a' ERROR ON ERROR"),
        "Invalid JSON value for CAST to INTEGER from column v at row 1"
    );
    assert_eq!(
        table("v VARCHAR(3) PATH '$.a' ERROR ON ERROR"),
        "Value is out of range for JSON_TABLE's column 'v'"
    );
    assert_eq!(
        table("v TINYINT PATH '$.c' ERROR ON ERROR"),
        "Value is out of range for JSON_TABLE's column 'v'"
    );
    assert_eq!(
        table("v INT PATH '$.b' ERROR ON ERROR"),
        "Can't store an array or an object in the scalar JSON_TABLE column 'v'"
    );
    assert_eq!(
        table("v INT PATH '$.b[*]' ERROR ON ERROR"),
        "Can't store an array or an object in the scalar JSON_TABLE column 'v'"
    );
    // Without the clause each of them is NULL.
    assert_eq!(
        rows(
            r#"SELECT * FROM JSON_TABLE('[{"a": "abcdef", "b": [1, 2], "c": 300}]', '$[*]' COLUMNS (
                 e INT PATH '$.missing', i INT PATH '$.a', s VARCHAR(3) PATH '$.a',
                 t TINYINT PATH '$.c', m INT PATH '$.b[*]')) AS jt"#
        ),
        [["NULL", "NULL", "NULL", "NULL", "NULL"]]
    );
}

#[test]
fn v9_a_default_is_checked_before_any_document_is_read() {
    // The document is NULL and has no rows, and the statement still fails
    // on a DEFAULT its column cannot hold.
    let over_null = |column: &str| {
        refusal(&format!(
            "SELECT * FROM JSON_TABLE(NULL, '$[*]' COLUMNS ({column})) AS jt"
        ))
    };
    assert!(
        over_null("v INT PATH '$' DEFAULT 'abc' ON EMPTY")
            .starts_with("Invalid JSON text in argument 1 to function JSON_TABLE"),
    );
    assert_eq!(
        over_null("v INT PATH '$' DEFAULT '[1]' ON ERROR"),
        "Invalid default value for 'v'"
    );
    assert_eq!(
        over_null(r#"v INT PATH '$' DEFAULT '"abc"' ON EMPTY"#),
        "Invalid JSON value for CAST to INTEGER from column v at row 1"
    );
    assert_eq!(
        over_null(r#"v VARCHAR(3) PATH '$' DEFAULT '"abcdef"' ON EMPTY"#),
        "Value is out of range for JSON_TABLE's column 'v'"
    );
    assert_eq!(
        over_null("v INT PATH '$' DEFAULT 'null' ON EMPTY"),
        "Invalid JSON value for CAST to INTEGER from column v at row 1"
    );
}

#[test]
fn v10_paths_are_checked_as_every_json_function_checks_them() {
    assert_eq!(
        refusal("SELECT * FROM JSON_TABLE('[1]', '$[' COLUMNS (v INT PATH '$')) AS jt"),
        "Invalid JSON path expression. The error is around character position 2."
    );
    assert_eq!(
        refusal("SELECT * FROM JSON_TABLE('[1]', '$' COLUMNS (v INT PATH 'a')) AS jt"),
        "Invalid JSON path expression. The error is around character position 1."
    );
    assert_eq!(
        refusal(
            "SELECT * FROM JSON_TABLE('[1]', '$' COLUMNS (\
             NESTED PATH 'x' COLUMNS (v INT PATH '$'))) AS jt"
        ),
        "Invalid JSON path expression. The error is around character position 1."
    );
}

#[test]
fn v11_what_is_not_supported_is_refused_by_name() {
    for (sql, needle) in [
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v INT PATH '$'))",
            "JSON_TABLE: Every table function must have an alias",
        ),
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v TIME PATH '$')) AS jt",
            "JSON_TABLE: column 'v' has type TIME",
        ),
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v BLOB PATH '$')) AS jt",
            "JSON_TABLE: column 'v' has type BLOB",
        ),
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v JSON EXISTS PATH '$')) AS jt",
            "JSON_TABLE: EXISTS column 'v' has type JSON",
        ),
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v INT PATH '$' DEFAULT 7 ON EMPTY)) \
             AS jt",
            "JSON_TABLE: non-string DEFAULT value",
        ),
        (
            "SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (a INT PATH '$', A INT PATH '$')) \
             AS jt",
            "JSON_TABLE: Duplicate column name 'A'",
        ),
        (
            "SELECT * FROM JSON_TABLE(5, '$' COLUMNS (v INT PATH '$')) AS jt",
            "JSON_TABLE: Invalid data type for JSON data in argument 1",
        ),
        (
            "SELECT p.id, (SELECT SUM(v) FROM JSON_TABLE(p.notes, '$[*]' \
             COLUMNS (v INT PATH '$')) AS jt) FROM parcels p",
            "JSON_TABLE: a document that reads a column of an enclosing query",
        ),
    ] {
        let error = refusal(sql);
        assert!(error.contains(needle), "`{sql}` answered `{error}`");
    }
    // A table function reads what is written before it, never after.
    let error = refusal(&format!("SELECT * FROM {ITEMS}, parcels p"));
    assert!(error.starts_with("bind:"), "{error}");
}

/// A document of `count` array cells, built by the statement itself.
fn wide_document(count: usize) -> String {
    format!(
        "JSON_TABLE(CONCAT('[', REPEAT('1,', {count}), '1]'), '$[*]' COLUMNS (v INT PATH '$')) \
         AS jt"
    )
}

#[test]
fn v12_rows_are_charged_to_the_memory_ceiling_as_they_are_produced() {
    let sql = format!("SELECT COUNT(*) FROM {}", wide_document(200_000));
    assert_eq!(rows(&sql), [["200001"]]);
    // The same statement under a ceiling its rows do not fit: the failure
    // is the reservation made while expanding the document.
    let tight = Limits {
        memory: 4 * 1024 * 1024,
        deadline: None,
    };
    assert_eq!(
        run_with(&sql, &tight, &mut |_| {}),
        Err("memory limit exceeded".to_owned())
    );
}

#[test]
fn v13_an_elapsed_deadline_ends_the_statement() {
    let sql = format!("SELECT COUNT(*) FROM {}", wide_document(1_000));
    let expired = Limits {
        memory: MEMORY_LIMIT,
        deadline: Instant::now().checked_sub(Duration::from_secs(1)),
    };
    assert!(expired.deadline.is_some(), "the clock has run for a second");
    assert_eq!(
        run_with(&sql, &expired, &mut |_| {}),
        Err("timed out".to_owned())
    );
}
