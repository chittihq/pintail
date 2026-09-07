//! Queries while replication is live: the primary state of a mirrored
//! table is a segment set with the latest changes still in the memtable,
//! flushes and compactions landing between queries. A generated sequence
//! of change batches, stale replays, flushes and compactions runs over two
//! tables while a fixed set of query shapes (scans, filters, lookups,
//! grouping, a join, a limit) is checked after every step against an
//! in-memory model, on a table small enough for the materialized path and
//! one large enough for the streaming path with the memtable overlay.
use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const STATUSES: [&str; 5] = ["done", "held", "new", "open", "void"];
const DIMS: i64 = 200;

fn fact_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "dim_id", DataType::Int64, false),
            Column::new(3, "status", DataType::Utf8, false),
            Column::new(4, "amount", DataType::Int64, false),
            Column::new(5, "note", DataType::Utf8, true),
        ],
    )
    .expect("fact schema")
}

fn dim_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::Int64, false),
            Column::new(2, "name", DataType::Utf8, false),
            Column::new(3, "grp", DataType::Int64, false),
        ],
    )
    .expect("dim schema")
}

#[derive(Clone, Debug, PartialEq)]
struct Fact {
    dim_id: i64,
    status: usize,
    amount: i64,
    note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Dim {
    name: String,
    grp: i64,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

fn fact_row(id: u64, fact: &Fact, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        vec![
            Value::UInt64(id),
            Value::Int64(fact.dim_id),
            Value::Utf8(STATUSES[fact.status].to_owned()),
            Value::Int64(fact.amount),
            fact.note.clone().map_or(Value::Null, Value::Utf8),
        ],
        version,
        deleted,
    )
}

fn dim_row(id: i64, dim: &Dim, version: u64) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            Value::Utf8(dim.name.clone()),
            Value::Int64(dim.grp),
        ],
        version,
        false,
    )
}

fn random_fact(rng: &mut Rng, id: u64) -> Fact {
    Fact {
        dim_id: i64::try_from(rng.below(u64::try_from(DIMS).expect("dims"))).expect("dim"),
        status: usize::try_from(rng.below(5)).expect("status"),
        amount: i64::try_from(rng.below(1_000)).expect("amount") - 200,
        note: id.is_multiple_of(3).then(|| format!("note-{id}")),
    }
}

struct World {
    _directory: tempfile::TempDir,
    facts: TableStore,
    dims: TableStore,
    fact_model: BTreeMap<u64, Fact>,
    dim_model: BTreeMap<i64, Dim>,
    /// The version of each key's newest write, and whether that write has
    /// been flushed: a stale replay picks a flushed key and replays a
    /// strictly older version, which must lose to the flushed one.
    key_versions: BTreeMap<u64, u64>,
    flushed: std::collections::BTreeSet<u64>,
    version: u64,
    next_id: u64,
    catalog: CatalogSnapshot,
    rng: Rng,
}

