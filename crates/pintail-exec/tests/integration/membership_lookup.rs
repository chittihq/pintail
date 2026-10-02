//! `column IN (SELECT key FROM t ..)` under a `LIMIT` looks each row's
//! value up by `t`'s key until the limit has its rows, a constant tested
//! for membership asks the subquery about that constant alone, a limit in
//! a scan's own key order stops the scan, and a key lookup join turns into
//! values only the driving rows it joins. Each answers exactly what the
//! built set, the whole subquery, the whole scan and the whole join answer.

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    ExecCounters, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    take_exec_counters,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableSnapshot, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const TICKETS: i64 = 60_000;
const AGENTS: i64 = 40_000;

fn database() -> DatabaseId {
    DatabaseId::new(1)
}

fn tickets_id() -> TableId {
    TableId::new(1)
}

fn agents_id() -> TableId {
    TableId::new(2)
}

fn schema(id: u32, owned: bool) -> TableSchema {
    let mut columns = vec![
        Column::new(1, "id", DataType::Int64, false),
        Column::new(2, "name", DataType::Utf8, false),
    ];
    if owned {
        columns.push(Column::new(3, "agent_id", DataType::Int64, true));
    }
    TableSchema::new(id, columns).expect("schema")
}

/// Several tickets share an agent, every seventh has none, and every
/// eleventh names an agent that does not exist.
fn agent_of(id: i64) -> Value {
    if id % 7 == 0 {
        Value::Null
    } else if id % 11 == 0 {
        Value::Int64(AGENTS + id)
    } else {
        Value::Int64((id * 13) % 500 + 1)
    }
}

fn ticket(id: i64, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            Value::Utf8(format!("ticket-{id}")),
            agent_of(id),
        ],
        version,
        false,
    )
}

fn agent(id: i64, name: &str, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![Value::Int64(id), Value::Utf8(name.to_owned())],
        version,
        deleted,
    )
}

fn agent_name(id: i64) -> String {
    if id % 1_000 == 0 {
        "lead".to_owned()
    } else {
        format!("agent-{id}")
    }
}

struct Answer {
    rows: Vec<String>,
    plan: String,
    agents_blocks: usize,
    counters: ExecCounters,
}

struct Fixture {
    tickets: TableSnapshot,
    agents: TableSnapshot,
    catalog: CatalogSnapshot,
    _stores: (TableStore, TableStore),
    _directories: (tempfile::TempDir, tempfile::TempDir),
}

impl Fixture {
    fn new() -> Self {
        let options = || StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let tickets_directory = tempfile::tempdir().expect("tickets directory");
        let agents_directory = tempfile::tempdir().expect("agents directory");
        let mut tickets = TableStore::open(tickets_directory.path(), schema(1, true), options())
            .expect("tickets");
        let mut agents =
            TableStore::open(agents_directory.path(), schema(2, false), options()).expect("agents");
        tickets
            .bulk_ingest_snapshot((1..=TICKETS).map(|id| ticket(id, 1)).collect())
            .expect("tickets snapshot");
        // Agents 1..=AGENTS, with every id ending in 3 absent.
        agents
            .bulk_ingest_snapshot(
                (1..=AGENTS)
                    .filter(|id| id % 10 != 3)
                    .map(|id| agent(id, &agent_name(id), 1, false))
                    .collect(),
            )
            .expect("agents snapshot");
        // Replicated writes still in the memtable: a delete, a rename and
        // an agent the snapshot never held.
        agents
            .ingest(vec![
                agent(4, &agent_name(4), 2, true),
                agent(5, "renamed-5", 2, false),
                agent(13, "late", 2, false),
            ])
            .expect("agent writes");
        let entries = [
            TableEntry::new(
                tickets_id(),
                "tickets",
                schema(1, true),
                TableStatistics::with_row_count(TICKETS.cast_unsigned()),
            )
            .expect("tickets entry")
            .with_key_columns([1])
            .expect("tickets key"),
            TableEntry::new(
                agents_id(),
                "agents",
                schema(2, false),
                TableStatistics::with_row_count(AGENTS.cast_unsigned()),
            )
            .expect("agents entry")
            .with_key_columns([1])
            .expect("agents key"),
        ];
        let catalog =
            CatalogSnapshot::new([DatabaseEntry::new(database(), "desk", entries).expect("desk")])
                .expect("catalog");
        Self {
            tickets: tickets.snapshot(),
            agents: agents.snapshot(),
            catalog,
            _stores: (tickets, agents),
            _directories: (tickets_directory, agents_directory),
        }
    }

