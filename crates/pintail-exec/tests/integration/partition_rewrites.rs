//! Metamorphic checks run in ordinary push CI without a source database.
//! Bags preserve duplicate multiplicity, NULLs, and complete projected values.
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, PhysicalScanStats, SnapshotScanProvider,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

struct Fixture {
    _directory: tempfile::TempDir,
    table: TableStore,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new(seed: u64) -> Self {
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "n", DataType::Int64, true),
                Column::new(3, "tag", DataType::Utf8, true),
            ],
        )
        .expect("schema");
        let directory = tempfile::tempdir().expect("directory");
        let mut table = TableStore::open(
            directory.path(),
            schema.clone(),
            StoreOptions {
                block_rows: 4,
                background_compaction: false,
                ..StoreOptions::default()
            },
        )
        .expect("table");
        let mut state = seed;
        let rows: Vec<_> = (0..96_u64)
            .map(|id| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let n = if id % 7 == 0 {
                    Value::Null
                } else {
                    Value::Int64(i64::try_from(state % 17).expect("small") - 8)
                };
                let tag = match id % 5 {
                    0 => Value::Null,
                    1 => Value::Utf8(String::new()),
                    2 => Value::Utf8("NULL".into()),
                    _ => Value::Utf8("same".into()),
                };
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![Value::UInt64(id), n, tag],
                    id + 1,
                    false,
                )
            })
            .collect();
        for segment in rows.chunks(24) {
            table
                .bulk_ingest_snapshot(segment.to_vec())
                .expect("ingest segment");
        }
        let entry = TableEntry::new(
            TableId::new(1),
            "samples",
            schema,
            TableStatistics::with_row_count(96),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [entry]).expect("database");
        Self {
            _directory: directory,
            table,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn query(&self, sql: &str) -> Vec<String> {
        self.query_with_optimizer(sql, true).0
    }

    fn query_with_optimizer(&self, sql: &str, optimize: bool) -> (Vec<String>, PhysicalScanStats) {
        let snapshot = self.table.snapshot();
        let provider =
            SnapshotScanProvider::new([(DatabaseId::new(1), TableId::new(1), &snapshot)])
                .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let logical = LogicalPlanner::plan(bound);
        // Unoptimized scans receive no pushed predicates, so they read all
        // segments/blocks and evaluate the original relational filter above.
        let logical = if optimize {
            Optimizer::optimize(logical)
        } else {
            logical
        };
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 64 * 1024 * 1024, Collation::default())
                .expect("execution");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
        {
            for row in batch.selection().selected_rows() {
                let values: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect();
                rows.push(format!("{values:?}"));
            }
        }
        rows.sort();
        (
            rows,
            provider
                .scan_stats(DatabaseId::new(1), TableId::new(1))
                .unwrap_or_default(),
        )
    }

    fn partition(&self, projection: &str, from: &str, predicate: &str) {
        let whole = self.query(&format!("SELECT {projection} FROM {from}"));
        let mut parts = Vec::new();
        for condition in [
            format!("({predicate})"),
            format!("NOT ({predicate})"),
            format!("({predicate}) IS NULL"),
        ] {
            parts.extend(self.query(&format!(
                "SELECT {projection} FROM {from} WHERE {condition}"
            )));
        }
        parts.sort();
        assert_eq!(
            whole, parts,
            "partition {predicate}; projection {projection}; from {from}"
        );
    }
}

#[test]
fn nullable_predicate_partitions_preserve_every_value_and_duplicate() {
    for seed in [1, 953, 65_537, u64::MAX] {
        let fixture = Fixture::new(seed);
        for pivot in [-9, -1, 0, 3, 9] {
            for predicate in [
                format!("n > {pivot}"),
                format!("n = {pivot} OR tag = 'same'"),
                format!("n BETWEEN {pivot} AND 5 AND tag <> ''"),
                format!("n IN ({pivot}, 3, NULL)"),
            ] {
                fixture.partition("n, tag", "samples", &predicate);
            }
        }
        for predicate in [
            "n IS NULL",
            "tag IS NULL",
            "NOT (n > 0 AND tag = 'same')",
            "TRUE",
            "FALSE",
            "NULL",
        ] {
            fixture.partition("n, tag", "samples", predicate);
        }
    }
}

#[test]
fn outer_join_partitions_preserve_null_extended_rows() {
    let fixture = Fixture::new(953);
    for predicate in ["a.n > b.n", "b.id IS NULL", "a.tag = b.tag OR b.n < 0"] {
        fixture.partition(
            "a.n, b.tag",
            "samples a LEFT JOIN samples b ON a.id = b.id + 1",
            predicate,
        );
    }
}