impl World {
    fn new(seed: u64, facts: u64) -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let options = StoreOptions {
            background_compaction: false,
            compaction_fan_in: 2,
            ..StoreOptions::default()
        };
        let mut rng = Rng(seed | 1);
        let mut fact_store =
            TableStore::open(directory.path().join("facts"), fact_schema(), options)
                .expect("facts");
        let mut dim_store =
            TableStore::open(directory.path().join("dims"), dim_schema(), options).expect("dims");
        let fact_model = (1..=facts)
            .map(|id| (id, random_fact(&mut rng, id)))
            .collect::<BTreeMap<_, _>>();
        let dim_model = (0..DIMS)
            .map(|id| {
                (
                    id,
                    Dim {
                        name: format!("dim-{id:03}"),
                        grp: id % 7,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        fact_store
            .bulk_ingest_snapshot(
                fact_model
                    .iter()
                    .map(|(id, fact)| fact_row(*id, fact, 1, false))
                    .collect(),
            )
            .expect("bulk facts");
        dim_store
            .bulk_ingest_snapshot(
                dim_model
                    .iter()
                    .map(|(id, dim)| dim_row(*id, dim, 1))
                    .collect(),
            )
            .expect("bulk dims");
        let key_versions = fact_model.keys().map(|id| (*id, 1)).collect();
        let flushed = fact_model.keys().copied().collect();
        let fact_entry = TableEntry::new(
            TableId::new(1),
            "facts",
            fact_schema(),
            TableStatistics::with_estimated_row_count(facts),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let dim_entry = TableEntry::new(
            TableId::new(2),
            "dims",
            dim_schema(),
            TableStatistics::with_estimated_row_count(u64::try_from(DIMS).expect("dims")),
        )
        .expect("entry")
        .with_key_columns([1])
        .expect("key");
        let catalog = CatalogSnapshot::new([DatabaseEntry::new(
            DatabaseId::new(1),
            "app",
            [fact_entry, dim_entry],
        )
        .expect("database")])
        .expect("catalog");
        Self {
            _directory: directory,
            facts: fact_store,
            dims: dim_store,
            fact_model,
            dim_model,
            key_versions,
            flushed,
            version: 1,
            next_id: facts + 1,
            catalog,
            rng,
        }
    }

    fn existing_key(&mut self) -> Option<u64> {
        if self.fact_model.is_empty() {
            return None;
        }
        let index = usize::try_from(self.rng.below(self.fact_model.len() as u64)).expect("index");
        self.fact_model.keys().nth(index).copied()
    }

    /// One replication batch: inserts past the end and into gaps, updates,
    /// deletes, on both tables, all newer than anything before.
    fn change_batch(&mut self) -> String {
        self.version += 1;
        let version = self.version;
        let mut rows = Vec::new();
        let inserts = self.rng.below(30);
        for _ in 0..inserts {
            let id = self.next_id;
            self.next_id += 1 + self.rng.below(3);
            let fact = random_fact(&mut self.rng, id);
            rows.push(fact_row(id, &fact, version, false));
            self.fact_model.insert(id, fact);
            self.key_versions.insert(id, version);
            self.flushed.remove(&id);
        }
        let gap_inserts = self.rng.below(5);
        for _ in 0..gap_inserts {
            let id = self.rng.below(self.next_id) + 1;
            if self.fact_model.contains_key(&id) {
                continue;
            }
            let fact = random_fact(&mut self.rng, id);
            rows.push(fact_row(id, &fact, version, false));
            self.fact_model.insert(id, fact);
            self.key_versions.insert(id, version);
            self.flushed.remove(&id);
        }
        let updates = self.rng.below(60);
        for _ in 0..updates {
            let Some(id) = self.existing_key() else { break };
            let mut fact = random_fact(&mut self.rng, id);
            fact.note = Some(format!("updated-{version}"));
            rows.push(fact_row(id, &fact, version, false));
            self.fact_model.insert(id, fact);
            self.key_versions.insert(id, version);
            self.flushed.remove(&id);
        }
        let deletes = self.rng.below(15);
        for _ in 0..deletes {
            let Some(id) = self.existing_key() else { break };
            let fact = self.fact_model.remove(&id).expect("existing");
            rows.push(fact_row(id, &fact, version, true));
            self.key_versions.insert(id, version);
            self.flushed.remove(&id);
        }
        let touched = rows.len();
        self.facts.ingest_cdc(rows).expect("fact cdc");
        let mut dim_rows = Vec::new();
        for _ in 0..self.rng.below(4) {
            let id =
                i64::try_from(self.rng.below(u64::try_from(DIMS).expect("dims"))).expect("dim");
            let dim = Dim {
                name: format!("dim-{id:03}-v{version}"),
                grp: i64::try_from(self.rng.below(7)).expect("grp"),
            };
            dim_rows.push(dim_row(id, &dim, version));
            self.dim_model.insert(id, dim);
        }
        let dims_touched = dim_rows.len();
        if !dim_rows.is_empty() {
            self.dims.ingest_cdc(dim_rows).expect("dim cdc");
        }
        format!("cdc batch v{version}: {touched} fact rows, {dims_touched} dim rows")
    }

    /// A replay of a strictly older version for a live key whose newest
    /// write is flushed: the store must keep the flushed version.
    fn stale_replay(&mut self) -> String {
        let candidates = self
            .flushed
            .iter()
            .copied()
            .filter(|id| self.fact_model.contains_key(id) && self.key_versions[id] > 1)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return "stale replay: no flushed key with an older version".to_owned();
        }
        let id =
            candidates[usize::try_from(self.rng.below(candidates.len() as u64)).expect("index")];
        let current = self.key_versions[&id];
        let stale_version = current - 1;
        let mut stale = self.fact_model[&id].clone();
        stale.amount = -9_999;
        stale.note = Some("stale".to_owned());
        self.facts
            .ingest_cdc(vec![fact_row(id, &stale, stale_version, false)])
            .expect("stale replay");
        format!("stale replay v{stale_version} for key {id} (flushed v{current})")
    }

    fn flush(&mut self) -> String {
        self.facts.flush().expect("flush facts");
        self.dims.flush().expect("flush dims");
        self.flushed.extend(self.key_versions.keys().copied());
        "flush".to_owned()
    }

    fn compact(&mut self) -> String {
        let outcome = self.facts.compact().expect("compact");
        format!("compact: {} input segments", outcome.input_segments())
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
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
        let mut execution =
            Execution::start(physical, &provider, 1 << 30, Collation::default()).expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution.next_batch().expect("batch") {
            for row in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(row).cloned().unwrap_or(Value::Null))
                        .collect::<Vec<_>>(),
                );
            }
        }
        rows
    }

