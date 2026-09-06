//! Production-shaped SQL at scale, under the shipped container's limits.
//!
//! The oracle's generated families stop at three-table joins with no CTE
//! chains, the E2E corpus is about a thousand rows, and the benchmark runs
//! eight hand-picked queries. Nothing ran the shape that actually fails in
//! deployments: a grouped report over a ten-way LEFT JOIN chain from a
//! filtered driving table, with datetime predicates, COUNT(DISTINCT),
//! windows over the grouped result and NOT EXISTS, on enough rows that the
//! joins and the aggregate have to spill under the compose file's default
//! per-query ceiling and its tighter siblings.
//!
//! The schema is invented. Every query runs three times: at a roomy ceiling
//! for the reference answer, at the compose file's default ceiling, and at
//! a ceiling that forces the joins and the aggregate to disk. All three must
//! agree exactly, and the tight run must actually have spilled.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::spill::QuerySpillMetrics;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE: DatabaseId = DatabaseId::new(1);

/// The compose file's default per-query ceiling.
const CONTAINER_DEFAULT: usize = 512 * 1024 * 1024;
/// Roomy: nothing spills, the reference answer.
const ROOMY: usize = 2048 * 1024 * 1024;
/// Tight: the build sides and the group map go to disk.
const TIGHT: usize = 24 * 1024 * 1024;

struct Spec {
    id: u64,
    name: &'static str,
    columns: Vec<Column>,
    rows: u64,
    row: fn(u64) -> Vec<Value>,
}

fn column(id: u32, name: &str, data_type: DataType) -> Column {
    Column::new(id, name, data_type, true)
}

fn datetime(day: u64, hour: u64) -> Value {
    Value::Utf8(format!(
        "2026-{:02}-{:02} {:02}:00:00",
        1 + (day / 28) % 12,
        1 + day % 28,
        hour % 24
    ))
}

fn int(value: u64) -> Value {
    Value::Int64(i64::try_from(value).expect("small"))
}

const ORGS: u64 = 50;
const COHORTS: u64 = 2_000;
const SESSIONS: u64 = 30_000;
const MEMBERS: u64 = 40_000;
const TASKS: u64 = 60_000;
const MEMBER_TASKS: u64 = 220_000;
const ATTENDANCE: u64 = 160_000;
const GRADES: u64 = 60_000;
const NOTES: u64 = 15_000;
const TAGS: u64 = 100;
const MEMBER_TAGS: u64 = 40_000;

#[allow(clippy::too_many_lines)] // one table per block, read top to bottom
fn specs() -> Vec<Spec> {
    let dt = DataType::DateTime64 { fsp: 0 };
    vec![
        Spec {
            id: 1,
            name: "org",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "name", DataType::Utf8),
            ],
            rows: ORGS,
            row: |i| vec![Value::UInt64(i), Value::Utf8(format!("org-{i}"))],
        },
        Spec {
            id: 2,
            name: "cohort",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "org_id", DataType::UInt64),
                column(3, "name", DataType::Utf8),
            ],
            rows: COHORTS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % ORGS),
                    Value::Utf8(format!("cohort-{i}")),
                ]
            },
        },
        Spec {
            id: 3,
            name: "session",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "cohort_id", DataType::UInt64),
                column(3, "starts_at", dt),
                column(4, "title", DataType::Utf8),
            ],
            rows: SESSIONS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % COHORTS),
                    datetime(i % 365, i),
                    Value::Utf8(format!("session-{i}")),
                ]
            },
        },
        Spec {
            id: 4,
            name: "member",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "cohort_id", DataType::UInt64),
                column(3, "name", DataType::Utf8),
                column(4, "joined_at", dt),
            ],
            rows: MEMBERS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % COHORTS),
                    Value::Utf8(format!("member-{i}")),
                    datetime(i % 300, 9),
                ]
            },
        },
        Spec {
            id: 5,
            name: "task",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "session_id", DataType::UInt64),
                column(3, "kind", DataType::Utf8),
            ],
            rows: TASKS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % SESSIONS),
                    Value::Utf8(format!("kind-{}", i % 7)),
                ]
            },
        },
        Spec {
            id: 6,
            name: "member_task",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "member_id", DataType::UInt64),
                column(3, "task_id", DataType::UInt64),
                column(4, "status", DataType::Utf8),
                column(5, "score", DataType::Int64),
                column(6, "done_at", dt),
            ],
            rows: MEMBER_TASKS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % MEMBERS),
                    Value::UInt64(1 + (i * 7) % TASKS),
                    Value::Utf8(["open", "done", "late"][(i % 3) as usize].to_owned()),
                    int(i % 100),
                    if i % 11 == 0 {
                        Value::Null
                    } else {
                        datetime(i % 365, i)
                    },
                ]
            },
        },
        Spec {
            id: 7,
            name: "attendance",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "session_id", DataType::UInt64),
                column(3, "member_id", DataType::UInt64),
                column(4, "present", DataType::Int64),
            ],
            rows: ATTENDANCE,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % SESSIONS),
                    Value::UInt64(1 + (i * 3) % MEMBERS),
                    int(u64::from(i % 5 != 0)),
                ]
            },
        },
        Spec {
            id: 8,
            name: "grade",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "member_task_id", DataType::UInt64),
                column(3, "value", DataType::Int64),
            ],
            rows: GRADES,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + (i * 4) % MEMBER_TASKS),
                    int(i % 10),
                ]
            },
        },
        Spec {
            id: 9,
            name: "note",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "member_id", DataType::UInt64),
                column(3, "body", DataType::Utf8),
            ],
            rows: NOTES,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % MEMBERS),
                    Value::Utf8(format!("note body number {i} with some length to it")),
                ]
            },
        },
        Spec {
            id: 10,
            name: "tag",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "name", DataType::Utf8),
            ],
            rows: TAGS,
            row: |i| vec![Value::UInt64(i), Value::Utf8(format!("tag-{i}"))],
        },
        Spec {
            id: 11,
            name: "member_tag",
            columns: vec![
                column(1, "id", DataType::UInt64),
                column(2, "member_id", DataType::UInt64),
                column(3, "tag_id", DataType::UInt64),
            ],
            rows: MEMBER_TAGS,
            row: |i| {
                vec![
                    Value::UInt64(i),
                    Value::UInt64(1 + i % MEMBERS),
                    Value::UInt64(1 + i % TAGS),
                ]
            },
        },
    ]
}