#[test]
fn equivalent_filter_group_and_order_rewrites_agree() {
    let fixture = Fixture::new(953);
    for (left, right) in [
        (
            "SELECT n,tag FROM samples WHERE NOT (n > 0 OR tag = 'same')",
            "SELECT n,tag FROM samples WHERE NOT (n > 0) AND NOT (tag = 'same')",
        ),
        (
            "SELECT n,tag FROM samples WHERE n BETWEEN -2 AND 3",
            "SELECT n,tag FROM samples WHERE n >= -2 AND n <= 3",
        ),
        (
            "SELECT n,tag FROM samples WHERE n IN (-2, 0, 3)",
            "SELECT n,tag FROM samples WHERE n = -2 OR n = 0 OR n = 3",
        ),
        (
            "SELECT n,tag FROM samples WHERE n > 0",
            "SELECT n,tag FROM (SELECT n,tag FROM samples) s WHERE n > 0",
        ),
        (
            "SELECT tag,COUNT(*),SUM(n) FROM samples WHERE tag IS NOT NULL GROUP BY tag",
            "SELECT tag,COUNT(*),SUM(n) FROM samples GROUP BY tag HAVING tag IS NOT NULL",
        ),
        (
            "SELECT DISTINCT n,tag FROM samples",
            "SELECT n,tag FROM samples GROUP BY n,tag",
        ),
        (
            "SELECT n,tag FROM samples ORDER BY id",
            "SELECT n,tag FROM samples ORDER BY id DESC",
        ),
    ] {
        assert_eq!(
            fixture.query(left),
            fixture.query(right),
            "rewrite: {left} versus {right}"
        );
    }
}

#[test]
fn pruning_and_full_scan_agree_across_segments_and_overlapping_versions() {
    let mut fixture = Fixture::new(953);
    let selective = "SELECT id,n,tag FROM samples WHERE id >= 36 AND id < 38";
    let (pruned, stats) = fixture.query_with_optimizer(selective, true);
    let (full, reference) = fixture.query_with_optimizer(selective, false);
    assert_eq!(pruned, full);
    assert_eq!(pruned.len(), 2);
    assert!(
        stats.segments_pruned > 0,
        "segment pruning must engage: {stats:?}"
    );
    assert!(
        stats.blocks_pruned > 0,
        "block pruning must engage: {stats:?}"
    );
    assert_eq!(reference.segments_pruned, 0);
    assert_eq!(reference.blocks_pruned, 0);
    assert!(reference.segments_read > stats.segments_read);
    assert!(reference.blocks_read > stats.blocks_read);

    for overlap in [false, true] {
        if overlap {
            fixture
                .table
                .ingest(vec![
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(0)]).expect("key"),
                        vec![
                            Value::UInt64(0),
                            Value::Int64(20),
                            Value::Utf8("moved".into()),
                        ],
                        100,
                        false,
                    ),
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(36)]).expect("key"),
                        vec![Value::UInt64(36), Value::Null, Value::Null],
                        101,
                        true,
                    ),
                ])
                .expect("overlapping update and tombstone");
            fixture.table.flush().expect("flush overlap");
        }
        for sql in [
            selective,
            "SELECT n,tag FROM samples WHERE n > 3",
            "SELECT n,tag FROM samples WHERE n IS NULL OR n < -3",
            "SELECT n,tag FROM samples WHERE tag = 'same' AND id BETWEEN 30 AND 65",
            "SELECT tag,COUNT(*),SUM(n) FROM samples WHERE id >= 30 GROUP BY tag",
        ] {
            assert_eq!(
                fixture.query_with_optimizer(sql, true).0,
                fixture.query_with_optimizer(sql, false).0,
                "overlap={overlap}: {sql}"
            );
        }
    }
}

#[test]
fn membership_and_inner_join_permutations_preserve_nulls_and_duplicates() {
    let fixture = Fixture::new(65_537);
    for (left, right) in [
        (
            "SELECT a.n,a.tag FROM samples a WHERE EXISTS (SELECT 1 FROM samples b WHERE b.n=a.n AND b.id<40)",
            "SELECT a.n,a.tag FROM samples a WHERE a.n IN (SELECT b.n FROM samples b WHERE b.id<40)",
        ),
        (
            "SELECT a.n,b.tag FROM samples a INNER JOIN samples b ON a.n=b.n WHERE a.id<8 AND b.id<40",
            "SELECT a.n,b.tag FROM samples b INNER JOIN samples a ON b.n=a.n WHERE b.id<40 AND a.id<8",
        ),
        (
            "SELECT a.n,b.tag,c.n FROM samples a JOIN samples b ON a.id=b.id JOIN samples c ON b.n=c.n WHERE a.id<8 AND c.id<20",
            "SELECT a.n,b.tag,c.n FROM samples c JOIN samples b ON c.n=b.n JOIN samples a ON b.id=a.id WHERE a.id<8 AND c.id<20",
        ),
    ] {
        let expected = fixture.query(left);
        assert!(
            !expected.is_empty(),
            "rewrite fixture must exercise matches"
        );
        assert_eq!(expected, fixture.query(right), "{left} versus {right}");
    }
}
