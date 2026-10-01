//! Edges of the column-at-a-time aggregate folds, each measured against
//! `MySQL` 8.4 on a replica of the same rows:
//!
//! - a decimal declared wider than a lane's 64 bits keeps its values (a
//!   grouped SUM, AVG, MIN or MAX over one answered NULL);
//! - a text group displays the spelling of its first row that passed the
//!   filter, not of a row the filter dropped;
//! - keys of two collations stay apart when segments fold one at a time;
//! - a DOUBLE SUM or AVG adds its rows in row order, and a variance runs
//!   its recurrence over them in that order, whatever the key;
//! - an integer SUM whose running total leaves 64 bits and comes back is
//!   its total, and so is one that ends outside them: an integer SUM is a
//!   DECIMAL.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const ROWS: u64 = 120_000;
const CRATES: u64 = 12;

/// Spellings `utf8mb4_0900_ai_ci` calls one letter, and one it does not.
const MAKERS: [&str; 8] = ["Á", "SS", "A", "Å", "á", "å", "a", "ss"];

/// The wide count of row `id`: past 64 bits for every row that has one.
fn wide_of(id: u64) -> Option<u128> {
    // Crate 11 never has a count; crate 10 has none until late in the table.
    let crate_id = id % CRATES;
    if id.is_multiple_of(10) || crate_id == 11 || (crate_id == 10 && id < 110_000) {
        return None;
    }
    Some(18_446_744_073_709_551_615 - u128::from(id % 500))
}

/// The reading of row `id`: values whose sum depends on the order of the
/// additions.
fn reading_of(id: u64) -> f64 {
    let scale = 1.0 + f64::from(u32::try_from(id % 13).expect("small")) / 1024.0;
    scale
        * match (id * 7 + id / 4_099) % 9 {
            0 => 1e16,
            1 => 1.0,
            2 => -1e16,
            3 => 0.1,
            4 => 1e-7,
            5 => 123_456.789,
            6 => -0.3,
            7 => 3.000_000_000_000_000_4,
            _ => -7e15,
        }
}

/// The stock count of row `id`: zero but for three rows, far apart, whose
/// running total leaves 64 bits and returns.
fn stock_of(id: u64) -> i64 {
    match id {
        5 => i64::MAX,
        60_005 => 1,
        117_005 => -1,
        _ => 0,
    }
}

/// The tag of row `id`: two spellings only a case-sensitive collation
/// tells apart, each beside every maker.
fn tag_of(id: u64) -> &'static str {
    if (id / 8).is_multiple_of(2) { "x" } else { "X" }
}

fn part_row(id: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % CRATES).expect("small")),
            wide_of(id).map_or(Value::Null, |wide| Value::Utf8(wide.to_string())),
            Value::Utf8(MAKERS[usize::try_from(id % 8).expect("small")].to_owned()),
            Value::float64(reading_of(id)),
            Value::Int64(stock_of(id)),
            Value::Utf8(tag_of(id).to_owned()),
        ],
        id + 1,
        false,
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

fn fixture() -> Fixture {
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "crate", DataType::Int64, false),
            Column::new(
                3,
                "wide",
                DataType::Decimal {
                    precision: 20,
                    scale: 0,
                },
                true,
            ),
            Column::new(4, "maker", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_0900_ai_ci".to_owned())),
            Column::new(5, "reading", DataType::Float64, false),
            Column::new(6, "stock", DataType::Int64, false),
            Column::new(7, "tag", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_bin".to_owned())),
        ],
    )
    .expect("schema");
    let directory = tempfile::tempdir().expect("directory");
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).expect("table");
    let mut start = 0;
    while start < ROWS {
        let end = (start + 40_000).min(ROWS);
        table
            .bulk_ingest_snapshot((start..end).map(part_row).collect())
            .expect("ingest");
        start = end;
    }
    let entry = TableEntry::new(
        TableId::new(1),
        "parts",
        schema,
        TableStatistics::with_row_count(ROWS),
    )
    .expect("entry");
    let catalog = CatalogSnapshot::new([
        DatabaseEntry::new(DatabaseId::new(1), "test", [entry]).expect("database")
    ])
    .expect("catalog");
    Fixture {
        _directory: directory,
        table,
        catalog,
    }
}