    #[allow(clippy::too_many_lines)]
    fn check(&mut self, step: &str) {
        // Draw every random choice first; the model is borrowed below.
        let mut probe = Vec::new();
        for _ in 0..4 {
            probe.push(self.rng.below(self.next_id + 50) + 1);
        }
        if let Some(id) = self.existing_key() {
            probe.push(id);
        }
        probe.sort_unstable();
        probe.dedup();
        let range_start = self.rng.below(self.next_id) + 1;
        let filter_status = usize::try_from(self.rng.below(5)).expect("status");
        let filter_floor = i64::try_from(self.rng.below(600)).expect("floor") - 100;
        let join_grp = i64::try_from(self.rng.below(7)).expect("grp");
        let facts = &self.fact_model;
        let context = |query: &str| format!("after {step}: {query}");

        // Whole-table aggregate.
        let count = facts.len() as u64;
        let sum = facts.values().map(|fact| fact.amount).sum::<i64>();
        let sql = "SELECT COUNT(*), SUM(amount) FROM facts";
        assert_eq!(
            self.run(sql),
            vec![vec![Value::UInt64(count), Value::Int64(sum)]],
            "{}",
            context(sql)
        );

        // Grouping by a text column, ordered.
        let mut groups: BTreeMap<usize, (u64, i64)> = BTreeMap::new();
        for fact in facts.values() {
            let entry = groups.entry(fact.status).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += fact.amount;
        }
        let expected = groups
            .iter()
            .map(|(status, (count, sum))| {
                vec![
                    Value::Utf8(STATUSES[*status].to_owned()),
                    Value::UInt64(*count),
                    Value::Int64(*sum),
                ]
            })
            .collect::<Vec<_>>();
        let sql = "SELECT status, COUNT(*), SUM(amount) FROM facts GROUP BY status ORDER BY status";
        assert_eq!(self.run(sql), expected, "{}", context(sql));

        // Key lookups: a mix of live, deleted and absent keys.
        let expected = probe
            .iter()
            .filter_map(|id| {
                facts
                    .get(id)
                    .map(|fact| vec![Value::UInt64(*id), Value::Int64(fact.amount)])
            })
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT id, amount FROM facts WHERE id IN ({}) ORDER BY id",
            probe
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(self.run(&sql), expected, "{}", context(&sql));

        // A key range straddling whatever the memtable changed.
        let start = range_start;
        let end = start + 500;
        let expected = facts
            .range(start..=end)
            .map(|(id, _)| vec![Value::UInt64(*id)])
            .collect::<Vec<_>>();
        let sql = format!("SELECT id FROM facts WHERE id BETWEEN {start} AND {end} ORDER BY id");
        assert_eq!(self.run(&sql), expected, "{}", context(&sql));

        // A filter-first predicate on two columns.
        let (status, floor) = (filter_status, filter_floor);
        let expected = facts
            .values()
            .filter(|fact| fact.status == status && fact.amount > floor)
            .count() as u64;
        let sql = format!(
            "SELECT COUNT(*) FROM facts WHERE status = '{}' AND amount > {floor}",
            STATUSES[status]
        );
        assert_eq!(
            self.run(&sql),
            vec![vec![Value::UInt64(expected)]],
            "{}",
            context(&sql)
        );

        // Nullable column, ordered limit.
        let expected = facts
            .iter()
            .filter(|(_, fact)| fact.note.is_none())
            .take(20)
            .map(|(id, _)| vec![Value::UInt64(*id), Value::Null])
            .collect::<Vec<_>>();
        let sql = "SELECT id, note FROM facts WHERE note IS NULL ORDER BY id LIMIT 20";
        assert_eq!(self.run(sql), expected, "{}", context(sql));

        // Extremes.
        let sql = "SELECT MIN(id), MAX(id) FROM facts";
        let expected = match (facts.keys().next(), facts.keys().next_back()) {
            (Some(min), Some(max)) => vec![vec![Value::UInt64(*min), Value::UInt64(*max)]],
            _ => vec![vec![Value::Null, Value::Null]],
        };
        assert_eq!(self.run(sql), expected, "{}", context(sql));

        // A join whose probe side is small, grouped by the dimension name.
        let grp = join_grp;
        let mut joined: BTreeMap<String, (u64, i64)> = BTreeMap::new();
        for fact in facts.values() {
            if let Some(dim) = self.dim_model.get(&fact.dim_id)
                && dim.grp == grp
            {
                let entry = joined.entry(dim.name.clone()).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += fact.amount;
            }
        }
        let expected = joined
            .iter()
            .map(|(name, (count, sum))| {
                vec![
                    Value::Utf8(name.clone()),
                    Value::UInt64(*count),
                    Value::Int64(*sum),
                ]
            })
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT d.name, COUNT(*), SUM(f.amount) FROM facts f JOIN dims d ON f.dim_id = d.id \
             WHERE d.grp = {grp} GROUP BY d.name ORDER BY d.name"
        );
        assert_eq!(self.run(&sql), expected, "{}", context(&sql));

