//! The fused inner join-aggregate's column fold against the general join
//! and aggregate operators, on a star join whose dimension key is dense.
//!
//! Each fused query has a twin whose ON clause adds a cross-side predicate
//! that is always true: a residual sends the join to the general operator
//! and the grouping to the general aggregate, so the twin's answer is
//! computed by code the fold does not share. The fact key carries NULLs,
//! negatives, values past both ends of the dimension's range and values in
//! its gaps; the measures carry NULLs and negatives.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const FACTS: i64 = 150_000;
const DIMS: i64 = 300;
const ZONES: [&str; 5] = ["amber", "basalt", "cobalt", "dune", "ember"];

fn decimal() -> DataType {
    DataType::Decimal {
        precision: 12,
        scale: 2,
    }
}

fn fact_key(id: i64) -> Option<i64> {
    // -5..=320: below, inside (with gaps) and above the dimension's keys.
    (id % 13 != 0).then_some((id * 37) % 326 - 5)
}

fn fact_amount(id: i64) -> Option<String> {
    (id % 11 != 0).then(|| {
        let cents = (id * 7_919) % 200_000 - 50_000;
        let sign = if cents < 0 { "-" } else { "" };
        format!("{sign}{}.{:02}", cents.abs() / 100, cents.abs() % 100)
    })
}

fn dim_present(id: i64) -> bool {
    id % 7 != 0
}

fn zone(id: i64) -> &'static str {
    ZONES[usize::try_from(id % 5).expect("small")]
}

struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<TableStore>,
    catalog: CatalogSnapshot,
}

