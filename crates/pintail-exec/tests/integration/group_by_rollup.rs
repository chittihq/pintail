//! `GROUP BY ... WITH ROLLUP`: subtotal rows after the groups they total,
//! the grand total last, rolled-up keys NULL outside aggregates (HAVING
//! included), `GROUPING()` telling a rolled NULL from a stored one, and no
//! rows at all over an empty input.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// (id, region, product, amount). One stored NULL region, so a rolled-up
/// NULL and a real one meet in the same result.
const ROWS: [(u64, Option<&str>, &str, i64); 5] = [
    (1, Some("east"), "apple", 10),
    (2, Some("east"), "pear", 5),
    (3, Some("west"), "apple", 7),
    (4, Some("west"), "apple", 3),
    (5, None, "apple", 1),
];

fn schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "region", DataType::Utf8, true),
            Column::new(3, "product", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Utf8(text) => text.clone(),
        Value::UInt64(number) => number.to_string(),
        Value::Int64(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn bind(sql: &str, catalog: &CatalogSnapshot) -> Result<pintail_sql::BoundQuery, String> {
    let statement = parse_statement(sql).map_err(|error| error.to_string())?;
    Binder::new(catalog, Some("app"))
        .bind(&statement)
        .map_err(|error| error.to_string())
}

fn catalog(database_id: DatabaseId, table_id: TableId) -> CatalogSnapshot {
    let database = DatabaseEntry::new(
        database_id,
        "app",
        [TableEntry::new(
            table_id,
            "sales",
            schema(),
            TableStatistics::with_row_count(ROWS.len() as u64),
        )
        .expect("sales entry")],
    )
    .expect("database entry");
    CatalogSnapshot::new([database]).expect("catalog")
}

fn run(sql: &str) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().expect("temporary table");
    let mut sales =
        TableStore::open(dir.path(), schema(), StoreOptions::default()).expect("open sales");
    sales
        .bulk_ingest_snapshot(
            ROWS.iter()
                .map(|(id, region, product, amount)| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(*id)]).expect("key"),
                        vec![
                            Value::UInt64(*id),
                            region.map_or(Value::Null, |region| Value::Utf8(region.to_owned())),
                            Value::Utf8((*product).to_owned()),
                            Value::Int64(*amount),
                        ],
                        *id,
                        false,
                    )
                })
                .collect(),
        )
        .expect("bulk sales");
    let snapshot = sales.snapshot();
    let database_id = DatabaseId::new(21);
    let table_id = TableId::new(22);
    let catalog = catalog(database_id, table_id);
    let provider =
        SnapshotScanProvider::new([(database_id, table_id, &snapshot)]).expect("provider");
    let bound = bind(sql, &catalog).unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("physical plan");
    let mut execution =
        Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
            .expect("start execution");
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("pull batch for {sql}: {error}"))
    {
        let columns = batch.columns().len();
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..columns)
                    .map(|column| {
                        render(
                            batch
                                .column(column)
                                .and_then(|column| column.value(row))
                                .expect("selected value"),
                        )
                    })
                    .collect(),
            );
        }
    }
    rows
}

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|value| (*value).to_owned()).collect())
        .collect()
}

#[test]
fn subtotals_follow_their_groups_and_the_grand_total_comes_last() {
    assert_eq!(
        run("SELECT region, product, COUNT(*), MAX(amount) FROM sales \
             GROUP BY region, product WITH ROLLUP"),
        table(&[
            &["NULL", "apple", "1", "1"],
            &["NULL", "NULL", "1", "1"],
            &["east", "apple", "1", "10"],
            &["east", "pear", "1", "5"],
            &["east", "NULL", "2", "10"],
            &["west", "apple", "2", "7"],
            &["west", "NULL", "2", "7"],
            &["NULL", "NULL", "5", "10"],
        ])
    );
}

#[test]
fn grouping_tells_a_rolled_up_null_from_a_stored_one() {
    assert_eq!(
        run(
            "SELECT region, product, GROUPING(region, product) AS g, GROUPING(product) \
             FROM sales GROUP BY region, product WITH ROLLUP"
        ),
        table(&[
            &["NULL", "apple", "0", "0"],
            &["NULL", "NULL", "1", "1"],
            &["east", "apple", "0", "0"],
            &["east", "pear", "0", "0"],
            &["east", "NULL", "1", "1"],
            &["west", "apple", "0", "0"],
            &["west", "NULL", "1", "1"],
            &["NULL", "NULL", "3", "1"],
        ])
    );
}

#[test]
fn having_sees_the_rolled_up_rows() {
    assert_eq!(
        run("SELECT region, COUNT(*) AS c FROM sales GROUP BY region WITH ROLLUP HAVING c > 1"),
        table(&[&["east", "2"], &["west", "2"], &["NULL", "5"]])
    );
    // A predicate on the key itself drops the rows that rolled it up.
    assert_eq!(
        run(
            "SELECT region, COUNT(*) FROM sales GROUP BY region WITH ROLLUP \
             HAVING region IS NOT NULL"
        ),
        table(&[&["east", "2"], &["west", "2"]])
    );
}