    fn run(&self, sql: &str) -> Answer {
        let provider = SnapshotScanProvider::new([
            (database(), tickets_id(), &self.tickets),
            (database(), agents_id(), &self.agents),
        ])
        .expect("provider");
        let bound = Binder::new(&self.catalog, Some("desk"))
            .bind(&parse_statement(sql).expect("parse"))
            .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
        let physical = PhysicalPlanner::plan(
            Optimizer::optimize(LogicalPlanner::plan(bound)),
            Collation::default(),
        )
        .expect("physical plan");
        let plan = format!("{physical:?}");
        let _ = take_exec_counters();
        let mut execution =
            Execution::start(physical, &provider, 256 * 1024 * 1024, Collation::default())
                .unwrap_or_else(|error| panic!("start {sql}: {error}"));
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("pull {sql}: {error}"))
        {
            for row in batch.selection().selected_rows() {
                let values = batch
                    .columns()
                    .iter()
                    .map(|column| column.value(row).expect("value"))
                    .collect::<Vec<_>>();
                rows.push(format!("{values:?}"));
            }
        }
        drop(execution);
        Answer {
            rows,
            plan,
            agents_blocks: provider
                .scan_stats(database(), agents_id())
                .unwrap_or_default()
                .blocks_read,
            counters: take_exec_counters(),
        }
    }
}

fn looked_up(plan: &str) -> bool {
    plan.contains("KeyLookupJoin") && plan.contains("kind: Semi")
}

/// Each statement ordered by `id`, which the scan yields, and by
/// `-id DESC`, the same order through an expression it cannot, so the
/// second answer comes from the built set and a sort of every row kept.
const LOOKED_UP: [&str; 12] = [
    // The first rows match: one round answers.
    "SELECT id, name FROM tickets WHERE id IN (SELECT id FROM agents WHERE id >= 2) \
     ORDER BY {key} LIMIT 2",
    // The limit's last row sits beside keys the subquery lacks (3, 4, 13
    // and 23 are absent, deleted, late and absent).
    "SELECT id, name FROM tickets WHERE id IN (SELECT id FROM agents) ORDER BY {key} LIMIT 3",
    "SELECT id, name FROM tickets WHERE id IN (SELECT id FROM agents) ORDER BY {key} LIMIT 11",
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE id >= 10) \
     ORDER BY {key} LIMIT 4 OFFSET 9",
    // Past every round of ranged reads, into the keys read once.
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents) ORDER BY {key} LIMIT 20 OFFSET 900",
    // Few members, far apart: the limit is never reached.
    "SELECT id, name FROM tickets WHERE id IN (SELECT id FROM agents WHERE name = 'lead') \
     ORDER BY {key} LIMIT 500",
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE name = 'late') \
     ORDER BY {key} LIMIT 5",
    // No member at all.
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE id < 0) \
     ORDER BY {key} LIMIT 5",
    // Other conjuncts stay with the scan.
    "SELECT id FROM tickets WHERE name <> 'ticket-6' AND id IN (SELECT id FROM agents) \
     AND id > 4 ORDER BY {key} LIMIT 6",
    // A column that is not the key: NULLs, values shared by many rows and
    // values no agent has.
    "SELECT id, agent_id FROM tickets WHERE agent_id IN (SELECT id FROM agents) \
     ORDER BY {key} LIMIT 40",
    "SELECT id, agent_id FROM tickets WHERE agent_id IN (SELECT id FROM agents WHERE id > 490) \
     ORDER BY {key} LIMIT 7 OFFSET 3",
    // Members past the end of the driving table.
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE id > 39990) \
     ORDER BY {key} LIMIT 50",
];