fn run(fixture: &Fixture, sql: &str) -> Vec<Vec<Value>> {
    try_run(fixture, sql).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

/// The rows `sql` answers, or the error it is refused with.
fn try_run(fixture: &Fixture, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let snapshot = fixture.table.snapshot();
    let provider = SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
        .expect("provider");
    let bound = Binder::new(&fixture.catalog, Some("test"))
        .bind(&parse_statement(sql).expect("parse"))
        .expect("bind");
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let mut execution = Execution::start(plan, &provider, 96 << 20, Collation::default())
        .map_err(|error| format!("{error:?}"))?;
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .map_err(|error| format!("{error:?}"))?
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value").clone())
                    .collect::<Vec<_>>(),
            );
        }
    }
    Ok(rows)
}

/// A decimal cell's text; `None` for NULL.
fn decimal_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Utf8(text) => Some(text.clone()),
        Value::DecimalAverage(average) => Some(average.label.clone()),
        other => panic!("not a decimal cell: {other:?}"),
    }
}

/// SUM, MIN and MAX of `wide` over the rows `keep` passes, per crate.
fn wide_totals(keep: impl Fn(u64) -> bool) -> Vec<Option<(u128, u128, u128)>> {
    let mut totals: Vec<Option<(u128, u128, u128)>> =
        vec![None; usize::try_from(CRATES).expect("small")];
    for id in (0..ROWS).filter(|id| keep(*id)) {
        if let Some(wide) = wide_of(id) {
            let slot = &mut totals[usize::try_from(id % CRATES).expect("small")];
            *slot = Some(slot.map_or((wide, wide, wide), |(sum, least, most)| {
                (sum + wide, least.min(wide), most.max(wide))
            }));
        }
    }
    totals
}

#[test]
fn a_decimal_wider_than_a_lane_keeps_its_values() {
    let fixture = fixture();
    for (filter, keep) in [
        ("", (|_| true) as fn(u64) -> bool),
        ("WHERE id % 5 <> 1", |id| id % 5 != 1),
    ] {
        let expected = wide_totals(keep);
        for key in ["crate", "crate + 0"] {
            let rows = run(
                &fixture,
                &format!(
                    "SELECT {key} AS k, SUM(wide), MIN(wide), MAX(wide), COUNT(wide) \
                     FROM parts {filter} GROUP BY k ORDER BY k"
                ),
            );
            assert_eq!(rows.len(), expected.len(), "{key} {filter}");
            for (row, expected) in rows.iter().zip(&expected) {
                let answered = (
                    decimal_text(&row[1]),
                    decimal_text(&row[2]),
                    decimal_text(&row[3]),
                );
                let expected = expected.map_or((None, None, None), |(sum, least, most)| {
                    (
                        Some(sum.to_string()),
                        Some(least.to_string()),
                        Some(most.to_string()),
                    )
                });
                assert_eq!(answered, expected, "{key} {filter} group {:?}", row[0]);
            }
        }
    }
}

#[test]
fn a_group_displays_its_first_selected_spelling() {
    let fixture = fixture();
    // Rows 0 and 2 hold the letter's first spellings and row 1 the first
    // of the double s; each filter drops a different one of them.
    for (filter, keep) in [
        ("", (|_| true) as fn(u64) -> bool),
        ("WHERE id % 3 <> 0", |id| id % 3 != 0),
        ("WHERE id > 2", |id| id > 2),
        ("WHERE id % 8 > 3", |id| id % 8 > 3),
    ] {
        let first = |letter: bool| {
            (0..ROWS)
                .filter(|id| keep(*id))
                .map(|id| MAKERS[usize::try_from(id % 8).expect("small")])
                .find(|maker| matches!(*maker, "SS" | "ss") != letter)
                .expect("both groups have rows")
                .to_owned()
        };
        let mut expected = vec![first(true), first(false)];
        expected.sort();
        for sql in [
            format!("SELECT maker, COUNT(*) FROM parts {filter} GROUP BY maker"),
            format!("SELECT maker, COUNT(*), MIN(id) FROM parts {filter} GROUP BY maker"),
        ] {
            let mut answered: Vec<String> = run(&fixture, &sql)
                .iter()
                .map(|row| row[0].text().expect("text key").to_owned())
                .collect();
            answered.sort();
            assert_eq!(answered, expected, "{sql}");
        }
    }
}