        // The scan itself streams in key order, whatever mix of segment and
        // memtable rows it serves, so first-seen group representatives are
        // the source's.
        let sql = "SELECT id FROM facts";
        let ids = self
            .run(sql)
            .into_iter()
            .map(|row| match row[0] {
                Value::UInt64(id) => id,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            facts.keys().copied().collect::<Vec<_>>(),
            "{}",
            context(sql)
        );
    }
}

fn run_sequence(seed: u64, facts: u64, steps: usize) {
    let mut world = World::new(seed, facts);
    world.check("the initial copy");
    for _ in 0..steps {
        let step = match world.rng.below(10) {
            0 | 1 => world.flush(),
            2 => world.compact(),
            3 => world.stale_replay(),
            _ => world.change_batch(),
        };
        world.check(&step);
    }
}

#[test]
fn queries_stay_exact_while_replication_is_live() {
    // The settled aggregate memo stays on, as in production: it may answer
    // only while the memtable is empty and keys on the manifest generation,
    // so a stale replay after a change would be a defect this test catches.
    // Below the streaming threshold: memtable overlap takes the
    // materialized path. Above it: the streaming path and the overlay.
    for (seed, facts, steps) in [(11, 20_000, 24), (23, 90_000, 24), (37, 90_000, 24)] {
        run_sequence(seed, facts, steps);
    }
}
