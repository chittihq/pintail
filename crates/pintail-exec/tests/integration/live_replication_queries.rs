//! Queries while replication is live: the primary state of a mirrored
//! table is a segment set with the latest changes still in the memtable,
//! flushes and compactions landing between queries. A generated sequence
//! of change batches, stale replays, insert-only batches, flushes and
//! compactions runs over a fact and a dimension table while a fixed set of
//! query shapes is checked after every step against an in-memory model:
//! aggregates, a collated GROUP BY whose first-seen representative must be
//! the source's, key lookups mixing live, deleted and absent keys, a key
//! range, filter-first predicates, an ordered limit and a pushed-down
//! limit, extremes, joins on a non-key and on the storage key, full-row
//! comparisons, the scan's own key order, and a reader pinned across
//! mutations. Every scenario the generator claims to cover is counted and
//! required. Three fixtures: below the streaming threshold, above it, and
//! across several direct slices with changes at the slice boundaries.
use std::collections::{BTreeMap, BTreeSet};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

/// Five statuses, each in three spellings a case-insensitive collation
/// folds together: which spelling a group shows depends on scan order.
const STATUSES: [[&str; 3]; 5] = [
    ["done", "Done", "DONE"],
    ["held", "Held", "HELD"],
    ["new", "New", "NEW"],
    ["open", "Open", "OPEN"],
    ["void", "Void", "VOID"],
];
const DIMS: i64 = 200;
/// Rows per direct slice (`DIRECT_SLICE_ROWS` in the store).
const SLICE_ROWS: u64 = 131_072;

fn fact_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "dim_id", DataType::Int64, false),
            Column::new(3, "status", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_general_ci".to_owned())),
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
    spelling: usize,
    amount: i64,
    note: Option<String>,
}

impl Fact {
    fn status_text(&self) -> &'static str {
        STATUSES[self.status][self.spelling]
    }
    fn values(&self, id: u64) -> Vec<Value> {
        vec![
            Value::UInt64(id),
            Value::Int64(self.dim_id),
            Value::Utf8(self.status_text().to_owned()),
            Value::Int64(self.amount),
            self.note.clone().map_or(Value::Null, Value::Utf8),
        ]
    }
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
    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        if items.is_empty() {
            None
        } else {
            Some(items[usize::try_from(self.below(items.len() as u64)).expect("index")])
        }
    }
}

fn fact_row(id: u64, fact: &Fact, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
        fact.values(id),
        version,
        deleted,
    )
}

fn dim_row(id: i64, dim: &Dim, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Int64(id)]).expect("key"),
        vec![
            Value::Int64(id),
            Value::Utf8(dim.name.clone()),
            Value::Int64(dim.grp),
        ],
        version,
        deleted,
    )
}

fn random_fact(rng: &mut Rng, id: u64) -> Fact {
    Fact {
        dim_id: i64::try_from(rng.below(u64::try_from(DIMS).expect("dims"))).expect("dim"),
        status: usize::try_from(rng.below(5)).expect("status"),
        spelling: usize::try_from(rng.below(3)).expect("spelling"),
        amount: i64::try_from(rng.below(1_000)).expect("amount") - 200,
        note: id.is_multiple_of(3).then(|| format!("note-{id}")),
    }
}

#[derive(Default, Debug)]
struct Hits {
    gap_inserts: usize,
    deleted_probes: usize,
    absent_probes: usize,
    insert_only_batches: usize,
    stale_replays: usize,
    compactions: usize,
    pinned_readers: usize,
    boundary_batches: usize,
    representative_changes: usize,
}

struct World {
    _directory: tempfile::TempDir,
    label: String,
    facts: TableStore,
    dims: TableStore,
    fact_model: BTreeMap<u64, Fact>,
    dim_model: BTreeMap<i64, Dim>,
    /// Keys never written, inside the key space: gap inserts draw here.
    holes: BTreeSet<u64>,
    /// Keys deleted and not reinserted.
    deleted: BTreeSet<u64>,
    /// The version of each key's newest write and whether it is flushed.
    key_versions: BTreeMap<u64, u64>,
    flushed: BTreeSet<u64>,
    version: u64,
    next_id: u64,
    catalog: CatalogSnapshot,
    rng: Rng,
    history: Vec<String>,
    hits: Hits,
}