fn table(
    directory: &std::path::Path,
    name: &str,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
) -> (TableStore, TableSchema, u64) {
    let schema = TableSchema::new(1, columns).expect("schema");
    let mut store = TableStore::open(
        directory.join(name),
        schema.clone(),
        StoreOptions::default(),
    )
    .expect("store");
    let count = u64::try_from(rows.len()).expect("rows");
    store
        .bulk_ingest_snapshot(
            rows.into_iter()
                .enumerate()
                .map(|(index, values)| {
                    let key = u64::try_from(index + 1).expect("key");
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(key)]).expect("key"),
                        values,
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
    (store, schema, count)
}

impl Fixture {
    #[allow(clippy::too_many_lines)]
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let optional = |value: Option<Value>| value.unwrap_or(Value::Null);
        let facts = table(
            directory.path(),
            "facts",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "dim_id", DataType::Int64, true),
                Column::new(3, "amount", decimal(), true),
                Column::new(4, "flag", DataType::Int64, false),
            ],
            (1..=FACTS)
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        optional(fact_key(id).map(Value::Int64)),
                        optional(fact_amount(id).map(Value::Utf8)),
                        Value::Int64(id % 3),
                    ]
                })
                .collect(),
        );
        let dims = table(
            directory.path(),
            "dims",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "zone", DataType::Utf8, false),
            ],
            (1..=DIMS)
                .filter(|id| dim_present(*id))
                .map(|id| vec![Value::Int64(id), Value::Utf8(zone(id).to_owned())])
                .collect(),
        );
        // Two rows per key: the fold must leave these to the row fold.
        let pairs = table(
            directory.path(),
            "pairs",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "k", DataType::Int64, false),
                Column::new(3, "zone", DataType::Utf8, false),
            ],
            (1..=2 * DIMS)
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        Value::Int64(id % DIMS),
                        Value::Utf8(zone(id).to_owned()),
                    ]
                })
                .collect(),
        );
        // A dimension whose group column is sometimes NULL: under a LEFT
        // join those rows share a group with the unmatched probe rows.
        let spots = table(
            directory.path(),
            "spots",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "zone", DataType::Utf8, true),
            ],
            (1..=DIMS)
                .filter(|id| dim_present(*id))
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        if id % 4 == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(zone(id).to_owned())
                        },
                    ]
                })
                .collect(),
        );
        // A dimension whose labels differ only in letter case from row to
        // row: under the default collation each pair is one group.
        let tones = table(
            directory.path(),
            "tones",
            vec![
                Column::new(1, "id", DataType::Int64, false),
                Column::new(2, "zone", DataType::Utf8, false),
            ],
            (1..=DIMS)
                .filter(|id| dim_present(*id))
                .map(|id| {
                    let label = if (id / 5) % 2 == 0 {
                        zone(id).to_owned()
                    } else {
                        zone(id).to_uppercase()
                    };
                    vec![Value::Int64(id), Value::Utf8(label)]
                })
                .collect(),
        );
        let mut stores = Vec::new();
        let mut entries = Vec::new();
        for (index, (name, (store, schema, count))) in [
            ("facts", facts),
            ("dims", dims),
            ("pairs", pairs),
            ("spots", spots),
            ("tones", tones),
        ]
        .into_iter()
        .enumerate()
        {
            entries.push(
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

    fn run(&self, sql: &str) -> (Vec<String>, String) {
        let snapshots: Vec<_> = self.stores.iter().map(TableStore::snapshot).collect();
        let provider = SnapshotScanProvider::new(snapshots.iter().enumerate().map(|(i, s)| {
            (
                DatabaseId::new(1),
                TableId::new(u64::try_from(i + 1).expect("table")),
                s,
            )
        }))
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let plan = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let mut execution =
            Execution::start_profiled(plan, &provider, 1 << 31, None, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                rows.push(
                    (0..batch.columns().len())
                        .map(|column| {
                            let value = batch
                                .column(column)
                                .and_then(|column| column.value_owned(row))
                                .expect("value");
                            value
                                .text()
                                .map_or_else(|| format!("{value:?}"), str::to_owned)
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                );
            }
        }
        let profile = execution
            .profile()
            .map(|profile| profile.render())
            .unwrap_or_default();
        (rows, profile)
    }
}

/// The fused join's operator line: annotated as fused, and never pulled,
/// since the aggregate reads its inputs directly.
fn fused_join_line(profile: &str) -> Option<&str> {
    profile
        .lines()
        .find(|line| line.trim_start().starts_with("HashJoin"))
        .filter(|line| line.contains("fused into the aggregate above") && line.contains(" rows=0 "))
}

/// Runs `select ... {join} ... {rest}` fused and through its residual twin,
/// and checks the two agree and that each took the path it is meant to.
fn agree(fixture: &Fixture, select: &str, join: &str, rest: &str) -> Vec<String> {
    let fused_sql = format!("{select} {join} {rest}");
    let twin_sql = format!("{select} {join} AND f.id + d.id > 0 {rest}");
    let (fused, fused_profile) = fixture.run(&fused_sql);
    let (twin, twin_profile) = fixture.run(&twin_sql);
    assert!(
        fused_join_line(&fused_profile).is_some(),
        "{fused_sql} did not fuse:\n{fused_profile}"
    );
    assert!(
        fused_join_line(&twin_profile).is_none() && twin_profile.contains("residual=true"),
        "{twin_sql} fused:\n{twin_profile}"
    );
    assert_eq!(fused, twin, "{fused_sql}");
    assert!(!fused.is_empty());
    fused
}

/// Every lane at once, over a unique dense key with NULL, negative, gap and
/// out-of-range probe keys and NULL measures.
#[test]
fn counts_sums_and_averages_match_the_general_operators() {
    let fixture = Fixture::new();
    let rows = agree(
        &fixture,
        "SELECT d.zone, COUNT(*), COUNT(f.amount), SUM(f.amount), AVG(f.amount) FROM facts f",
        "JOIN dims d ON f.dim_id = d.id",
        "GROUP BY d.zone ORDER BY d.zone",
    );
    // Independently of both engines' aggregate states: the counts.
    let mut expected = std::collections::BTreeMap::<&str, (u64, u64)>::new();
    for id in 1..=FACTS {
        let Some(key) = fact_key(id) else { continue };
        if !(1..=DIMS).contains(&key) || !dim_present(key) {
            continue;
        }
        let entry = expected.entry(zone(key)).or_default();
        entry.0 += 1;
        entry.1 += u64::from(fact_amount(id).is_some());
    }
    let counts: Vec<String> = rows
        .iter()
        .map(|row| row.split('|').take(3).collect::<Vec<_>>().join("|"))
        .collect();
    let expected: Vec<String> = expected
        .iter()
        .map(|(zone, (rows, valid))| format!("{zone}|UInt64({rows})|UInt64({valid})"))
        .collect();
    assert_eq!(counts, expected);
}

/// A filter on the probe side leaves a partial selection for the fold.
#[test]
fn a_filtered_probe_folds_only_its_selected_rows() {
    let fixture = Fixture::new();
    agree(
        &fixture,
        "SELECT d.zone, COUNT(*), SUM(f.amount) FROM facts f",
        "JOIN dims d ON f.dim_id = d.id",
        "WHERE f.flag = 1 GROUP BY d.zone ORDER BY d.zone",
    );
}

/// A key naming two build rows counts each probe row twice.
#[test]
fn a_repeated_build_key_folds_every_matching_row() {
    let fixture = Fixture::new();
    let rows = agree(
        &fixture,
        "SELECT d.zone, COUNT(*), SUM(f.amount) FROM facts f",
        "JOIN pairs d ON f.dim_id = d.k",
        "GROUP BY d.zone ORDER BY d.zone",
    );
    // Both answers share the build, so the counts are also taken here:
    // every key in 0..DIMS names two rows of one zone.
    let mut expected = std::collections::BTreeMap::<&str, u64>::new();
    for id in 1..=FACTS {
        if let Some(key) = fact_key(id).filter(|key| (0..DIMS).contains(key)) {
            *expected.entry(zone(key)).or_default() += 2;
        }
    }
    let counts: Vec<String> = rows
        .iter()
        .map(|row| row.split('|').take(2).collect::<Vec<_>>().join("|"))
        .collect();
    let expected: Vec<String> = expected
        .iter()
        .map(|(zone, rows)| format!("{zone}|UInt64({rows})"))
        .collect();
    assert_eq!(counts, expected);
    // The general join, without an aggregate to fuse into.
    let (rows, _) = fixture.run(
        "SELECT f.id, d.id FROM facts f JOIN pairs d ON f.dim_id = d.k \
         WHERE f.id <= 2000 ORDER BY f.id, d.id",
    );
    let mut joined = Vec::new();
    for id in 1..=2000 {
        if let Some(key) = fact_key(id).filter(|key| (0..DIMS).contains(key)) {
            let first = if key == 0 { DIMS } else { key };
            joined.push(format!("{id}|{first}"));
            joined.push(format!("{id}|{}", first + DIMS));
        }
    }
    let rows: Vec<String> = rows
        .iter()
        .map(|row| row.replace("Int64(", "").replace(')', ""))
        .collect();
    assert_eq!(rows, joined);
}

/// Aggregates without a lane - a build-side argument, MIN -
/// keep the row fold and still agree.
#[test]
fn aggregates_without_a_lane_keep_the_row_fold() {
    let fixture = Fixture::new();
    agree(
        &fixture,
        "SELECT d.zone, COUNT(*), MIN(f.amount), SUM(d.id), AVG(d.id) FROM facts f",
        "JOIN dims d ON f.dim_id = d.id",
        "GROUP BY d.zone ORDER BY d.zone",
    );
}

/// The row fold keeps its states per worker for the whole probe when no
/// aggregate's answer depends on the order its rows are folded in, opens
/// them per morsel when one does, and says which on the join's line.
#[test]
fn the_row_fold_says_how_it_kept_its_states() {
    let fixture = Fixture::new();
    for (select, group, kept) in [
        (
            "SELECT d.id, COUNT(*), MIN(f.amount), MAX(f.amount), SUM(f.amount), \
             AVG(f.amount), SUM(f.flag), MAX(d.id) FROM facts f",
            "d.id",
            "states kept per worker",
        ),
        (
            "SELECT d.id, COUNT(*), MAX(d.zone), MIN(f.amount) FROM facts f",
            "d.id",
            "states opened per morsel: an extreme of text",
        ),
        (
            "SELECT d.zone, COUNT(*), MIN(f.amount), MAX(d.id) FROM facts f",
            "d.zone",
            "states kept per worker",
        ),
    ] {
        for join in ["JOIN", "LEFT JOIN"] {
            let join = format!("{join} dims d ON f.dim_id = d.id");
            let rest = format!("GROUP BY {group} ORDER BY {group}");
            let fused_sql = format!("{select} {join} {rest}");
            let (fused, profile) = fixture.run(&fused_sql);
            assert!(
                fused_join_line(&profile).is_some_and(|line| line.contains(kept)),
                "{profile}"
            );
            let (twin, _) = fixture.run(&format!("{select} {join} AND f.id + d.id > 0 {rest}"));
            assert_eq!(fused, twin, "{fused_sql}");
            assert!(!fused.is_empty());
        }
    }
}

/// A LEFT join keeps the probe rows that match nothing - NULL keys, gaps,
/// keys past either end - in one group whose build columns are NULL.
#[test]
fn a_left_join_folds_unmatched_rows_into_the_null_group() {
    let fixture = Fixture::new();
    let rows = agree(
        &fixture,
        "SELECT d.zone, COUNT(*), COUNT(f.amount), SUM(f.amount), AVG(f.amount) FROM facts f",
        "LEFT JOIN dims d ON f.dim_id = d.id",
        "GROUP BY d.zone ORDER BY d.zone",
    );
    let unmatched = (1..=FACTS)
        .filter(|id| {
            fact_key(*id).is_none_or(|key| !(1..=DIMS).contains(&key) || !dim_present(key))
        })
        .count();
    assert!(
        rows[0].starts_with(&format!("Null|UInt64({unmatched})|")),
        "{rows:?}"
    );
}

/// The same with the row fold: build-side arguments are NULL for the
/// unmatched rows, a filtered probe and a repeated build key.
#[test]
fn a_left_join_row_fold_reads_null_build_columns() {
    let fixture = Fixture::new();
    agree(
        &fixture,
        "SELECT d.zone, COUNT(*), COUNT(d.id), SUM(d.id), MIN(f.amount) FROM facts f",
        "LEFT JOIN dims d ON f.dim_id = d.id",
        "WHERE f.flag = 2 GROUP BY d.zone ORDER BY d.zone",
    );
    agree(
        &fixture,
        "SELECT d.zone, COUNT(*), SUM(f.amount), MAX(d.id) FROM facts f",
        "LEFT JOIN pairs d ON f.dim_id = d.k",
        "GROUP BY d.zone ORDER BY d.zone",
    );
}

/// A build group whose column is NULL merges with the unmatched rows.
#[test]
fn a_left_join_merges_a_null_build_group_with_the_unmatched_rows() {
    let fixture = Fixture::new();
    agree(
        &fixture,
        "SELECT d.zone, COUNT(*), SUM(f.amount) FROM facts f",
        "LEFT JOIN spots d ON f.dim_id = d.id",
        "GROUP BY d.zone ORDER BY d.zone",
    );
}

/// Labels equal under the group column's collation are one group, under an
/// inner join and beside an outer join's NULL group.
#[test]
fn labels_equal_under_the_collation_are_one_group() {
    let fixture = Fixture::new();
    for (join, groups) in [("JOIN", ZONES.len()), ("LEFT JOIN", ZONES.len() + 1)] {
        let rows = agree(
            &fixture,
            "SELECT COUNT(*), COUNT(f.amount), SUM(f.amount), AVG(f.amount) FROM facts f",
            &format!("{join} tones d ON f.dim_id = d.id"),
            "GROUP BY d.zone ORDER BY 1, 2, 3",
        );
        assert_eq!(rows.len(), groups, "{join}: {rows:?}");
    }
}

/// Semi and anti joins on a key with NULLs, gaps and out-of-range values,
/// against counts taken here.
#[test]
fn semi_and_anti_joins_count_what_the_keys_say() {
    let fixture = Fixture::new();
    let matched =
        |id: &i64| fact_key(*id).is_some_and(|key| (1..=DIMS).contains(&key) && dim_present(key));
    let semi = (1..=FACTS).filter(matched).count();
    let keyed = (1..=FACTS).filter(|id| fact_key(*id).is_some()).count();
    for (sql, expected) in [
        (
            "SELECT COUNT(*) FROM facts f WHERE f.dim_id IN (SELECT id FROM dims)",
            semi,
        ),
        (
            "SELECT COUNT(*) FROM facts f WHERE EXISTS (SELECT 1 FROM dims d WHERE d.id = f.dim_id)",
            semi,
        ),
        (
            "SELECT COUNT(*) FROM facts f WHERE f.dim_id NOT IN (SELECT id FROM dims)",
            keyed - semi,
        ),
        (
            "SELECT COUNT(*) FROM facts f WHERE NOT EXISTS \
             (SELECT 1 FROM dims d WHERE d.id = f.dim_id)",
            usize::try_from(FACTS).expect("rows") - semi,
        ),
    ] {
        let (rows, _) = fixture.run(sql);
        assert_eq!(rows, vec![format!("UInt64({expected})")], "{sql}");
    }
}

/// A probe key computed per row reaches the dense build by position, as a
/// plain column does: the build keeps no bucket addresses to look it up by.
#[test]
fn a_computed_probe_key_reaches_a_dense_build() {
    let fixture = Fixture::new();
    for key in ["f.dim_id + 0", "f.dim_id % 1000000"] {
        agree(
            &fixture,
            "SELECT d.zone, COUNT(*), COUNT(f.amount), SUM(f.amount) FROM facts f",
            &format!("JOIN dims d ON {key} = d.id"),
            "GROUP BY d.zone ORDER BY d.zone",
        );
    }
}