#[test]
fn keys_outside_the_select_list_ordinals_and_expressions() {
    assert_eq!(
        run("SELECT COUNT(*) FROM sales GROUP BY region WITH ROLLUP"),
        table(&[&["1"], &["2"], &["2"], &["5"]])
    );
    assert_eq!(
        run("SELECT CONCAT(region, '-') AS r, COUNT(*) FROM sales \
             WHERE region IS NOT NULL GROUP BY 1 WITH ROLLUP"),
        table(&[&["east-", "2"], &["west-", "2"], &["NULL", "4"]])
    );
}

#[test]
fn a_written_order_by_and_limit_apply_to_the_whole_result() {
    assert_eq!(
        run(
            "SELECT region, COUNT(*) FROM sales GROUP BY region WITH ROLLUP \
             ORDER BY region DESC LIMIT 2"
        ),
        table(&[&["west", "2"], &["east", "2"]])
    );
    assert_eq!(
        run(
            "SELECT region, COUNT(*) AS c FROM sales GROUP BY region WITH ROLLUP \
             ORDER BY c DESC"
        ),
        table(&[
            &["NULL", "5"],
            &["east", "2"],
            &["west", "2"],
            &["NULL", "1"]
        ])
    );
}

#[test]
fn an_empty_input_has_no_grand_total() {
    assert!(
        run("SELECT region, COUNT(*) FROM sales WHERE amount > 100 GROUP BY region WITH ROLLUP")
            .is_empty()
    );
}

#[test]
fn modifiers_the_union_cannot_express_are_refused() {
    let catalog = catalog(DatabaseId::new(21), TableId::new(22));
    for sql in [
        "SELECT region, COUNT(*) FROM sales GROUP BY region WITH CUBE",
        "SELECT DISTINCT region FROM sales GROUP BY region WITH ROLLUP",
        "SELECT region, GROUPING(product) FROM sales GROUP BY region WITH ROLLUP",
    ] {
        assert!(bind(sql, &catalog).is_err(), "{sql} must be refused");
    }
}

/// A select item that names no grouping key keeps its value in every row:
/// `GROUP BY 1` names the first select item, never a literal `1` written
/// elsewhere in the select list.
#[test]
fn a_constant_item_survives_the_super_aggregate_rows() {
    assert_eq!(
        run("SELECT region, 1+1, COUNT(*) FROM sales GROUP BY 1 WITH ROLLUP"),
        table(&[
            &["NULL", "2", "1"],
            &["east", "2", "2"],
            &["west", "2", "2"],
            &["NULL", "2", "5"]
        ])
    );
}

/// An item that is itself a key the subtotal still groups by keeps its
/// value even when a column inside it is rolled up.
#[test]
fn an_expression_key_keeps_its_value_while_its_column_is_rolled_up() {
    for sql in [
        "SELECT LEFT(region, 2), region, COUNT(*) FROM sales GROUP BY 1, 2 WITH ROLLUP",
        "SELECT LEFT(region, 2) AS r, region, COUNT(*) FROM sales GROUP BY r, region WITH ROLLUP",
    ] {
        assert_eq!(
            run(sql),
            table(&[
                &["NULL", "NULL", "1"],
                &["NULL", "NULL", "1"],
                &["ea", "east", "2"],
                &["ea", "NULL", "2"],
                &["we", "west", "2"],
                &["we", "NULL", "2"],
                &["NULL", "NULL", "5"]
            ]),
            "{sql}"
        );
    }
}

/// Rolling up a key named by an alias turns that one item into NULL, not
/// every item over the same column.
#[test]
fn an_aliased_key_rolls_up_only_its_own_item() {
    let rows = run("SELECT region, product, region AS r, COUNT(*) FROM sales \
                    GROUP BY region, product, r WITH ROLLUP");
    assert!(
        rows.contains(&table(&[&["east", "apple", "NULL", "1"]])[0]),
        "{rows:?}"
    );
    assert!(
        rows.contains(&table(&[&["east", "NULL", "NULL", "2"]])[0]),
        "{rows:?}"
    );
    assert_eq!(rows.len(), 12, "{rows:?}");
    assert_eq!(
        rows.last(),
        Some(&table(&[&["NULL", "NULL", "NULL", "5"]])[0])
    );
}

/// A folded `GROUPING()` is still named as written.
#[test]
fn grouping_columns_keep_their_written_names() {
    let catalog = catalog(DatabaseId::new(21), TableId::new(22));
    let sql = "SELECT region, GROUPING(region), GROUPING(region,product) AS g, \
               COUNT(*) FROM sales GROUP BY region, product WITH ROLLUP";
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&catalog, Some("app"))
        .with_source(sql)
        .bind(&statement)
        .expect("bind");
    assert_eq!(
        bound
            .projection
            .iter()
            .take(4)
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["region", "GROUPING(region)", "g", "COUNT(*)"]
    );
}
