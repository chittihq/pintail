//! Lookups by a text key must not read the table, and must answer exactly
//! what reading it answers.
//!
//! Two invented tables keyed by identifier strings: `hubs` and the `parcels`
//! that reference them by three text columns, one per collation family (a
//! case- and accent-insensitive NO PAD one, a PAD SPACE one, a binary one).
//! Checked with the side index on and off against a row model judged by the
//! engine's own comparator:
//!
//! - equality and IN on the text primary key, in another spelling;
//! - a scan that projects nothing but its predicate column;
//! - joins in both directions whose small side names the text keys the large
//!   side can match, inner and left;
//! - a literal on one side of a join equality;
//!
//! from a fresh snapshot, with changes in the memtable, after a flush and
//! after compaction. The values-decoded counter proves the large table was
//! not read whole.
//!
//! The measurement is `#[ignore]`d and runs with the memo off:
//! `PINTAIL_DISABLE_SETTLED_MEMO=1 cargo test --profile recovery -p pintail-exec
//! --test integration text_key_lookup:: -- --ignored --nocapture`.

use std::collections::BTreeMap;

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    compare_collated_text,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::{StoreOptions, TableStore, override_side_index};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

const DATABASE_ID: DatabaseId = DatabaseId::new(1);
const HUBS_ID: TableId = TableId::new(1);
const PARCELS_ID: TableId = TableId::new(2);
const HUBS: u64 = 2_000;
const PARCELS: u64 = 150_000;
const REGIONS: u64 = 200;

fn mix(value: u64) -> u64 {
    let mut hash = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// An identifier string shaped like the ones applications generate.
fn reference(kind: &str, id: u64) -> String {
    let hash = mix(id ^ (u64::from(kind.as_bytes()[0]) << 56));
    format!(
        "{:08x}-{:04x}-{:04x}-{kind}{id:07}",
        hash >> 32,
        (hash >> 16) & 0xffff,
        hash & 0xffff
    )
}

fn hubs_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "hub_ref", DataType::Utf8, false),
            Column::new(2, "n", DataType::UInt64, false),
            Column::new(3, "region", DataType::UInt64, false),
            Column::new(4, "code", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_general_ci".into())),
            Column::new(5, "zone", DataType::Utf8, false)
                .with_collation(Some("utf8mb4_bin".into())),
        ],
    )
    .expect("schema")
}

fn parcels_schema() -> TableSchema {
    TableSchema::new(
        1,
        vec![
            Column::new(1, "parcel_ref", DataType::Utf8, false),
            Column::new(2, "seq", DataType::UInt64, false),
            Column::new(3, "hub_ref", DataType::Utf8, true),
            Column::new(4, "carrier", DataType::Utf8, true)
                .with_collation(Some("utf8mb4_general_ci".into())),
            Column::new(5, "zone", DataType::Utf8, true).with_collation(Some("utf8mb4_bin".into())),
            Column::new(6, "weight", DataType::Int64, false),
        ],
    )
    .expect("schema")
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_field_names)]
struct Hub {
    hub_ref: String,
    region: u64,
    code: String,
    zone: String,
}

fn hub(n: u64) -> Hub {
    Hub {
        hub_ref: reference("h", n),
        region: n % REGIONS,
        code: format!("code{n}"),
        zone: format!("zone{n}"),
    }
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_field_names)]
struct Parcel {
    parcel_ref: String,
    hub_ref: Option<String>,
    carrier: Option<String>,
    zone: Option<String>,
    weight: i64,
}

/// A parcel pointing at hub `target`, in a spelling only some collations
/// fold onto the hub's own.
fn parcel_at(seq: u64, target: u64) -> Parcel {
    let hub = hub(target);
    let variant = seq % 4;
    Parcel {
        parcel_ref: reference("p", seq),
        hub_ref: (!seq.is_multiple_of(23)).then(|| match variant {
            0 => hub.hub_ref.clone(),
            1 => hub.hub_ref.to_uppercase(),
            2 => hub.hub_ref.replacen('h', "H", 1),
            // NO PAD: a trailing space is a different key.
            _ => format!("{} ", hub.hub_ref),
        }),
        carrier: (!seq.is_multiple_of(19)).then(|| match variant {
            0 => hub.code.clone(),
            1 => hub.code.to_uppercase(),
            // PAD SPACE: trailing spaces are ignored.
            2 => format!("{}  ", hub.code),
            _ => hub.code.replacen('o', "\u{d3}", 1),
        }),
        zone: (!seq.is_multiple_of(17)).then(|| match variant {
            0 | 1 => hub.zone.clone(),
            2 => hub.zone.to_uppercase(),
            _ => format!("{} ", hub.zone),
        }),
        weight: i64::try_from(seq % 97).expect("small"),
    }
}

