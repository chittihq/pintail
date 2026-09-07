//! In-process timing of the aggregate paths over a large table, with the
//! settled memo off so every run executes. Ignored: it is a measurement,
//! not a gate. Run with
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --release -p pintail-exec
//!  --test morsel_bench -- --ignored --nocapture`.
//! `PINTAIL_BENCH_ROWS` overrides the fact-table size.
use std::time::{Duration, Instant};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const STATUSES: [&str; 5] = ["new", "open", "held", "done", "void"];
const CHUNK: u64 = 1_000_000;

struct Fixture {
    _directory: tempfile::TempDir,
    facts: TableStore,
    dims: TableStore,
    catalog: CatalogSnapshot,
}

fn fact_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "grp", DataType::Int64, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

fn dim_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "name", DataType::Utf8, false),
        ],
    )
    .expect("schema")
}

fn fact_row(id: u64, dim_rows: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(i64::try_from(id % dim_rows).expect("small")),
            Value::Utf8(STATUSES[usize::try_from(id % 5).expect("small")].to_owned()),
            Value::Int64(i64::try_from(id % 1000).expect("small")),
        ],
        1,
        false,
    )
}

impl Fixture {
    fn new(rows: u64) -> Self {
        Self::with_dims(rows, 50)
    }

    /// `dim_rows` is both the dimension table's row count and the fact
    /// table's join-key cardinality: the fused join-aggregate's dense
    /// build-side table and its pre-resolved group index are sized to it,
    /// so a case that wants to measure that path at Q8's scale (~100K
    /// distinct users, not 50) needs this rather than `new`.
    fn with_dims(rows: u64, dim_rows: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut facts = TableStore::open(
            directory.path().join("facts"),
            fact_schema(),
            StoreOptions::default(),
        )
        .expect("facts");
        let mut next = 1;
        while next <= rows {
            let end = (next + CHUNK - 1).min(rows);
            facts
                .bulk_ingest_snapshot((next..=end).map(|id| fact_row(id, dim_rows)).collect())
                .expect("ingest");
            next = end + 1;
        }
        let mut dims = TableStore::open(
            directory.path().join("dims"),
            dim_schema(),
            StoreOptions::default(),
        )
        .expect("dims");
        dims.bulk_ingest_snapshot(
            (0..i64::try_from(dim_rows).expect("dim_rows fits i64"))
                .map(|id| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
                        vec![Value::Int64(id), Value::Utf8(format!("region-{}", id % 8))],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest dims");
        let facts_entry = TableEntry::new(
            TableId::new(1),
            "facts",
            fact_schema(),
            TableStatistics::with_row_count(rows),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let dims_entry = TableEntry::new(
            TableId::new(2),
            "dims",
            dim_schema(),
            TableStatistics::with_row_count(dim_rows),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [facts_entry, dims_entry])
            .expect("database");
        Self {
            _directory: directory,
            facts,
            dims,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(&self, sql: &str, limit: usize) -> Result<(usize, Duration), String> {
        let facts = self.facts.snapshot();
        let dims = self.dims.snapshot();
        let provider = SnapshotScanProvider::new([
            (DatabaseId::new(1), TableId::new(1), &facts),
            (DatabaseId::new(1), TableId::new(2), &dims),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&parse_statement(sql).expect("parse"))
            .expect("bind");
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("plan");
        let clock = Instant::now();
        let mut execution = if std::env::var_os("PINTAIL_BENCH_PROFILE").is_some() {
            Execution::start_profiled(physical, &provider, limit, None, Collation::default())
        } else {
            Execution::start(physical, &provider, limit, Collation::default())
        }
        .map_err(|error| error.to_string())?;
        let mut rows = 0;
        loop {
            match execution.next_batch() {
                Ok(Some(batch)) => rows += batch.visible_row_count(),
                Ok(None) => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        let elapsed = clock.elapsed();
        if let Some(profile) = execution.profile() {
            eprintln!("{}", profile.render());
        }
        Ok((rows, elapsed))
    }
}

struct Case {
    label: &'static str,
    sql: &'static str,
    limit: usize,
}

const CASES: &[Case] = &[
    Case {
        label: "predicate count single",
        sql: "SELECT COUNT(*) FROM facts WHERE grp = 2",
        limit: 512 << 20,
    },
    Case {
        label: "predicate count conjunction",
        sql: "SELECT COUNT(*) FROM facts WHERE id >= 1 AND grp = 2",
        limit: 512 << 20,
    },
    Case {
        label: "predicate single",
        sql: "SELECT id, grp FROM facts WHERE grp = 2",
        limit: 512 << 20,
    },
    Case {
        label: "predicate conjunction",
        sql: "SELECT id, grp FROM facts WHERE id >= 1 AND grp = 2",
        limit: 512 << 20,
    },
    Case {
        label: "scan: filtered count",
        // Q2's shape (benchmark/queries.ts): almost pure scan - the
        // aggregate itself is one counter - so its time is the scan pool's
        // own width and I/O overlap, not anything downstream.
        sql: "SELECT COUNT(*) FROM facts WHERE status = 'open'",
        limit: 512 << 20,
    },
    Case {
        label: "two-pass int key",
        sql: "SELECT grp, COUNT(*), SUM(amount) FROM facts GROUP BY grp",
        limit: 512 << 20,
    },
    Case {
        label: "two-pass text key",
        sql: "SELECT status, COUNT(*), SUM(amount) FROM facts GROUP BY status",
        limit: 512 << 20,
    },
    Case {
        label: "general int+text keys",
        sql: "SELECT grp, status, COUNT(*), SUM(amount) FROM facts GROUP BY grp, status",
        limit: 512 << 20,
    },
    Case {
        label: "general int+text keys, 64MiB",
        sql: "SELECT grp, status, COUNT(*), SUM(amount) FROM facts GROUP BY grp, status",
        limit: 64 << 20,
    },
    Case {
        label: "general expression key",
        sql: "SELECT grp % 10 AS g, COUNT(*), SUM(amount) FROM facts GROUP BY g",
        limit: 512 << 20,
    },
    Case {
        label: "general high cardinality",
        sql: "SELECT id % 200000 AS g, COUNT(*), SUM(amount) FROM facts GROUP BY g",
        limit: 512 << 20,
    },
    Case {
        label: "fused join + group",
        sql: "SELECT d.name, COUNT(*), SUM(f.amount) FROM facts f JOIN dims d ON f.grp = d.id \
              GROUP BY d.name",
        limit: 512 << 20,
    },
    Case {
        label: "count distinct, 100K-value column",
        // Q7's shape (benchmark/queries.ts): a handful of groups, each
        // counting distinct values of a column whose real cardinality
        // (100K) is far higher than the group count.
        sql: "SELECT status, COUNT(*), COUNT(DISTINCT id % 100000) FROM facts GROUP BY status",
        limit: 512 << 20,
    },
];

/// Q8's own shape: a dense integer build key with real-world cardinality
/// (100K users, like `benchmark/queries.ts`), grouped by a build-side
/// column that folds to a handful of groups (8 regions). The 50-row `dims`
/// case above shares the query text but not the scale that makes the
/// per-probe-row bucket-address lookup worth precomputing once per key.
const WIDE_JOIN_CASE: Case = Case {
    label: "fused join + group, 100K-key dim",
    sql: "SELECT d.name, COUNT(*), SUM(f.amount) FROM facts f JOIN dims d ON f.grp = d.id \
          GROUP BY d.name",
    limit: 512 << 20,
};

/// Q6's own shape (benchmark/queries.ts): `GROUP BY` a bare high-cardinality
/// int COLUMN (not an expression - `grp` is stored, not computed here) with
/// `COUNT(*)`/`SUM`, `ORDER BY` the sum, `LIMIT 10`. The "general high
/// cardinality" case above groups by an EXPRESSION (`id % 200000`), which
/// `column_index()` cannot resolve to a plain column and so never reaches
/// the direct/two-pass paths at all - it is not what Q6 runs.
const TOP_K_CASE: Case = Case {
    label: "top 10 by sum, 200K-value column",
    sql: "SELECT grp, COUNT(*) AS order_count, SUM(amount) AS total_spent FROM facts \
          GROUP BY grp ORDER BY total_spent DESC, grp LIMIT 10",
    limit: 512 << 20,
};

fn measure(fixture: &Fixture, case: &Case, runs: usize) -> String {
    let mut times = Vec::with_capacity(runs);
    let mut rows = 0;
    for _ in 0..runs {
        match fixture.run(case.sql, case.limit) {
            Ok((count, elapsed)) => {
                rows = count;
                times.push(elapsed);
            }
            Err(error) => return format!("{:<34} ERROR {error}", case.label),
        }
    }
    times.sort();
    let min = times[0].as_secs_f64() * 1e3;
    let median = times[times.len() / 2].as_secs_f64() * 1e3;
    let max = times[times.len() - 1].as_secs_f64() * 1e3;
    format!(
        "{:<34} rows={rows:<8} min={min:>8.1}ms  median={median:>8.1}ms  max={max:>8.1}ms",
        case.label
    )
}

#[test]
#[ignore = "a measurement over a large in-process table, not a gate"]
fn aggregate_paths_over_a_large_table() {
    let rows = std::env::var("PINTAIL_BENCH_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000_000_u64);
    let runs = std::env::var("PINTAIL_BENCH_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_usize);
    let clock = Instant::now();
    let fixture = Fixture::new(rows);
    eprintln!(
        "ingested {rows} rows in {:.1}s; {} rayon threads; memo {}",
        clock.elapsed().as_secs_f64(),
        rayon::current_num_threads(),
        if std::env::var_os("PINTAIL_DISABLE_SETTLED_MEMO").is_some() {
            "off"
        } else {
            "ON (set PINTAIL_DISABLE_SETTLED_MEMO=1)"
        }
    );
    // Warm the page cache once so the first case is not charged for it.
    let _ = fixture.run("SELECT COUNT(*) FROM facts", 512 << 20);
    let only = std::env::var("PINTAIL_BENCH_CASE").ok();
    let wanted = |case: &&Case| {
        only.as_ref()
            .is_none_or(|only| case.label.contains(only.as_str()))
    };
    for case in CASES.iter().filter(wanted) {
        eprintln!("{}", measure(&fixture, case, runs));
    }
    let small = Fixture::new(150_000);
    for case in CASES
        .iter()
        .filter(wanted)
        .filter(|case| case.limit == 512 << 20)
    {
        eprintln!("[150K rows] {}", measure(&small, case, runs));
    }
    if wanted(&&WIDE_JOIN_CASE) {
        let wide = Fixture::with_dims(rows, 100_000);
        eprintln!("{}", measure(&wide, &WIDE_JOIN_CASE, runs));
    }
    if wanted(&&TOP_K_CASE) {
        let wide = Fixture::with_dims(rows, 200_000);
        eprintln!("{}", measure(&wide, &TOP_K_CASE, runs));
    }
}