#[test]
fn a_limited_membership_test_answers_as_the_built_set_does() {
    let fixture = Fixture::new();
    for template in LOOKED_UP {
        let fast = fixture.run(&template.replace("{key}", "id"));
        let reference = fixture.run(&template.replace("{key}", "-id DESC"));
        assert!(!looked_up(&reference.plan), "{template}");
        assert!(looked_up(&fast.plan), "{template}: {}", fast.plan);
        assert!(fast.counters.membership_rows_looked_up > 0, "{template}");
        assert_eq!(
            reference.counters.membership_rows_looked_up, 0,
            "{template}"
        );
        assert_eq!(fast.rows, reference.rows, "{template}");
    }
}

#[test]
fn a_membership_test_without_an_order_answers_the_same_rows() {
    let fixture = Fixture::new();
    let fast = fixture
        .run("SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE id > 20) LIMIT 6");
    assert!(looked_up(&fast.plan), "{}", fast.plan);
    assert_eq!(
        fast.rows,
        ["21", "22", "24", "25", "26", "27"].map(|id| format!("[Int64({id})]"))
    );
}

/// Statements the lookup must leave alone, each answered as before.
const LEFT_ALONE: [&str; 6] = [
    // A row is kept for what the set lacks.
    "SELECT id FROM tickets WHERE id NOT IN (SELECT id FROM agents) ORDER BY id LIMIT 5",
    // Not the subquery table's key.
    "SELECT id FROM tickets WHERE id IN (SELECT agent_id FROM tickets) ORDER BY id LIMIT 5",
    // More than a table read.
    "SELECT id FROM tickets WHERE id IN (SELECT MAX(id) FROM agents) ORDER BY id LIMIT 5",
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents ORDER BY id LIMIT 3) \
     ORDER BY id LIMIT 5",
    // Most of the table: looked up row by row it costs more than the set.
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents) ORDER BY id LIMIT 30000",
    // No limit to stop at.
    "SELECT id FROM tickets WHERE id IN (SELECT id FROM agents WHERE id < 9) ORDER BY id",
];

#[test]
fn other_membership_tests_keep_their_set() {
    let fixture = Fixture::new();
    for sql in LEFT_ALONE {
        let answer = fixture.run(sql);
        assert!(!looked_up(&answer.plan), "{sql}: {}", answer.plan);
        assert_eq!(answer.counters.membership_rows_looked_up, 0, "{sql}");
        assert!(!answer.rows.is_empty(), "{sql}");
    }
    let absent = fixture
        .run("SELECT id FROM tickets WHERE id NOT IN (SELECT id FROM agents) ORDER BY id LIMIT 5");
    assert_eq!(
        absent.rows,
        ["3", "4", "23", "33", "43"].map(|id| format!("[Int64({id})]"))
    );
}

#[test]
fn a_small_limit_tests_a_few_rows_and_reads_a_few_blocks() {
    let fixture = Fixture::new();
    let template = "SELECT id, name FROM tickets WHERE id IN (SELECT id FROM agents WHERE id >= 2) \
                    ORDER BY {key} LIMIT 2";
    let fast = fixture.run(&template.replace("{key}", "id"));
    let reference = fixture.run(&template.replace("{key}", "-id DESC"));
    assert!(
        fast.counters.membership_rows_looked_up <= 16,
        "{} rows tested",
        fast.counters.membership_rows_looked_up
    );
    assert!(
        fast.agents_blocks * 4 <= reference.agents_blocks,
        "the lookup read {} blocks of agents against {} for the set",
        fast.agents_blocks,
        reference.agents_blocks
    );
}