#[test]
fn keys_of_two_collations_stay_apart_across_segments() {
    let mut fixture = fixture();
    // A written row makes the table's segments fold one at a time and
    // merge their finished groups.
    fixture
        .table
        .ingest(vec![part_row(ROWS + 8)])
        .expect("write");
    let mut expected = std::collections::BTreeMap::<(bool, &str), u64>::new();
    for id in (0..ROWS).chain([ROWS + 8]) {
        let maker = MAKERS[usize::try_from(id % 8).expect("small")];
        *expected
            .entry((matches!(maker, "SS" | "ss"), tag_of(id)))
            .or_default() += 1;
    }
    for sql in [
        "SELECT maker, tag, COUNT(*) FROM parts GROUP BY maker, tag",
        "SELECT tag, maker, COUNT(*), MIN(id) FROM parts GROUP BY tag, maker",
    ] {
        let rows = run(&fixture, sql);
        let mut answered = std::collections::BTreeMap::<(bool, String), u64>::new();
        for row in &rows {
            let (maker, tag) = if sql.starts_with("SELECT maker") {
                (&row[0], &row[1])
            } else {
                (&row[1], &row[0])
            };
            let count = match row[2] {
                Value::UInt64(count) => count,
                Value::Int64(count) => u64::try_from(count).expect("a count"),
                ref other => panic!("not a count: {other:?}"),
            };
            let double_s = matches!(maker.text().expect("maker"), "SS" | "ss");
            let earlier = answered.insert((double_s, tag.text().expect("tag").to_owned()), count);
            assert_eq!(earlier, None, "{sql}: one group answered twice");
        }
        let expected: std::collections::BTreeMap<(bool, String), u64> = expected
            .iter()
            .map(|((double_s, tag), count)| ((*double_s, (*tag).to_owned()), *count))
            .collect();
        assert_eq!(answered, expected, "{sql}");
    }
}

/// SUM of `reading` over the rows `keep` passes, added in row order.
fn reading_total(keep: impl Fn(u64) -> bool) -> (f64, u64) {
    (0..ROWS)
        .filter(|id| keep(*id))
        .fold((0.0, 0), |(sum, rows), id| (sum + reading_of(id), rows + 1))
}

fn double_of(value: &Value) -> f64 {
    match value {
        Value::Float64(number) => number.get(),
        other => panic!("not a double cell: {other:?}"),
    }
}

/// The aggregates [`reading_answer`] answers, in its order.
const READING_AGGREGATES: &str = "SUM(reading), AVG(reading), VAR_SAMP(reading), \
                                  VAR_POP(reading), STDDEV_POP(reading)";

/// What the [`READING_AGGREGATES`] answer over the rows `keep` passes, as
/// bits: the sum added in row order, and the variance by the recurrence
/// `MySQL` runs over the rows in that order - the mean moved by each row's
/// distance from it over the count, the spread grown by that distance times
/// the distance from the moved mean, the product and the sum each rounded.
#[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
fn reading_answer(keep: impl Fn(u64) -> bool) -> [u64; 5] {
    let (sum, rows) = reading_total(&keep);
    let (mut count, mut mean, mut spread) = (0_u64, 0.0_f64, 0.0_f64);
    for reading in (0..ROWS).filter(|id| keep(*id)).map(reading_of) {
        count += 1;
        let delta = reading - mean;
        mean += delta / count as f64;
        spread += delta * (reading - mean);
    }
    let population = spread / count as f64;
    [
        sum,
        sum / rows as f64,
        spread / (count - 1) as f64,
        population,
        population.sqrt(),
    ]
    .map(f64::to_bits)
}