struct Fixture {
    _directory: tempfile::TempDir,
    stores: Vec<(u64, TableStore)>,
    catalog: CatalogSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let mut stores = Vec::new();
        let mut entries = Vec::new();
        for spec in specs() {
            let schema = TableSchema::new(1, spec.columns).expect("schema");
            let path = directory.path().join(spec.name);
            std::fs::create_dir_all(&path).expect("table directory");
            let mut store =
                TableStore::open(&path, schema.clone(), StoreOptions::default()).expect("open");
            let rows = (1..=spec.rows)
                .map(|i| {
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::UInt64(i)]).expect("key"),
                        (spec.row)(i),
                        i,
                        false,
                    )
                })
                .collect();
            store.bulk_ingest_snapshot(rows).expect("ingest");
            entries.push(
                TableEntry::new(
                    TableId::new(spec.id),
                    spec.name,
                    schema,
                    TableStatistics::with_row_count(spec.rows),
                )
                .expect("entry")
                .with_key_columns([1])
                .expect("key"),
            );
            stores.push((spec.id, store));
        }
        let database = DatabaseEntry::new(DATABASE, "app", entries).expect("database");
        Self {
            _directory: directory,
            stores,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
        }
    }

    fn run(
        &self,
        sql: &str,
        memory_limit: usize,
        optimize: bool,
    ) -> Result<(Vec<String>, QuerySpillMetrics), String> {
        let snapshots: Vec<_> = self
            .stores
            .iter()
            .map(|(id, store)| (*id, store.snapshot()))
            .collect();
        let provider = SnapshotScanProvider::new(
            snapshots
                .iter()
                .map(|(id, snapshot)| (DATABASE, TableId::new(*id), snapshot)),
        )
        .expect("provider");
        let statement = parse_statement(sql).map_err(|error| format!("parse: {error}"))?;
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .map_err(|error| format!("bind: {error}"))?;
        let logical = LogicalPlanner::plan(bound);
        let logical = if optimize {
            Optimizer::optimize(logical)
        } else {
            logical
        };
        let physical = PhysicalPlanner::plan(logical, Collation::default())
            .map_err(|error| format!("plan: {error}"))?;
        let mut execution =
            Execution::start(physical, &provider, memory_limit, Collation::default())
                .map_err(|error| format!("start: {error}"))?;
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .map_err(|error| format!("execute: {error}"))?
        {
            for index in batch.selection().selected_rows() {
                let values: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(index).expect("value"))
                    .collect();
                rows.push(format!("{values:?}"));
            }
        }
        Ok((rows, execution.spill_metrics()))
    }
}