impl World {
    fn new(label: &str, seed: u64, facts: u64) -> Self {
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
        // Every 97th key is left out of the initial copy: a hole for a gap
        // insert to fill later.
        let mut holes = BTreeSet::new();
        let mut fact_model = BTreeMap::new();
        for id in 1..=facts {
            if id.is_multiple_of(97) {
                holes.insert(id);
            } else {
                fact_model.insert(id, random_fact(&mut rng, id));
            }
        }
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
                    .map(|(id, dim)| dim_row(*id, dim, 1, false))
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
            label: format!("{label} (seed {seed}, {facts} rows)"),
            facts: fact_store,
            dims: dim_store,
            fact_model,
            dim_model,
            holes,
            deleted: BTreeSet::new(),
            key_versions,
            flushed,
            version: 1,
            next_id: facts + 1,
            catalog,
            rng,
            history: Vec::new(),
            hits: Hits::default(),
        }
    }

    fn existing_key(&mut self) -> Option<u64> {
        if self.fact_model.is_empty() {
            return None;
        }
        let index = usize::try_from(self.rng.below(self.fact_model.len() as u64)).expect("index");
        self.fact_model.keys().nth(index).copied()
    }

    fn write(&mut self, rows: &mut Vec<StoredRow>, id: u64, fact: Fact, deleted: bool) {
        let version = self.version;
        rows.push(fact_row(id, &fact, version, deleted));
        if deleted {
            self.fact_model.remove(&id);
            self.deleted.insert(id);
        } else {
            self.fact_model.insert(id, fact);
            self.deleted.remove(&id);
            self.holes.remove(&id);
        }
        self.key_versions.insert(id, version);
        self.flushed.remove(&id);
    }

    /// One replication batch: inserts past the end and into holes, updates
    /// (some to a different spelling of the same status, which moves a
    /// group's first-seen representative), deletes, and dimension changes.
    fn change_batch(&mut self) -> String {
        self.version += 1;
        let version = self.version;
        let mut rows = Vec::new();
        for _ in 0..self.rng.below(30) {
            let id = self.next_id;
            self.next_id += 1 + self.rng.below(3);
            let fact = random_fact(&mut self.rng, id);
            self.write(&mut rows, id, fact, false);
        }
        let holes = self.holes.iter().copied().collect::<Vec<_>>();
        for _ in 0..=self.rng.below(3) {
            if let Some(id) = self.rng.pick(&holes) {
                let fact = random_fact(&mut self.rng, id);
                self.write(&mut rows, id, fact, false);
                self.hits.gap_inserts += 1;
            }
        }
        for _ in 0..self.rng.below(60) {
            let Some(id) = self.existing_key() else { break };
            let mut fact = random_fact(&mut self.rng, id);
            fact.note = Some(format!("updated-{version}"));
            if self.fact_model[&id].status != fact.status
                || self.fact_model[&id].spelling != fact.spelling
            {
                self.hits.representative_changes += 1;
            }
            self.write(&mut rows, id, fact, false);
        }
        for _ in 0..self.rng.below(15) {
            let Some(id) = self.existing_key() else { break };
            let fact = self.fact_model[&id].clone();
            self.write(&mut rows, id, fact, true);
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
            dim_rows.push(dim_row(id, &dim, version, false));
            self.dim_model.insert(id, dim);
        }
        let dims_touched = dim_rows.len();
        if !dim_rows.is_empty() {
            self.dims.ingest_cdc(dim_rows).expect("dim cdc");
        }
        format!("cdc v{version}: {touched} fact rows, {dims_touched} dim rows")
    }

    /// Only dimensions change: the join must notice without a fact write.
    fn dimension_only_batch(&mut self) -> String {
        self.version += 1;
        let version = self.version;
        let id = i64::try_from(self.rng.below(u64::try_from(DIMS).expect("dims"))).expect("dim");
        let delete = self.rng.below(4) == 0 && self.dim_model.contains_key(&id);
        let dim = Dim {
            name: format!("dim-{id:03}-v{version}"),
            grp: i64::try_from(self.rng.below(7)).expect("grp"),
        };
        self.dims
            .ingest_cdc(vec![dim_row(id, &dim, version, delete)])
            .expect("dim cdc");
        if delete {
            self.dim_model.remove(&id);
        } else {
            self.dim_model.insert(id, dim);
        }
        format!(
            "dimension-only v{version}: {} {id}",
            if delete { "delete" } else { "upsert" }
        )
    }

    /// A flush, a grouped aggregate (which the settled memo may keep), then
    /// inserts only, all above the key space: the executor's insert-only
    /// delta path, whichever way it answers, must agree with the model.
    fn insert_only_batch(&mut self) -> String {
        self.flush();
        self.check_grouping("before an insert-only batch");
        self.version += 1;
        let mut rows = Vec::new();
        for _ in 0..(5 + self.rng.below(40)) {
            let id = self.next_id;
            self.next_id += 1;
            let fact = random_fact(&mut self.rng, id);
            self.write(&mut rows, id, fact, false);
        }
        let count = rows.len();
        self.facts.ingest_cdc(rows).expect("insert-only cdc");
        self.hits.insert_only_batches += 1;
        format!("insert-only v{}: {count} rows past the end", self.version)
    }

    /// Changes hugging the direct slice boundaries of a segment: the last
    /// and first rows of adjacent slices updated, deleted or left alone.
    fn boundary_batch(&mut self) -> String {
        self.version += 1;
        let mut rows = Vec::new();
        let mut boundary = SLICE_ROWS;
        while boundary < self.next_id {
            // Keys are dense from 1, so row r holds key r + 1 up to the
            // holes; take the keys around the boundary that exist.
            for key in boundary.saturating_sub(2)..=boundary + 2 {
                if !self.fact_model.contains_key(&key) {
                    continue;
                }
                let delete = self.rng.below(3) == 0;
                let mut fact = random_fact(&mut self.rng, key);
                fact.note = Some("boundary".to_owned());
                self.write(&mut rows, key, fact, delete);
            }
            boundary += SLICE_ROWS;
        }
        let count = rows.len();
        self.facts.ingest_cdc(rows).expect("boundary cdc");
        self.hits.boundary_batches += 1;
        format!("boundary v{}: {count} rows at slice edges", self.version)
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
        let Some(id) = self.rng.pick(&candidates) else {
            return "stale replay: no flushed key with an older version".to_owned();
        };
        let current = self.key_versions[&id];
        let stale_version = current - 1;
        let mut stale = self.fact_model[&id].clone();
        stale.amount = -9_999;
        stale.note = Some("stale".to_owned());
        self.facts
            .ingest_cdc(vec![fact_row(id, &stale, stale_version, false)])
            .expect("stale replay");
        self.hits.stale_replays += 1;
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
        if outcome.input_segments() > 0 {
            self.hits.compactions += 1;
        }
        format!("compact: {} input segments", outcome.input_segments())
    }

    fn start(
        &self,
        sql: &str,
    ) -> (
        Execution,
        pintail_store::TableSnapshot,
        pintail_store::TableSnapshot,
    ) {
        let facts = self.facts.snapshot();
        let dims = self.dims.snapshot();
        let execution = {
            let provider = SnapshotScanProvider::new([
                (DatabaseId::new(1), TableId::new(1), &facts),
                (DatabaseId::new(1), TableId::new(2), &dims),
            ])
            .expect("provider");
            let bound = Binder::new(&self.catalog, Some("app"))
                .bind(&parse_statement(sql).expect("parse"))
                .expect("bind");
            let collation = Collation::from_mysql_name(bound.text_collation).unwrap_or_default();
            let physical =
                PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)), collation)
                    .expect("plan");
            // The provider borrows the snapshots; the execution outlives
            // this block only through the snapshots returned with it.
            Execution::start(physical, &provider, 1 << 30, collation).expect("start")
        };
        (execution, facts, dims)
    }

    fn run(&self, sql: &str) -> Vec<Vec<Value>> {
        let (mut execution, _facts, _dims) = self.start(sql);
        drain(&mut execution)
    }

    fn context(&self, query: &str) -> String {
        format!(
            "{} after {} steps; last: {} :: {query}",
            self.label,
            self.history.len(),
            self.history
                .iter()
                .rev()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(" <- ")
        )
    }

    fn check_grouping(&self, when: &str) {
        // Groups fold spellings; each shows the spelling of its lowest key,
        // the row the source meets first.
        let mut groups: BTreeMap<usize, (&'static str, u64, i64)> = BTreeMap::new();
        for fact in self.fact_model.values() {
            let entry = groups
                .entry(fact.status)
                .or_insert((fact.status_text(), 0, 0));
            entry.1 += 1;
            entry.2 += fact.amount;
        }
        let expected = groups
            .values()
            .map(|(spelling, count, sum)| {
                vec![
                    Value::Utf8((*spelling).to_owned()),
                    Value::UInt64(*count),
                    Value::Int64(*sum),
                ]
            })
            .collect::<Vec<_>>();
        let sql = "SELECT status, COUNT(*), SUM(amount) FROM facts GROUP BY status ORDER BY status";
        assert_eq!(self.run(sql), expected, "{when}: {}", self.context(sql));
    }

    /// A reader that started before a change keeps answering from what it
    /// saw; a reader started after sees the change.
    fn pinned_reader_check(&mut self) {
        let sql = "SELECT id, amount FROM facts";
        let before = self
            .fact_model
            .iter()
            .map(|(id, fact)| vec![Value::UInt64(*id), Value::Int64(fact.amount)])
            .collect::<Vec<_>>();
        let (mut execution, _facts, _dims) = self.start(sql);
        let mut rows = Vec::new();
        if let Some(batch) = execution.next_batch().expect("first batch") {
            rows.extend(rows_of(&batch));
        }
        let step = self.change_batch();
        self.history.push(format!("{step} (under a pinned reader)"));
        self.flush();
        self.compact();
        rows.extend(drain(&mut execution));
        assert_eq!(rows, before, "pinned reader: {}", self.context(sql));
        let after = self
            .fact_model
            .iter()
            .map(|(id, fact)| vec![Value::UInt64(*id), Value::Int64(fact.amount)])
            .collect::<Vec<_>>();
        assert_eq!(
            self.run(sql),
            after,
            "reader after the change: {}",
            self.context(sql)
        );
        self.hits.pinned_readers += 1;
    }

    #[allow(clippy::too_many_lines)]
    fn check(&mut self) {
        // Draw every random choice first; the model is borrowed below.
        let deleted = self.deleted.iter().copied().collect::<Vec<_>>();
        let mut probe = Vec::new();
        for _ in 0..2 {
            if let Some(id) = self.existing_key() {
                probe.push(id);
            }
        }
        if let Some(id) = self.rng.pick(&deleted) {
            probe.push(id);
            self.hits.deleted_probes += 1;
        }
        // A key past everything ever written, and a hole never filled.
        probe.push(self.next_id + 1_000 + self.rng.below(1_000));
        self.hits.absent_probes += 1;
        let holes = self.holes.iter().copied().collect::<Vec<_>>();
        if let Some(id) = self.rng.pick(&holes) {
            probe.push(id);
            self.hits.absent_probes += 1;
        }
        probe.sort_unstable();
        probe.dedup();
        let range_start = self.rng.below(self.next_id) + 1;
        let filter_status = usize::try_from(self.rng.below(5)).expect("status");
        let filter_spelling = usize::try_from(self.rng.below(3)).expect("spelling");
        let filter_floor = i64::try_from(self.rng.below(600)).expect("floor") - 100;
        let join_grp = i64::try_from(self.rng.below(7)).expect("grp");
        let offset =
            usize::try_from(self.rng.below(self.fact_model.len().max(1) as u64)).expect("offset");
        let facts = &self.fact_model;

        // Whole-table aggregate; SUM over nothing is NULL.
        let count = facts.len() as u64;
        let sum = if facts.is_empty() {
            Value::Null
        } else {
            Value::Int64(facts.values().map(|fact| fact.amount).sum::<i64>())
        };
        let sql = "SELECT COUNT(*), SUM(amount) FROM facts";
        assert_eq!(
            self.run(sql),
            vec![vec![Value::UInt64(count), sum]],
            "{}",
            self.context(sql)
        );

        self.check_grouping("step");

        // Full rows for live, deleted and absent keys.
        let expected = probe
            .iter()
            .filter_map(|id| facts.get(id).map(|fact| fact.values(*id)))
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT id, dim_id, status, amount, note FROM facts WHERE id IN ({}) ORDER BY id",
            probe
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(self.run(&sql), expected, "{}", self.context(&sql));

        // A key range straddling whatever the memtable changed.
        let (start, end) = (range_start, range_start + 500);
        let expected = facts
            .range(start..=end)
            .map(|(id, fact)| vec![Value::UInt64(*id), Value::Int64(fact.amount)])
            .collect::<Vec<_>>();
        let sql =
            format!("SELECT id, amount FROM facts WHERE id BETWEEN {start} AND {end} ORDER BY id");
        assert_eq!(self.run(&sql), expected, "{}", self.context(&sql));

        // Filter-first predicates on two columns, a third column projected
        // so the predicate columns do not exhaust the projection, and the
        // collation folding the spelling.
        let expected = facts
            .values()
            .filter(|fact| {
                fact.status == filter_status && fact.amount > filter_floor && fact.note.is_some()
            })
            .count() as u64;
        let sql = format!(
            "SELECT COUNT(note) FROM facts WHERE status = '{}' AND amount > {filter_floor}",
            STATUSES[filter_status][filter_spelling]
        );
        assert_eq!(
            self.run(&sql),
            vec![vec![Value::UInt64(expected)]],
            "{}",
            self.context(&sql)
        );

        // Nullable column under an ordered limit.
        let expected = facts
            .iter()
            .filter(|(_, fact)| fact.note.is_none())
            .take(20)
            .map(|(id, _)| vec![Value::UInt64(*id), Value::Null])
            .collect::<Vec<_>>();
        let sql = "SELECT id, note FROM facts WHERE note IS NULL ORDER BY id LIMIT 20";
        assert_eq!(self.run(sql), expected, "{}", self.context(sql));

        // A limit pushed into the scan, in scan (key) order.
        let expected = facts
            .iter()
            .skip(offset)
            .take(25)
            .map(|(id, fact)| vec![Value::UInt64(*id), Value::Int64(fact.amount)])
            .collect::<Vec<_>>();
        let sql = format!("SELECT id, amount FROM facts LIMIT 25 OFFSET {offset}");
        assert_eq!(self.run(&sql), expected, "{}", self.context(&sql));

        // Extremes.
        let sql = "SELECT MIN(id), MAX(id) FROM facts";
        let expected = match (facts.keys().next(), facts.keys().next_back()) {
            (Some(min), Some(max)) => vec![vec![Value::UInt64(*min), Value::UInt64(*max)]],
            _ => vec![vec![Value::Null, Value::Null]],
        };
        assert_eq!(self.run(sql), expected, "{}", self.context(sql));

        // A join on a non-key column, grouped by the dimension name.
        let mut joined: BTreeMap<String, (u64, i64)> = BTreeMap::new();
        for fact in facts.values() {
            if let Some(dim) = self.dim_model.get(&fact.dim_id)
                && dim.grp == join_grp
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
             WHERE d.grp = {join_grp} GROUP BY d.name ORDER BY d.name"
        );
        assert_eq!(self.run(&sql), expected, "{}", self.context(&sql));

        // A join on the fact table's storage key: the build side's key span
        // restricts the probe scan.
        let mut matched = 0_u64;
        let mut matched_sum = 0_i64;
        for (id, dim) in &self.dim_model {
            if dim.grp == join_grp
                && let Ok(key) = u64::try_from(*id)
                && let Some(fact) = facts.get(&key)
            {
                matched += 1;
                matched_sum += fact.amount;
            }
        }
        let expected = vec![vec![
            Value::UInt64(matched),
            if matched == 0 {
                Value::Null
            } else {
                Value::Int64(matched_sum)
            },
        ]];
        let sql = format!(
            "SELECT COUNT(*), SUM(f.amount) FROM dims d JOIN facts f ON f.id = d.id \
             WHERE d.grp = {join_grp}"
        );
        assert_eq!(self.run(&sql), expected, "{}", self.context(&sql));

        // The scan streams in key order, whatever mix of segment and
        // memtable rows it serves.
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
            self.context(sql)
        );
    }

    /// Every row, every column.
    fn check_all_rows(&self) {
        let expected = self
            .fact_model
            .iter()
            .map(|(id, fact)| fact.values(*id))
            .collect::<Vec<_>>();
        let sql = "SELECT id, dim_id, status, amount, note FROM facts";
        assert_eq!(self.run(sql), expected, "{}", self.context(sql));
    }
}