/// The last five cells of `row` but one, the [`READING_AGGREGATES`], as bits.
fn reading_cells(row: &[Value]) -> [u64; 5] {
    let first = row.len() - 6;
    std::array::from_fn(|index| double_of(&row[first + index]).to_bits())
}

#[test]
fn a_double_sum_adds_its_rows_in_row_order() {
    let fixture = fixture();
    for (filter, keep) in [
        ("", (|_| true) as fn(u64) -> bool),
        ("WHERE id % 5 <> 1", |id| id % 5 != 1),
        ("WHERE id >= 50000", |id| id >= 50_000),
    ] {
        let answered = run(
            &fixture,
            &format!("SELECT {READING_AGGREGATES}, COUNT(*) FROM parts {filter}"),
        );
        assert_eq!(
            reading_cells(&answered[0]),
            reading_answer(keep),
            "ungrouped {filter}"
        );
        // A plain key, an expression key, a text key, two keys.
        for (key, group_of) in [
            ("crate", (|id| id % CRATES) as fn(u64) -> u64),
            ("crate + 0", |id| id % CRATES),
            ("tag", |id| u64::from(tag_of(id) == "x")),
            ("crate, tag", |id| {
                (id % CRATES) * 2 + u64::from(tag_of(id) == "x")
            }),
        ] {
            let grouped = run(
                &fixture,
                &format!(
                    "SELECT {key}, {READING_AGGREGATES}, COUNT(*) FROM parts {filter} \
                     GROUP BY {key}"
                ),
            );
            let mut answered: Vec<[u64; 5]> =
                grouped.iter().map(|row| reading_cells(row)).collect();
            answered.sort_unstable();
            let mut groups: Vec<u64> = (0..ROWS).filter(|id| keep(*id)).map(group_of).collect();
            groups.sort_unstable();
            groups.dedup();
            let mut expected: Vec<[u64; 5]> = groups
                .into_iter()
                .map(|group| reading_answer(|id| keep(id) && group_of(id) == group))
                .collect();
            expected.sort_unstable();
            assert_eq!(answered, expected, "{key} {filter}");
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn an_integer_sum_whose_running_total_leaves_64_bits_is_its_total() {
    let mut fixture = fixture();
    // Clean segments first, then with a written row, which sends the
    // grouped queries through the segment-at-a-time fold.
    for written in [false, true] {
        if written {
            fixture
                .table
                .ingest(vec![part_row(ROWS + 8)])
                .expect("write");
        }
        for sql in [
            "SELECT SUM(stock) FROM parts",
            "SELECT SUM(stock) FROM parts WHERE id % 1000 = 5",
            "SELECT SUM(stock), COUNT(*) FROM parts WHERE id % 5 = 0",
            "SELECT crate, SUM(stock) FROM parts GROUP BY crate HAVING crate = 5",
            "SELECT crate, SUM(stock) FROM parts WHERE id % 5 = 0 GROUP BY crate HAVING crate = 5",
            "SELECT crate + 0 AS k, SUM(stock) FROM parts GROUP BY k HAVING k = 5",
            "SELECT maker, crate, SUM(stock) FROM parts WHERE crate = 5 GROUP BY maker, crate \
             HAVING SUM(stock) <> 0",
            "SELECT SUM(DISTINCT stock) FROM parts",
        ] {
            let rows = run(&fixture, sql);
            assert_eq!(
                decimal_total(&rows[0]),
                Some(i128::from(i64::MAX)),
                "{sql} (written: {written})"
            );
        }
        // A total that ends outside 64 bits is an ordinary DECIMAL: every
        // fold that adds integers answers it, as do the expressions over
        // it. One query per fold: ungrouped, a plain key, an expression
        // key, two keys, a filter, DISTINCT, a join folded into its probe
        // (grouped and not), a window, and a sum of sums.
        let past = i128::from(i64::MAX) + 1;
        for (sql, expected) in [
            ("SELECT SUM(stock) FROM parts WHERE id < 100000", past),
            (
                "SELECT crate, SUM(stock) FROM parts WHERE id < 100000 GROUP BY crate \
                 HAVING SUM(stock) > 9223372036854775807",
                past,
            ),
            (
                "SELECT crate + 0 AS k, SUM(stock) FROM parts WHERE id < 100000 GROUP BY k \
                 HAVING SUM(stock) <> 0",
                past,
            ),
            (
                "SELECT maker, crate, SUM(stock) FROM parts WHERE id < 100000 AND crate = 5 \
                 GROUP BY maker, crate HAVING SUM(stock) <> 0",
                past,
            ),
            (
                "SELECT SUM(stock) FROM parts WHERE id < 100000 AND id % 5 = 0",
                past,
            ),
            (
                "SELECT SUM(DISTINCT stock) FROM parts WHERE id < 100000",
                past,
            ),
            (
                "SELECT SUM(p.stock) FROM parts p JOIN parts q ON q.id = p.crate \
                 WHERE p.id < 100000",
                past,
            ),
            (
                "SELECT q.crate, SUM(p.stock) FROM parts p JOIN parts q ON q.id = p.crate \
                 WHERE p.id < 100000 GROUP BY q.crate HAVING SUM(p.stock) <> 0",
                past,
            ),
            (
                "SELECT SUM(stock) OVER (ORDER BY id) FROM parts WHERE id IN (5, 60005) \
                 ORDER BY id DESC LIMIT 1",
                past,
            ),
            (
                "SELECT SUM(t.s) FROM (SELECT crate, SUM(stock) AS s FROM parts \
                 WHERE id < 100000 GROUP BY crate) t",
                past,
            ),
            ("SELECT SUM(stock) * 2 FROM parts", past * 2 - 2),
            ("SELECT SUM(stock) + SUM(stock) FROM parts", past * 2 - 2),
            ("SELECT SUM(stock) + 1 FROM parts", past),
        ] {
            let rows = run(&fixture, sql);
            assert_eq!(
                decimal_total(&rows[0]),
                Some(expected),
                "{sql} (written: {written})"
            );
        }
        // Dividing a sum is decimal division, at four more places.
        for (sql, expected) in [
            (
                "SELECT SUM(stock) / 2 FROM parts",
                "4611686018427387903.5000",
            ),
            (
                "SELECT AVG(stock) FROM parts WHERE id IN (5, 60005)",
                "4611686018427387904.0000",
            ),
        ] {
            let rows = run(&fixture, sql);
            assert_eq!(
                rows[0][0].text(),
                Some(expected),
                "{sql} (written: {written})"
            );
        }
    }
}

/// The one cell of `row` that spells an integer past 100: an integer SUM
/// answers as a DECIMAL, whose value is carried as its digits.
fn decimal_total(row: &[Value]) -> Option<i128> {
    row.iter()
        .filter_map(|value| value.text()?.parse::<i128>().ok())
        .find(|total| *total > 100)
}

/// The distinct readings of the rows `keep` selects, added in ascending
/// order: their sum and how many there are.
fn distinct_reading_total(keep: impl Fn(u64) -> bool) -> (f64, usize) {
    let mut distinct: Vec<f64> = (0..ROWS).filter(|id| keep(*id)).map(reading_of).collect();
    distinct.sort_unstable_by(f64::total_cmp);
    distinct.dedup_by(|left, right| left.to_bits() == right.to_bits());
    (distinct.iter().sum(), distinct.len())
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn a_distinct_double_sum_adds_its_values_in_ascending_order() {
    let fixture = fixture();
    let (total, count) = distinct_reading_total(|_| true);
    let rows = run(
        &fixture,
        "SELECT SUM(DISTINCT reading), AVG(DISTINCT reading) FROM parts",
    );
    assert_eq!(double_of(&rows[0][0]).to_bits(), total.to_bits());
    assert_eq!(
        double_of(&rows[0][1]).to_bits(),
        (total / count as f64).to_bits()
    );
    let grouped = run(
        &fixture,
        "SELECT crate, SUM(DISTINCT reading), AVG(DISTINCT reading) FROM parts \
         GROUP BY crate ORDER BY crate",
    );
    assert_eq!(grouped.len(), usize::try_from(CRATES).expect("small"));
    for (group, row) in (0..CRATES).zip(&grouped) {
        let (total, count) = distinct_reading_total(|id| id % CRATES == group);
        assert_eq!(double_of(&row[1]).to_bits(), total.to_bits(), "{group}");
        assert_eq!(
            double_of(&row[2]).to_bits(),
            (total / count as f64).to_bits(),
            "{group}"
        );
    }
}

/// Integer totals of many groups reach what reads them next - a HAVING,
/// an ORDER BY, arithmetic, a window - as packed whole numbers, and each
/// answers what the spelled decimal would: a total at the edge of 64 bits
/// plus one, doubled, divided.
#[test]
fn integer_totals_of_many_groups_compare_and_compute_as_decimals() {
    let fixture = fixture();
    let largest = i128::from(i64::MAX);
    let cells = |sql: &str| -> Vec<Vec<String>> {
        run(&fixture, sql)
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Value::UInt64(number) => number.to_string(),
                        other => other.text().expect("a decimal cell").to_owned(),
                    })
                    .collect()
            })
            .collect()
    };
    assert_eq!(
        cells(
            "SELECT id, SUM(stock) AS s FROM parts GROUP BY id HAVING s > 0 \
             ORDER BY s DESC, id LIMIT 3"
        ),
        [
            vec!["5".to_owned(), largest.to_string()],
            vec!["60005".to_owned(), "1".to_owned()]
        ]
    );
    assert_eq!(
        cells(
            "SELECT id, SUM(stock) + 1, SUM(stock) * 2, SUM(stock) - 1, SUM(stock) / COUNT(*) \
             FROM parts GROUP BY id HAVING SUM(stock) <> 0 ORDER BY id"
        ),
        [
            vec![
                "5".to_owned(),
                (largest + 1).to_string(),
                (largest * 2).to_string(),
                (largest - 1).to_string(),
                format!("{largest}.0000"),
            ],
            vec![
                "60005".to_owned(),
                "2".to_owned(),
                "2".to_owned(),
                "0".to_owned(),
                "1.0000".to_owned()
            ],
            vec![
                "117005".to_owned(),
                "0".to_owned(),
                "-2".to_owned(),
                "-2".to_owned(),
                "-1.0000".to_owned()
            ],
        ]
    );
    // The number first, and a bound no total reaches from either side.
    assert_eq!(
        cells("SELECT id, SUM(stock) AS s FROM parts GROUP BY id HAVING 0 > s"),
        [vec!["117005".to_owned(), "-1".to_owned()]]
    );
    assert_eq!(
        cells(
            "SELECT id, SUM(stock) AS s FROM parts GROUP BY id \
             HAVING s >= 9223372036854775807 OR s < -1"
        ),
        [vec!["5".to_owned(), largest.to_string()]]
    );
    // A running window total: whole numbers while they fit 64 bits, and
    // the one past them beside them in the same column.
    assert_eq!(
        cells(
            "SELECT id, SUM(stock) OVER (ORDER BY id) FROM parts \
             WHERE id IN (4, 5, 60005, 117005) ORDER BY id"
        ),
        [
            vec!["4".to_owned(), "0".to_owned()],
            vec!["5".to_owned(), largest.to_string()],
            vec!["60005".to_owned(), (largest + 1).to_string()],
            vec!["117005".to_owned(), largest.to_string()],
        ]
    );
}