/// The report shapes. Each starts from a driving table narrowed by a
/// literal, walks the chain through keys no literal reaches, and groups.
/// The flag says whether the tight ceiling applies; every shape takes it
/// now that the two-pass aggregate path spills as well.
const REPORTS: &[(&str, &str, bool)] = &[
    (
        "ten-way left join chain grouped by cohort",
        "SELECT c.id, c.name, COUNT(*) AS n, COUNT(DISTINCT m.id) AS members, \
                COUNT(DISTINCT s.id) AS sessions, SUM(mt.score) AS score, \
                AVG(g.value) AS grade, MIN(s.starts_at) AS first_session, \
                MAX(mt.done_at) AS last_done, COUNT(DISTINCT tg.name) AS tags \
         FROM cohort c \
         LEFT JOIN member m ON m.cohort_id = c.id \
         LEFT JOIN member_task mt ON mt.member_id = m.id \
         LEFT JOIN task t ON t.id = mt.task_id \
         LEFT JOIN session s ON s.id = t.session_id \
         LEFT JOIN attendance a ON a.member_id = m.id AND a.session_id = s.id \
         LEFT JOIN grade g ON g.member_task_id = mt.id \
         LEFT JOIN note n ON n.member_id = m.id \
         LEFT JOIN member_tag mg ON mg.member_id = m.id \
         LEFT JOIN tag tg ON tg.id = mg.tag_id \
         LEFT JOIN org o ON o.id = c.org_id \
         WHERE c.org_id = 7 AND (s.starts_at IS NULL OR DATE(s.starts_at) >= '2026-03-01') \
         GROUP BY c.id, c.name ORDER BY c.id",
        true,
    ),
    (
        "window over the grouped chain, ranked within the driving filter",
        "SELECT id, members, score, ROW_NUMBER() OVER (ORDER BY score DESC, id) AS rank_in_org, \
                SUM(score) OVER () AS org_total \
         FROM ( \
           SELECT c.id, COUNT(DISTINCT m.id) AS members, COALESCE(SUM(mt.score), 0) AS score \
           FROM cohort c \
           LEFT JOIN member m ON m.cohort_id = c.id \
           LEFT JOIN member_task mt ON mt.member_id = m.id AND mt.status <> 'open' \
           LEFT JOIN task t ON t.id = mt.task_id \
           LEFT JOIN session s ON s.id = t.session_id \
           WHERE c.org_id = 12 AND DATE(m.joined_at) BETWEEN '2026-01-01' AND '2026-12-31' \
           GROUP BY c.id \
         ) ranked ORDER BY rank_in_org",
        true,
    ),
    (
        "members without attendance in their sessions, with a two-level CTE",
        "WITH sessions_of AS ( \
           SELECT s.id AS session_id, s.cohort_id, s.starts_at FROM session s \
           JOIN cohort c ON c.id = s.cohort_id WHERE c.org_id = 3 \
         ), absent AS ( \
           SELECT m.id AS member_id, so.session_id \
           FROM member m JOIN sessions_of so ON so.cohort_id = m.cohort_id \
           WHERE NOT EXISTS ( \
             SELECT 1 FROM attendance a \
             WHERE a.member_id = m.id AND a.session_id = so.session_id AND a.present = 1 \
           ) \
         ) \
         SELECT ab.member_id, COUNT(*) AS missed, MIN(so.starts_at) AS first_missed \
         FROM absent ab JOIN sessions_of so ON so.session_id = ab.session_id \
         GROUP BY ab.member_id HAVING COUNT(*) >= 2 ORDER BY missed DESC, ab.member_id LIMIT 200",
        true,
    ),
    (
        "the chain for half the organisations, wide enough to spill",
        "SELECT o.id AS org, COUNT(*) AS n, COUNT(DISTINCT m.id) AS members, \
                COUNT(DISTINCT mt.id) AS tasks_done, SUM(mt.score) AS score, \
                COUNT(DISTINCT a.id) AS attendances, MAX(s.starts_at) AS last_session \
         FROM cohort c \
         JOIN org o ON o.id = c.org_id \
         LEFT JOIN member m ON m.cohort_id = c.id \
         LEFT JOIN member_task mt ON mt.member_id = m.id AND mt.status = 'done' \
         LEFT JOIN task t ON t.id = mt.task_id \
         LEFT JOIN session s ON s.id = t.session_id \
         LEFT JOIN attendance a ON a.member_id = m.id \
         WHERE c.org_id BETWEEN 1 AND 25 \
         GROUP BY o.id ORDER BY o.id",
        true,
    ),
    (
        "per-member activity summary: forty thousand groups with distinct sets",
        "SELECT m.id, m.cohort_id, COUNT(DISTINCT mt.task_id) AS tasks, \
                COUNT(DISTINCT a.session_id) AS sessions_attended, \
                SUM(mt.score) AS score, MAX(mt.done_at) AS last_done \
         FROM member m \
         LEFT JOIN member_task mt ON mt.member_id = m.id \
         LEFT JOIN attendance a ON a.member_id = m.id AND a.present = 1 \
         GROUP BY m.id, m.cohort_id ORDER BY score DESC, m.id LIMIT 500",
        true,
    ),
    (
        "grouped by status and month with a having clause",
        "SELECT mt.status, MONTH(mt.done_at) AS month, COUNT(*) AS n, \
                COUNT(DISTINCT mt.member_id) AS members, AVG(mt.score) AS score \
         FROM member_task mt \
         JOIN member m ON m.id = mt.member_id \
         JOIN cohort c ON c.id = m.cohort_id \
         LEFT JOIN grade g ON g.member_task_id = mt.id \
         WHERE c.org_id IN (5, 6) AND mt.done_at IS NOT NULL AND g.id IS NULL \
         GROUP BY mt.status, MONTH(mt.done_at) HAVING COUNT(*) > 10 \
         ORDER BY mt.status, month",
        true,
    ),
];