fn rows_of(batch: &pintail_exec::RecordBatch) -> Vec<Vec<Value>> {
    batch
        .selection()
        .selected_rows()
        .map(|row| {
            batch
                .columns()
                .iter()
                .map(|column| column.value(row).expect("a value for every row").clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn drain(execution: &mut Execution) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    while let Some(batch) = execution.next_batch().expect("batch") {
        rows.extend(rows_of(&batch));
    }
    rows
}

fn run_sequence(label: &str, seed: u64, facts: u64, steps: usize, boundaries: bool) -> Hits {
    let mut world = World::new(label, seed, facts);
    world.history.push("initial copy".to_owned());
    world.check();
    world.check_all_rows();
    for step in 0..steps {
        let description = match world.rng.below(12) {
            0 | 1 => world.flush(),
            2 => world.compact(),
            3 => world.stale_replay(),
            4 => world.insert_only_batch(),
            5 => world.dimension_only_batch(),
            6 if boundaries => world.boundary_batch(),
            _ => world.change_batch(),
        };
        world.history.push(description);
        world.check();
        if step % 5 == 4 {
            world.check_all_rows();
            world.pinned_reader_check();
        }
    }
    // The last step exercises the boundaries of a sliced segment directly.
    if boundaries {
        let description = world.boundary_batch();
        world.history.push(description);
        world.check();
        world.check_all_rows();
    }
    world.hits
}

#[test]
fn queries_stay_exact_while_replication_is_live() {
    // The settled aggregate memo stays on, as in production: it answers
    // only while the memtable is empty and keys on the manifest
    // generation, so a stale replay through it would be caught here.
    let mut total = Hits::default();
    for (label, seed, facts, steps, boundaries) in [
        ("materialized path", 11, 8_000, 22, false),
        ("streaming path", 23, 70_000, 22, false),
        ("several slices", 37, 300_000, 6, true),
    ] {
        let hits = run_sequence(label, seed, facts, steps, boundaries);
        eprintln!("{label}: {hits:?}");
        assert!(hits.gap_inserts > 0, "{label}: no gap insert happened");
        assert!(
            hits.deleted_probes > 0,
            "{label}: no deleted key was probed"
        );
        assert!(hits.absent_probes > 0, "{label}: no absent key was probed");
        assert!(
            hits.pinned_readers > 0,
            "{label}: no reader was pinned across a change"
        );
        assert!(
            hits.representative_changes > 0,
            "{label}: no group representative moved"
        );
        total.insert_only_batches += hits.insert_only_batches;
        total.stale_replays += hits.stale_replays;
        total.compactions += hits.compactions;
        total.boundary_batches += hits.boundary_batches;
    }
    assert!(
        total.insert_only_batches > 0,
        "no insert-only batch happened"
    );
    assert!(total.stale_replays > 0, "no stale replay happened");
    assert!(total.compactions > 0, "no compaction merged anything");
    assert!(
        total.boundary_batches > 0,
        "no slice-boundary batch happened"
    );
}