fn initial(seq: u64) -> Parcel {
    parcel_at(seq, mix(seq) % HUBS)
}

fn text(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |text| Value::Utf8(text.clone()))
}

fn parcel_row(seq: u64, parcel: &Parcel, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::Utf8(parcel.parcel_ref.clone())]).expect("key"),
        vec![
            Value::Utf8(parcel.parcel_ref.clone()),
            Value::UInt64(seq),
            text(parcel.hub_ref.as_ref()),
            text(parcel.carrier.as_ref()),
            text(parcel.zone.as_ref()),
            Value::Int64(parcel.weight),
        ],
        version,
        deleted,
    )
}

struct Fixture {
    _dirs: [tempfile::TempDir; 2],
    hubs: TableStore,
    parcels: TableStore,
    catalog: CatalogSnapshot,
    model: BTreeMap<u64, Parcel>,
    version: u64,
}

type Rows = Vec<Vec<Value>>;

impl Fixture {
    fn new() -> Self {
        let options = || StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        };
        let dirs = [
            tempfile::tempdir().expect("dir"),
            tempfile::tempdir().expect("dir"),
        ];
        let mut hubs = TableStore::open(dirs[0].path(), hubs_schema(), options()).expect("open");
        hubs.bulk_ingest_snapshot(
            (0..HUBS)
                .map(|n| {
                    let hub = hub(n);
                    StoredRow::new(
                        PrimaryKey::new(vec![KeyPart::Utf8(hub.hub_ref.clone())]).expect("key"),
                        vec![
                            Value::Utf8(hub.hub_ref),
                            Value::UInt64(n),
                            Value::UInt64(hub.region),
                            Value::Utf8(hub.code),
                            Value::Utf8(hub.zone),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .expect("ingest");
        let mut parcels =
            TableStore::open(dirs[1].path(), parcels_schema(), options()).expect("open");
        let model = (0..PARCELS)
            .map(|seq| (seq, initial(seq)))
            .collect::<BTreeMap<_, _>>();
        parcels
            .bulk_ingest_snapshot(
                model
                    .iter()
                    .map(|(seq, parcel)| parcel_row(*seq, parcel, 1, false))
                    .collect(),
            )
            .expect("ingest");
        let entry = |id, name: &str, schema, rows| {
            TableEntry::new(id, name, schema, TableStatistics::with_row_count(rows))
                .expect("entry")
                .with_key_columns([1])
                .expect("key")
        };
        let database = DatabaseEntry::new(
            DATABASE_ID,
            "app",
            [
                entry(HUBS_ID, "hubs", hubs_schema(), HUBS),
                entry(PARCELS_ID, "parcels", parcels_schema(), PARCELS),
            ],
        )
        .expect("database");
        Self {
            _dirs: dirs,
            hubs,
            parcels,
            catalog: CatalogSnapshot::new([database]).expect("catalog"),
            model,
            version: 2,
        }
    }

    /// Moves parcels onto and off the probed hubs, deletes some and appends
    /// new ones.
    fn change(&mut self, round: u64) {
        let mut changes = Vec::new();
        let seqs = self.model.keys().copied().collect::<Vec<_>>();
        for seq in seqs {
            if seq % 101 != round % 101 {
                continue;
            }
            self.version += 1;
            if seq % 5 == 0 {
                let old = self.model.remove(&seq).expect("row");
                changes.push(parcel_row(seq, &old, self.version, true));
                continue;
            }
            // Region 7's hubs are 7, 207, 407, ...: move onto one of them,
            // or onto its neighbour in another region.
            let target = 7 + REGIONS * (seq % 4) + u64::from(seq % 3 == 0);
            let moved = parcel_at(seq, target);
            changes.push(parcel_row(seq, &moved, self.version, false));
            self.model.insert(seq, moved);
        }
        for seq in PARCELS + round * 1_000..PARCELS + round * 1_000 + 400 {
            self.version += 1;
            let parcel = parcel_at(seq, 7 + REGIONS * (seq % 9));
            changes.push(parcel_row(seq, &parcel, self.version, false));
            self.model.insert(seq, parcel);
        }
        for batch in changes.chunks(2_000) {
            self.parcels
                .ingest_cdc(batch.to_vec())
                .expect("change batch");
        }
    }

    /// The statement's rows and how many `parcels` values its scans decoded.
    fn run(&self, sql: &str, index: bool) -> (Rows, u64) {
        override_side_index(Some(index));
        let hubs = self.hubs.snapshot();
        let parcels = self.parcels.snapshot();
        let provider = SnapshotScanProvider::new([
            (DATABASE_ID, HUBS_ID, &hubs),
            (DATABASE_ID, PARCELS_ID, &parcels),
        ])
        .expect("provider");
        let statement = parse_statement(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let bound = Binder::new(&self.catalog, Some("app"))
            .bind(&statement)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
        let physical = PhysicalPlanner::plan(logical, Collation::default()).expect("plan");
        let mut execution =
            Execution::start(physical, &provider, 512 * 1024 * 1024, Collation::default())
                .expect("start");
        let mut rows = Vec::new();
        while let Some(batch) = execution
            .next_batch()
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
        {
            for index in batch.selection().selected_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|column| column.value(index).cloned().expect("value"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        drop(execution);
        override_side_index(None);
        let decoded = provider
            .scan_stats(DATABASE_ID, PARCELS_ID)
            .map_or(0, |stats| stats.values_decoded);
        (rows, decoded)
    }

    /// Runs `sql` with the index off and on, checks both against `expected`
    /// (compared as sorted rows) and answers the values decoded with it on.
    fn both(&self, stage: &str, sql: &str, mut expected: Rows) -> u64 {
        expected.sort_by_key(|row| format!("{row:?}"));
        let mut decoded = 0;
        for index in [false, true] {
            let (mut rows, values) = self.run(sql, index);
            rows.sort_by_key(|row| format!("{row:?}"));
            assert_eq!(rows, expected, "{stage}, index {index}: {sql}");
            decoded = values;
        }
        decoded
    }

    /// `(seq, hub n)` for every parcel whose `pick`ed column equals, under
    /// `collation`, the same column of a hub in region 7.
    fn joined(
        &self,
        pick: fn(&Parcel) -> Option<&String>,
        of_hub: fn(&Hub) -> &String,
        collation: Collation,
    ) -> Vec<(u64, u64)> {
        let hubs = (0..HUBS)
            .filter(|n| n % REGIONS == 7)
            .map(|n| (n, hub(n)))
            .collect::<Vec<_>>();
        let mut pairs = Vec::new();
        for (seq, parcel) in &self.model {
            let Some(value) = pick(parcel) else {
                continue;
            };
            for (n, hub) in &hubs {
                if compare_collated_text(value, of_hub(hub), collation) == std::cmp::Ordering::Equal
                {
                    pairs.push((*seq, *n));
                }
            }
        }
        pairs
    }

    /// `ROW_NUMBER()` over partitions of a text column must number the rows
    /// of every spelling the collation folds together as one partition, in
    /// the window's order: `canonical` gives the partition a value belongs
    /// to under the plan's collation (case and accents fold, a trailing
    /// space does not).
    fn check_window(
        &self,
        stage: &str,
        column: &str,
        pick: fn(&Parcel) -> Option<&String>,
        canonical: fn(&str) -> String,
    ) {
        let mut partitions: BTreeMap<Option<String>, Vec<(i64, u64)>> = BTreeMap::new();
        for (seq, parcel) in self.model.iter().filter(|(_, parcel)| parcel.weight < 40) {
            partitions
                .entry(pick(parcel).map(|value| canonical(value)))
                .or_default()
                .push((parcel.weight, *seq));
        }
        let mut expected = Vec::new();
        for rows in partitions.values_mut() {
            rows.sort_by(|left, right| right.cmp(left));
            for (position, (_, seq)) in rows.iter().enumerate() {
                expected.push((*seq, position as u64 + 1));
            }
        }
        expected.sort_unstable();
        let sql = format!(
            "SELECT seq, ROW_NUMBER() OVER (PARTITION BY {column} ORDER BY weight DESC, seq DESC) \
             FROM parcels WHERE weight < 40"
        );
        let number = |value: &Value| match value {
            Value::UInt64(number) => *number,
            Value::Int64(number) => u64::try_from(*number).expect("positive"),
            other => panic!("not a number: {other:?}"),
        };
        let (rows, _) = self.run(&sql, true);
        let mut answered = rows
            .iter()
            .map(|row| (number(&row[0]), number(&row[1])))
            .collect::<Vec<_>>();
        answered.sort_unstable();
        assert_eq!(answered.len(), expected.len(), "{stage}: {sql}");
        assert!(
            answered == expected,
            "{stage}: {sql} numbers rows differently"
        );
    }

    #[allow(clippy::too_many_lines)]
    fn check(&self, stage: &str, fresh: bool) {
        // Every value printable ASCII: partitions fold case only.
        self.check_window(
            stage,
            "hub_ref",
            |parcel| parcel.hub_ref.as_ref(),
            str::to_lowercase,
        );
        // An accented spelling among them: it joins its base letter's.
        self.check_window(
            stage,
            "carrier",
            |parcel| parcel.carrier.as_ref(),
            |value| value.to_lowercase().replace('\u{f3}', "o"),
        );
        let pair_rows = |pairs: &[(u64, u64)]| -> Rows {
            pairs
                .iter()
                .map(|(seq, n)| vec![Value::UInt64(*seq), Value::UInt64(*n)])
                .collect()
        };
        // A parcel that no round of changes touches.
        let (seq, parcel) = self
            .model
            .iter()
            .find(|(seq, _)| **seq % 101 > 3 && **seq > 4_000)
            .expect("parcel");
        let spelled = parcel.parcel_ref.to_uppercase();
        let decoded = self.both(
            stage,
            &format!("SELECT seq, weight FROM parcels WHERE parcel_ref = '{spelled}'"),
            vec![vec![Value::UInt64(*seq), Value::Int64(parcel.weight)]],
        );
        if fresh {
            assert!(
                decoded < PARCELS / 10,
                "{stage}: a key equality decoded {decoded} values"
            );
        }
        // NO PAD: the key with a trailing space is another key.
        self.both(
            stage,
            &format!(
                "SELECT seq FROM parcels WHERE parcel_ref = '{} '",
                parcel.parcel_ref
            ),
            Vec::new(),
        );
        let other = reference("p", 12_345);
        let listed = [&other, &spelled]
            .iter()
            .filter(|wanted| {
                self.model.values().any(|parcel| {
                    compare_collated_text(&parcel.parcel_ref, wanted, Collation::Utf8mb40900AiCi)
                        == std::cmp::Ordering::Equal
                })
            })
            .count();
        let decoded = self.both(
            stage,
            &format!(
                "SELECT COUNT(*) FROM parcels WHERE parcel_ref IN ('{other}', '{spelled}', 'none')"
            ),
            vec![vec![Value::UInt64(listed as u64)]],
        );
        if fresh {
            assert!(
                decoded < PARCELS / 10,
                "{stage}: a key IN list decoded {decoded} values"
            );
        }
        self.both(
            stage,
            &format!("SELECT parcel_ref FROM parcels WHERE parcel_ref = '{spelled}'"),
            vec![vec![Value::Utf8(parcel.parcel_ref.clone())]],
        );

        // Joins whose small side names the keys.
        let by_hub = self.joined(
            |parcel| parcel.hub_ref.as_ref(),
            |hub| &hub.hub_ref,
            Collation::Utf8mb40900AiCi,
        );
        assert!(by_hub.len() > 100, "{stage}: {} pairs", by_hub.len());
        for sql in [
            "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.hub_ref = h.hub_ref WHERE h.region = 7",
            "SELECT p.seq, h.n FROM parcels p JOIN hubs h ON h.hub_ref = p.hub_ref WHERE h.region = 7",
        ] {
            let decoded = self.both(stage, sql, pair_rows(&by_hub));
            if fresh {
                assert!(
                    decoded < PARCELS,
                    "{stage}: {sql} decoded {decoded} values of a {PARCELS}-row table"
                );
            }
        }
        let by_code = self.joined(
            |parcel| parcel.carrier.as_ref(),
            |hub| &hub.code,
            Collation::Utf8mb4GeneralCi,
        );
        assert!(
            by_code.len() > by_hub.len(),
            "{stage}: PAD SPACE folds more"
        );
        self.both(
            stage,
            "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.carrier = h.code WHERE h.region = 7",
            pair_rows(&by_code),
        );
        let by_zone = self.joined(
            |parcel| parcel.zone.as_ref(),
            |hub| &hub.zone,
            Collation::Utf8mb4Bin,
        );
        assert!(!by_zone.is_empty(), "{stage}: no binary matches");
        self.both(
            stage,
            "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.zone = h.zone WHERE h.region = 7",
            pair_rows(&by_zone),
        );

        // A left join keeps the hubs nothing references.
        let mut left = pair_rows(&by_zone);
        for n in (0..HUBS).filter(|n| n % REGIONS == 7) {
            if !by_zone.iter().any(|(_, hub)| *hub == n) {
                left.push(vec![Value::Null, Value::UInt64(n)]);
            }
        }
        self.both(
            stage,
            "SELECT p.seq, h.n FROM hubs h LEFT JOIN parcels p ON p.zone = h.zone WHERE h.region = 7",
            left,
        );

        // The latest row per key, joined to a few keys: the join's keys
        // reach the scan beneath the window, whose partitions they keep or
        // drop whole.
        let mut latest = Vec::new();
        for n in (0..HUBS).filter(|n| n % REGIONS == 7) {
            let wanted = hub(n).hub_ref;
            if let Some(top) = self
                .model
                .iter()
                .filter(|(_, parcel)| {
                    parcel
                        .hub_ref
                        .as_ref()
                        .is_some_and(|value| value.to_lowercase() == wanted)
                })
                .map(|(seq, parcel)| (parcel.weight, *seq))
                .max()
            {
                latest.push(vec![Value::UInt64(top.1), Value::UInt64(n)]);
            }
        }
        assert!(!latest.is_empty(), "{stage}: no latest rows");
        let decoded = self.both(
            stage,
            "SELECT r.seq, h.n FROM hubs h JOIN (SELECT seq, hub_ref, ROW_NUMBER() OVER \
             (PARTITION BY hub_ref ORDER BY weight DESC, seq DESC) AS rn FROM parcels) r \
             ON r.hub_ref = h.hub_ref WHERE h.region = 7 AND r.rn = 1",
            latest,
        );
        if fresh {
            assert!(
                decoded < PARCELS,
                "{stage}: the latest row per key decoded {decoded} values"
            );
        }

        // A literal on one side of the join equality.
        let wanted = hub(207);
        let literal = pair_rows(
            &by_hub
                .iter()
                .copied()
                .filter(|(_, n)| *n == 207)
                .collect::<Vec<_>>(),
        );
        let decoded = self.both(
            stage,
            &format!(
                "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.hub_ref = h.hub_ref \
                 WHERE h.hub_ref = '{}'",
                wanted.hub_ref.to_uppercase()
            ),
            literal,
        );
        if fresh {
            assert!(
                decoded < PARCELS,
                "{stage}: a literal across a join decoded {decoded} values"
            );
        }
    }
}

#[test]
fn text_key_lookups_and_joins_answer_as_a_full_read_does() {
    let mut fixture = Fixture::new();
    fixture.check("snapshot", true);
    fixture.change(1);
    fixture.check("changes in the memtable", false);
    fixture.parcels.flush().expect("flush");
    fixture.check("flushed over the snapshot", false);
    fixture.change(2);
    fixture.parcels.flush().expect("flush");
    fixture.parcels.compact().expect("compact");
    fixture.check("compacted", true);
}

#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_text_key_lookups() {
    let fixture = Fixture::new();
    let key = reference("p", 77_777).to_uppercase();
    let hub_key = hub(207).hub_ref;
    for sql in [
        format!("SELECT seq, weight FROM parcels WHERE parcel_ref = '{key}'"),
        format!("SELECT COUNT(*) FROM parcels WHERE parcel_ref = '{key}'"),
        "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.hub_ref = h.hub_ref WHERE h.region = 7"
            .to_owned(),
        "SELECT p.seq, h.n FROM parcels p JOIN hubs h ON h.hub_ref = p.hub_ref WHERE h.region = 7"
            .to_owned(),
        format!(
            "SELECT p.seq, h.n FROM hubs h JOIN parcels p ON p.hub_ref = h.hub_ref \
             WHERE h.hub_ref = '{hub_key}'"
        ),
        "SELECT r.seq, h.n FROM hubs h JOIN (SELECT seq, hub_ref, ROW_NUMBER() OVER (PARTITION BY hub_ref ORDER BY weight DESC, seq DESC) AS rn FROM parcels) r ON r.hub_ref = h.hub_ref WHERE h.region = 7 AND r.rn = 1"
            .to_owned(),
        "SELECT COUNT(*) FROM (SELECT seq, ROW_NUMBER() OVER (PARTITION BY hub_ref ORDER BY weight DESC, seq DESC) AS rn FROM parcels) ranked WHERE rn = 1"
            .to_owned(),
        "SELECT COUNT(*) FROM (SELECT seq, ROW_NUMBER() OVER (PARTITION BY weight ORDER BY seq DESC) AS rn FROM parcels) ranked WHERE rn = 1"
            .to_owned(),
    ] {
        for index in [false, true] {
            fixture.run(&sql, index);
            let mut samples = (0..9)
                .map(|_| {
                    let started = std::time::Instant::now();
                    fixture.run(&sql, index);
                    started.elapsed().as_secs_f64() * 1_000.0
                })
                .collect::<Vec<_>>();
            samples.sort_by(f64::total_cmp);
            println!(
                "{:>8.2} ms median {:>8.2} ms min  index={index}  {sql}",
                samples[samples.len() / 2],
                samples[0]
            );
        }
    }
}