/// Set in the child that runs the suite with the settled aggregate memo
/// off. With the memo on, every run after the first of a query over a
/// settled snapshot is a replay of the first, so the ceilings would never
/// be exercised and the answers would agree trivially. The switch is an
/// environment variable read at execution time, and a test cannot set one
/// for itself without unsafe code, so the suite runs in a child.
const REPORT_CHILD: &str = "PINTAIL_REPORT_SHAPES_CHILD";

#[test]
fn report_shapes_answer_the_same_under_every_ceiling_and_spill_under_the_tight_one() {
    const NAME: &str =
        "report_shapes_answer_the_same_under_every_ceiling_and_spill_under_the_tight_one";
    if std::env::var_os(REPORT_CHILD).is_some() {
        run_report_shapes();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(REPORT_CHILD, "1")
        .env("PINTAIL_DISABLE_SETTLED_MEMO", "1")
        .output()
        .expect("spawn the report child");
    assert!(
        output.status.success(),
        "report child failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_report_shapes() {
    let fixture = Fixture::new();
    let mut failures = Vec::new();
    let mut spilled_somewhere = false;
    for (name, sql, tight_applies) in REPORTS {
        let (reference, reference_metrics) = match fixture.run(sql, ROOMY, true) {
            Ok(result) => result,
            Err(error) => {
                failures.push(format!("{name}: reference run failed: {error}"));
                continue;
            }
        };
        assert!(
            !reference.is_empty(),
            "{name}: the reference answer is empty, the shape proves nothing"
        );
        if reference_metrics.files > 0 {
            failures.push(format!("{name}: the roomy reference spilled"));
        }
        // The unoptimized plan is a second, independent path to the same
        // answer: no predicate pushdown, no temporal rewrite, no join
        // constant propagation.
        match fixture.run(sql, ROOMY, false) {
            Ok((rows, _)) if rows == reference => {}
            Ok(_) => failures.push(format!("{name}: the unoptimized plan answers differently")),
            Err(error) => failures.push(format!("{name}: unoptimized run failed: {error}")),
        }
        let mut ceilings = vec![("container default", CONTAINER_DEFAULT)];
        if *tight_applies {
            ceilings.push(("tight", TIGHT));
        }
        for (label, ceiling) in ceilings {
            match fixture.run(sql, ceiling, true) {
                Ok((rows, metrics)) => {
                    if rows != reference {
                        failures.push(format!(
                            "{name}: answers differently at the {label} ceiling"
                        ));
                    }
                    if ceiling == TIGHT && metrics.files > 0 {
                        spilled_somewhere = true;
                        if metrics.peak_handles > 17 {
                            failures.push(format!(
                                "{name}: held {} spill files open at once",
                                metrics.peak_handles
                            ));
                        }
                    }
                    if metrics.active_handles != 0 || metrics.active_bytes != 0 {
                        failures.push(format!("{name}: spill files still held after the last row"));
                    }
                }
                Err(error) => {
                    failures.push(format!("{name}: failed at the {label} ceiling: {error}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "report shapes that did not hold:\n  {}",
        failures.join("\n  ")
    );
    assert!(
        spilled_somewhere,
        "no report spilled at the tight ceiling; the shapes are not exercising the disk path"
    );
}