#[test]
fn a_constant_tested_for_membership_asks_about_that_constant() {
    let fixture = Fixture::new();
    // 5 is present, 4 deleted, 3 never there, 13 arrived late.
    let constants = fixture.run(
        "SELECT 5 IN (SELECT id FROM agents), 4 IN (SELECT id FROM agents), \
         3 IN (SELECT id FROM agents), 13 IN (SELECT id FROM agents WHERE name = 'late'), \
         5 NOT IN (SELECT id FROM agents), 4 NOT IN (SELECT id FROM agents)",
    );
    assert_eq!(constants.counters.membership_point_queries, 6);
    assert_eq!(
        constants.rows,
        [
            "[Boolean(true), Boolean(false), Boolean(false), Boolean(true), Boolean(false), \
          Boolean(true)]"
        ]
    );
    let whole =
        fixture.run("SELECT id FROM tickets WHERE id IN (SELECT id FROM agents) AND id < 3");
    assert!(
        constants.agents_blocks * 2 <= whole.agents_blocks,
        "six constants read {} blocks of agents against {} for the set",
        constants.agents_blocks,
        whole.agents_blocks
    );
    // A row pinned to one key asks about that key.
    for (id, held) in [(5, true), (4, false), (13, true), (23, false)] {
        let pinned = fixture.run(&format!(
            "SELECT id, id IN (SELECT id FROM agents WHERE id >= 4) FROM tickets WHERE id = {id} \
             ORDER BY id"
        ));
        assert_eq!(pinned.counters.membership_point_queries, 1, "{id}");
        assert_eq!(pinned.rows, [format!("[Int64({id}), Boolean({held})]")]);
    }
    // A NULL among the rows makes a miss unknown, so the whole set is read.
    let nullable =
        fixture.run("SELECT 5 IN (SELECT agent_id FROM tickets), NULL IN (SELECT id FROM agents)");
    assert_eq!(nullable.counters.membership_point_queries, 0);
    // A set that is more than rows of one table is read whole too.
    for sql in [
        "SELECT 5 IN (SELECT id FROM agents ORDER BY id DESC LIMIT 3)",
        "SELECT 5 IN (SELECT MAX(id) FROM agents)",
        "SELECT 5 IN (SELECT id FROM agents UNION ALL SELECT id FROM tickets)",
    ] {
        assert_eq!(
            fixture.run(sql).counters.membership_point_queries,
            0,
            "{sql}"
        );
    }
}

#[test]
fn a_limit_in_key_order_stops_the_scan() {
    let fixture = Fixture::new();
    for template in [
        "SELECT id, name FROM agents ORDER BY {key} LIMIT 3 OFFSET 4",
        "SELECT name FROM agents ORDER BY {key} LIMIT 12",
        "SELECT id, CONCAT(name, '!') FROM agents ORDER BY {key} LIMIT 1 OFFSET 2",
    ] {
        let fast = fixture.run(&template.replace("{key}", "id"));
        let reference = fixture.run(&template.replace("{key}", "-id DESC"));
        assert!(
            fast.plan.contains("limit: Some("),
            "{template}: {}",
            fast.plan
        );
        assert!(!reference.plan.contains("limit: Some("), "{template}");
        assert_eq!(fast.rows, reference.rows, "{template}");
    }
    // A predicate drops rows the scan would have counted.
    let filtered = fixture.run("SELECT id FROM agents WHERE name <> 'agent-1' ORDER BY id LIMIT 2");
    assert!(!filtered.plan.contains("limit: Some("), "{}", filtered.plan);
    assert_eq!(filtered.rows, ["[Int64(2)]", "[Int64(5)]"]);
}

#[test]
fn a_lookup_join_turns_only_the_rows_it_joins_into_values() {
    let fixture = Fixture::new();
    let template = "SELECT t.id, t.name, a.name FROM tickets AS t INNER JOIN agents AS a \
                    ON t.id = a.id WHERE t.id >= 2 ORDER BY {key} LIMIT 2";
    let fast = fixture.run(&template.replace("{key}", "t.id"));
    let reference = fixture.run(&template.replace("{key}", "-t.id DESC"));
    assert!(fast.plan.contains("KeyLookupJoin"), "{}", fast.plan);
    assert_eq!(fast.rows, reference.rows);
    assert!(
        (1..=64).contains(&fast.counters.lookup_rows_joined),
        "{} driving rows joined",
        fast.counters.lookup_rows_joined
    );
    // A limit the first rounds do not satisfy keeps reading where it left
    // off: tickets 2..=60000 hold 35999 agents' ids up to 40000.
    let long = fixture.run(
        "SELECT t.id FROM tickets AS t INNER JOIN agents AS a ON t.id = a.id \
         WHERE t.id >= 2 ORDER BY t.id LIMIT 3 OFFSET 2000",
    );
    assert!(long.plan.contains("KeyLookupJoin"), "{}", long.plan);
    let reference = fixture.run(
        "SELECT t.id FROM tickets AS t INNER JOIN agents AS a ON t.id = a.id \
         WHERE t.id >= 2 ORDER BY -t.id DESC LIMIT 3 OFFSET 2000",
    );
    assert_eq!(long.rows, reference.rows);
}
