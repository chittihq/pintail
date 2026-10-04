use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    ExecError, Execution, ExplainError, LogicalPlanner, Optimizer, PhysicalPlanner,
    SnapshotScanProvider, explain_analyze_statement_with_deadline, explain_statement,
};
use pintail_meta::{DatabaseRecord, MetaStore, TableRecord};

use crate::admission::{QueryAdmission, QueryClass, shared_admission};
use crate::plan_cache::{self, PlanCache, PlanCacheStats};
use crate::replica_cache::{
    self, CacheKey, FileStamp, Lookup, ReplicaCache, ReplicaCacheStats, ReplicaStamp, TableStamp,
};
use crate::result_rows::ResultRows;
use crate::shared_query::{Join, SharedQueryKey, shared_queries};
use pintail_probe::{ProbeReport, SourceTable};
use pintail_sql::{
    Binder, BoundExprKind, BoundJoinKind, BoundQuery, ColumnFacts, DEFAULT_TEXT_COLLATION,
    IndexFacts, MetadataError, SourceFacts, Statement, execute_metadata, parse_statement,
};
use pintail_store::TableSnapshot;
use pintail_types::{DataType, Value};
use thiserror::Error;

/// Default hard memory ceiling for one client query.
///
/// Sized for an analytical join rather than a point lookup. At 64MiB a
/// nine-way dashboard join over a four-thousand-row table was refused - not a
/// pathological query, just the shape a health or funnel report takes - and
/// the operator's only signal was a byte count. Operators spill rather than
/// fail above this, so a larger ceiling trades resident memory for fewer
/// spills; the concurrent total, not this, is what bounds the process.
pub const DEFAULT_QUERY_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
/// Default result row ceiling for HTTP and wire clients.
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// How long a replica holding a table that would not open keeps answering
/// before the next query reopens that table. Short enough that a table
/// whose store recovers is live again within a second, long enough that a
/// table which never recovers - an in-place type change with no prior
/// definition - costs one load a second rather than one per query.
const UNREADABLE_TABLE_RETRY: Duration = Duration::from_secs(1);

/// Refusal for transaction control on a local database.
///
/// A local database autocommits every statement
/// (`docs/design/writable-mode.md`, phase 4). Accepting `BEGIN` ... `ROLLBACK`
/// as a no-op therefore reports that a write was undone when it is durably
/// stored, which is worse than refusing: a client cannot detect it.
pub(crate) const TRANSACTION_CONTROL_UNSUPPORTED: &str = "explicit transactions are not supported on a local database: every statement is its own \
     autocommit transaction";
/// Refusal for `SET autocommit=0` on a local database - the other way a
/// client asks for atomicity across statements.
pub(crate) const AUTOCOMMIT_REQUIRED: &str = "autocommit cannot be disabled on a local database: every statement is its own autocommit \
     transaction";

/// A query output field in `MySQL` presentation order.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // independent per-column wire facts
pub struct QueryField {
    /// Presentation metadata retained independently of execution carriers.
    pub wire_column: Option<pintail_protocol::Column>,
    pub name: String,
    pub data_type: Option<DataType>,
    pub nullable: bool,
    /// Resolved text collation, absent for non-text results.
    pub collation: Option<String>,
    /// Direct `GROUP_CONCAT` projections choose VARCHAR versus TEXT/BLOB on
    /// the wire from the connection's `group_concat_max_len`.
    pub group_concat: bool,
    /// Spatial column: advertised as `MYSQL_TYPE_GEOMETRY` on the wire.
    pub geometry: bool,
    /// Source `TIMESTAMP` column: advertised as `MYSQL_TYPE_TIMESTAMP`.
    pub timestamp: bool,
    /// Wire-metadata override for direct projections whose VALUES stay
    /// variable-width text deliberately (`SEC_TO_TIME`'s fraction follows
    /// its input), but whose column TYPE matches `MySQL`'s.
    pub wire_hint: Option<WireTypeHint>,
}

/// The column type `MySQL` advertises for a handful of text-carried results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireTypeHint {
    /// `SEC_TO_TIME`/`MAKETIME`: `MYSQL_TYPE_TIME`.
    Time,
    /// `CONVERT_TZ`: `MYSQL_TYPE_DATETIME`.
    Datetime,
    /// `JSON_UNQUOTE`/`->>`: `MYSQL_TYPE_BLOB` with the binary collation.
    JsonText,
}

/// Physical work observed while executing one query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryStats {
    pub duration_ms: u64,
    pub rows: usize,
    pub batches: usize,
    pub segments_read: usize,
    pub segments_pruned: usize,
    pub blocks_read: usize,
    pub blocks_pruned: usize,
    pub blocks_decoded: usize,
}

/// Typed, bounded result returned by Pintail's shared query service.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryOutput {
    pub fields: Vec<QueryField>,
    pub rows: ResultRows,
    pub stats: QueryStats,
    pub truncated: bool,
    /// Rows a WRITE changed, when the statement changed rows instead of
    /// returning them. `None` is a result set - every read answers `None`,
    /// so a query can never be mistaken for a write - and `Some` makes the
    /// server answer with an OK packet carrying this count.
    pub affected: Option<u64>,
}

/// Where a result too large to hold whole goes while execution produces
/// it.
pub trait RowSink {
    /// The result's fields, once, before any rows. `false` when whoever
    /// reads the rows has gone; the query then stops.
    fn begin(&mut self, fields: &[QueryField]) -> bool;

    /// The next rows, in order. `false` stops the query the same way.
    fn rows(&mut self, rows: ResultRows) -> bool;
}

/// How a statement answered.
#[derive(Debug)]
pub enum Answer {
    /// Every row, held.
    Whole(QueryOutput),
    /// The rows went to the sink as they were produced.
    Streamed {
        /// How many rows went.
        rows: usize,
        /// The statement's work.
        stats: QueryStats,
    },
}

/// Rows a result holds whole before it streams. A result this small is
/// answered at once and can be handed to identical requests waiting on it;
/// a larger one goes to the reader as it is produced, holding a bounded
/// part of itself at a time.
pub const STREAM_AFTER_ROWS: usize = 16_384;

/// Failure from loading or querying one mirrored database.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QueryError {
    #[error("database does not exist")]
    DatabaseNotFound,
    #[error("replica is not ready: {0}")]
    NotReady(String),
    #[error("{0}")]
    Invalid(String),
    /// A statement the engine understood and rejected for a reason `MySQL`
    /// names with a specific error code - kept apart from
    /// [`QueryError::Invalid`] so the wire server can answer with `MySQL`'s
    /// errno/SQLSTATE instead of a blanket parse error.
    #[error("{message}")]
    Rejected {
        /// Which `MySQL` error class the rejection belongs to.
        rejection: SqlRejection,
        /// Human-readable detail.
        message: String,
    },
    #[error("query engine failed: {0}")]
    Internal(String),
    #[error("query execution was interrupted after max_execution_time elapsed")]
    Interrupted,
    #[error("too many concurrent queries; the server is at its execution limit, retry shortly")]
    Overloaded,
}

/// The `MySQL` error classes Pintail distinguishes on the wire. Each maps
/// to one errno/SQLSTATE pair; everything else stays a 1064 parse error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlRejection {
    /// 1049: the qualified database does not exist.
    UnknownDatabase,
    /// 1146: the table does not exist.
    UnknownTable,
    /// 1054: the column (or relation qualifier) does not exist.
    UnknownColumn,
    /// 1052: an unqualified name matches more than one input.
    AmbiguousColumn,
    /// 1055: a selected column is neither grouped nor aggregated.
    UngroupedColumn,
    /// 1111: a group function appeared where no aggregation scope exists.
    GroupFunctionMisplaced,
    /// 1690: numeric evaluation left the result type's range.
    OutOfRange,
    /// 1050: `CREATE TABLE` named an existing table.
    TableExists,
    /// 1062: a write repeated a unique key.
    DuplicateKey,
    /// 1048: a `NOT NULL` column received no value.
    NotNull,
    /// 1406: a character value exceeds its declared width.
    DataTooLong,
    /// 1242: a scalar subquery produced more than one row.
    SubqueryRows,
    /// 3143: a JSON path expression does not parse.
    InvalidJsonPath,
    /// 1582: a built-in function was called with the wrong number of
    /// arguments.
    ParameterCount,
    /// 1210: a function's arguments have no answer.
    WrongArguments,
    /// 3513: binary bitwise operands have unequal lengths.
    BinaryBitwiseLength,
    /// 3514: aggregate binary width exceeds 511 bytes.
    BinaryBitwiseAggregateWidth,
    /// 3854: a required character-set conversion is invalid.
    CharacterConversion,
    /// 1267: two text operands whose collations tie.
    CollationMixOfTwo,
    /// 1271: an operation over several texts whose collations tie.
    CollationMixOfSeveral,
    /// A spatial function refused its arguments, by `MySQL`'s class.
    Spatial(pintail_exec::SpatialError),
}

/// The metadata files a database's signature was last read against, and
/// that signature.
///
/// The files stand in for the signature so that a statement learns nothing
/// changed from two `stat` calls, without opening the store. They are not
/// the same thing: a commit writes the store's write-ahead log, syncs it,
/// and only then publishes itself to readers. A signature read between the
/// write and the publication sees the files as the commit leaves them and
/// the rows as they were before it, and remembered against those files it
/// would hide the commit from every statement until another write moved
/// the files again. So a signature is held as read only provisionally:
/// once its files have stood unchanged for [`SIGNATURE_SETTLES_AFTER`] it
/// is read once more, and that reading - which no write preceded so
/// closely - is the one kept.
///
/// A process that is the only writer of its data directory needs no file
/// to tell it the store was written: every commit it makes moves
/// [`pintail_meta::write_generation`], which stands in for the files. That
/// number moves as a commit begins and again once it is published, so a
/// signature read against it is never kept past a commit; the second look
/// is taken all the same.
#[derive(Clone, Debug)]
struct SignatureMemo {
    files: Vec<FileStamp>,
    /// The store's write generation the signature was read against, in a
    /// process that is its store's only writer; the files are then not
    /// looked at.
    generation: Option<u64>,
    signature: u64,
    /// When the signature was read.
    read_at: Instant,
    /// Whether it was read with the files already settled.
    settled: bool,
}

/// How long a signature's files must have stood unchanged before the
/// signature read against them is taken as final. Far longer than a commit
/// takes to publish after writing its log.
const SIGNATURE_SETTLES_AFTER: Duration = Duration::from_secs(1);

/// What proved a loaded replica current, in a process that is the only
/// writer of its data directory: the three numbers that between them move
/// with every change a query could see - a metadata commit, a table
/// directory appearing or going, a table's published generation.
///
/// The numbers are read before the replica is checked against its stamp,
/// so a change made after the check moved at least one of them. While all
/// three stand, the check would come to what it came to: the replica is
/// current, without reading each table's generation again - a lookup per
/// table for every statement, on a database of hundreds of tables more
/// than a small statement otherwise costs. A proof is still let lapse
/// after [`PROOF_STANDS`], so the stamp's own second look at a metadata
/// signature read close behind a commit, and its retry of a table that
/// would not open, both happen as they did.
#[derive(Clone, Copy, Debug)]
struct CurrentProof {
    /// The load proved current.
    load_id: u64,
    metadata: u64,
    directories: u64,
    publications: u64,
    proved_at: Instant,
}

/// How long a proof is reused before the replica is checked against its
/// stamp again.
const PROOF_STANDS: Duration = Duration::from_millis(500);

/// A tables directory as last listed: its modification time, when it was
/// listed, and each entry's name, path and whether it is a directory.
struct TableListing {
    modified: Option<std::time::SystemTime>,
    listed_at: std::time::SystemTime,
    /// The table-directory epoch read before listing, in a process that is
    /// its data directory's only writer: the listing then stands for as
    /// long as the epoch does, and the directory is not looked at.
    epoch: Option<u64>,
    entries: Arc<[(String, PathBuf, bool)]>,
}

/// Opens reader-pinned table snapshots and runs Pintail's native SQL engine.
#[derive(Clone)]
pub struct ReplicaEngine {
    data_dir: PathBuf,
    metadata_path: PathBuf,
    memory_limit: usize,
    /// Bounds concurrent execution. Without it the server admits every
    /// query and converts overload into unbounded latency rather than
    /// backpressure (see `tests/load/results.md`).
    admission: std::sync::Arc<QueryAdmission>,
    /// The process-wide replica cache, revalidated per request against
    /// on-disk file stamps: reopening every table snapshot (manifest read
    /// plus WAL merge) and the metadata store cost ~200ms on EVERY query,
    /// the fixed floor under the whole benchmark board - and one copy per
    /// connection was the floor under the process's memory.
    cache: Arc<ReplicaCache<LoadedReplica>>,
    /// Per database, the metadata files last seen and the signature they
    /// carried: when the files have not moved the signature is known
    /// without opening the store.
    signatures: Arc<Mutex<HashMap<String, SignatureMemo>>>,
    /// A read connection kept open for the signature query, so a request
    /// that follows a metadata write does not pay a store open and a
    /// migration check to learn that nothing it reads has changed.
    signature_reader: Arc<Mutex<Option<MetaStore>>>,
    /// Per tables directory, its entries as last listed, so a stamp lists
    /// the directory again only when the directory itself moved.
    listings: Arc<Mutex<HashMap<PathBuf, TableListing>>>,
    /// Per database, what last proved its loaded replica current, in a
    /// process that is its data directory's only writer.
    proofs: Arc<Mutex<HashMap<String, CurrentProof>>>,
    /// The longest a statement waits for a table recopied after a schema
    /// change before it is refused (see [`Self::execute_answer`]).
    recopy_wait: Duration,
    /// Statements kept prepared between executions, or `None` when every
    /// execution prepares its own (see [`crate::plan_cache`]).
    plans: Option<Arc<PlanCache<KeptSelect>>>,
}

/// How long a statement waits, by default, for a table being recopied after
/// a schema change: `PINTAIL_TABLE_RECOPY_WAIT_MS`, else ten seconds. Zero
/// refuses at once.
fn default_recopy_wait() -> Duration {
    static WAIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| {
        Duration::from_millis(
            std::env::var("PINTAIL_TABLE_RECOPY_WAIT_MS")
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(10_000),
        )
    })
}

/// How often a waiting statement asks whether the recopy has finished.
const RECOPY_POLL: Duration = Duration::from_millis(25);

/// Records whether a result began to reach its reader, so a statement is
/// only ever retried while nothing of it has been sent.
struct TrackedSink<'a> {
    inner: &'a mut dyn RowSink,
    begun: bool,
}

impl RowSink for TrackedSink<'_> {
    fn begin(&mut self, fields: &[QueryField]) -> bool {
        self.begun = true;
        self.inner.begin(fields)
    }

    fn rows(&mut self, rows: ResultRows) -> bool {
        self.begun = true;
        self.inner.rows(rows)
    }
}

impl std::fmt::Debug for ReplicaEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplicaEngine")
            .field("data_dir", &self.data_dir)
            .field("metadata_path", &self.metadata_path)
            .field("memory_limit", &self.memory_limit)
            .finish_non_exhaustive()
    }
}

/// What admission classification settled for one statement: the replica it
/// proved current, the statement prepared against it when classifying had
/// to cost it, and whether it runs as a short query.
struct Classified {
    replica: Arc<LoadedReplica>,
    prepared: Option<PreparedSelect>,
    short: bool,
    /// Whether the statement's work is small enough to run on the thread
    /// that received it: see [`ReplicaEngine::execute_answer_inline`].
    inline: bool,
}

/// Why classification settled nothing for a statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unclassified {
    /// The statement itself: its shape is not one classification bounds,
    /// or it does not bind. Asking again gives the same answer.
    Shape,
    /// The moment: no current replica is cached to classify against.
    Unready,
}

/// Where a statement is being executed from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    /// A thread that may block: waits for capacity, loads replicas, joins
    /// identical requests.
    Worker,
    /// The thread that received the statement, which serves other
    /// connections too: only bounded work, and nothing that waits.
    Inline,
}

/// What one pass over a statement came to.
enum Attempt {
    Answered(Answer),
    Declined(InlineAnswer),
}

/// The outcome of [`ReplicaEngine::execute_answer_inline`].
#[derive(Debug)]
pub enum InlineAnswer {
    /// The statement ran; this is its result.
    Answered(Answer),
    /// The statement's work is not bounded tightly enough to run inline.
    /// Nothing was executed, and asking again will say the same.
    NotBounded,
    /// The statement could not run inline at this moment - its replica has
    /// to be loaded, or capacity waited for. Nothing was executed.
    NotNow,
}

/// The longest statement text considered for inline execution. Parsing is
/// part of the inline work, and it grows with the text.
const INLINE_STATEMENT_BYTES: usize = 2048;

/// One SELECT bound and planned, with the result metadata binding decided.
/// Built once per statement: by admission classification when it costs
/// the statement, otherwise just before execution.
#[derive(Clone)]
struct PreparedSelect {
    physical: pintail_exec::PhysicalPlan,
    collation: pintail_exec::collation::Collation,
    wire_columns: Vec<pintail_protocol::Column>,
    result_nullability: Vec<Option<bool>>,
    result_collations: Vec<Option<String>>,
    group_concat: Vec<bool>,
    wire_hints: Vec<Option<WireTypeHint>>,
}

/// A statement kept prepared: its plan, and what else its text decides
/// that an execution asks for before it runs.
struct KeptSelect {
    prepared: PreparedSelect,
    /// Whether the statement has the shape classification bounds
    /// ([`pintail_sql::has_bounded_planning_shape`]); one that has not is
    /// always a general query.
    bounded_planning: bool,
    /// [`pintail_sql::has_bounded_admission_shape`].
    bounded_admission: bool,
    /// [`pintail_sql::has_bounded_table_less_shape`]: the statement may
    /// run on the thread that received it.
    table_less: bool,
    /// The statement's own `MAX_EXECUTION_TIME` hint, in milliseconds.
    execution_time_hint: Option<u64>,
}

struct LoadedReplica {
    server_version: String,
    /// Identifies this load, and only this one. Taken fresh every time a
    /// replica is built, so anything that reloads it - a CDC commit, a
    /// local write, a schema change - gives the same statement a different
    /// shared-execution key instead of the answer from before the change.
    load_id: u64,
    database: DatabaseRecord,
    tables: Vec<TableRecord>,
    targets: Vec<ReaderTarget>,
    /// The catalog and column facts this load answers with. Both follow
    /// from the load alone, so they are built on first use and shared by
    /// every query the load serves rather than rebuilt per query.
    catalog: OnceLock<CatalogSnapshot>,
    facts: OnceLock<SourceFacts>,
}

impl LoadedReplica {
    /// Whether everything this load holds is small enough that a statement
    /// of bounded shape over it is a short query whatever it reads.
    fn is_tiny(&self) -> bool {
        self.targets.len() <= 16
            && self.targets.iter().fold(0_u64, |rows, table| {
                rows.saturating_add(table.snapshot.physical_row_upper_bound())
            }) <= 1024
            && self
                .targets
                .iter()
                .map(|table| table.snapshot.schema().columns().len())
                .sum::<usize>()
                <= 128
            && self
                .targets
                .iter()
                .map(|table| table.snapshot.segment_count())
                .sum::<usize>()
                <= 128
            && self.targets.iter().fold(0_u64, |bytes, table| {
                bytes.saturating_add(table.stored_bytes())
            }) <= 4 * 1024 * 1024
    }

    fn catalog(&self) -> Result<&CatalogSnapshot, QueryError> {
        if let Some(catalog) = self.catalog.get() {
            return Ok(catalog);
        }
        let catalog = build_catalog(self)?;
        Ok(self.catalog.get_or_init(|| catalog))
    }

    fn facts(&self) -> &SourceFacts {
        self.facts.get_or_init(|| column_facts(self))
    }
}

/// Hands out [`LoadedReplica::load_id`]. Monotonic, so a number is never
/// reused by a later load.
static NEXT_REPLICA_LOAD_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct ReaderTarget {
    source: SourceTable,
    /// Schema generation the snapshot was opened under; a newer one means
    /// the table must be reopened even if its files did not move.
    version: u32,
    snapshot: TableSnapshot,
    /// Whether the table holds a complete copy of its source. A table whose
    /// snapshot is still running, or whose first copy failed before it
    /// finished, has a store that is empty or partial; answering from it
    /// would be silently wrong, so its scans are refused as not ready.
    ready: bool,
    /// Why the table's store could not be opened, when it could not. Its
    /// snapshot is then empty and its scans are refused with the reason.
    unreadable: Option<String>,
    /// The snapshot's [`TableSnapshot::stored_bytes`], taken the first time
    /// the short-query screen asks rather than on every query.
    stored_bytes: OnceLock<u64>,
}

impl ReaderTarget {
    fn new(
        source: SourceTable,
        version: u32,
        snapshot: TableSnapshot,
        ready: bool,
        unreadable: Option<String>,
    ) -> Self {
        Self {
            source,
            version,
            snapshot,
            ready,
            unreadable,
            stored_bytes: OnceLock::new(),
        }
    }

    fn stored_bytes(&self) -> u64 {
        *self
            .stored_bytes
            .get_or_init(|| self.snapshot.stored_bytes())
    }
}

static SHARED_REPLICA_CACHE: OnceLock<Arc<ReplicaCache<LoadedReplica>>> = OnceLock::new();
static SHARED_PLAN_CACHE: OnceLock<Option<Arc<PlanCache<KeptSelect>>>> = OnceLock::new();

/// The plan cache every engine in the process shares, or `None` when it is
/// turned off: one kept preparation of a statement however many connections
/// send it.
fn shared_plan_cache() -> Option<Arc<PlanCache<KeptSelect>>> {
    SHARED_PLAN_CACHE
        .get_or_init(|| {
            plan_cache::configured_bounds()
                .map(|(entries, bytes)| Arc::new(PlanCache::new(entries, bytes)))
        })
        .clone()
}

/// What the shared plan cache has done since startup; all zero when it is
/// turned off.
#[must_use]
pub fn plan_cache_stats() -> PlanCacheStats {
    shared_plan_cache().map_or_else(PlanCacheStats::default, |plans| plans.stats())
}

/// The replica cache every engine in the process shares: one loaded copy
/// of a database however many connections and requests read it.
fn shared_replica_cache() -> Arc<ReplicaCache<LoadedReplica>> {
    Arc::clone(SHARED_REPLICA_CACHE.get_or_init(|| {
        Arc::new(ReplicaCache::new(
            replica_cache::default_capacity(),
            pintail_exec::shared_memory_budget(),
        ))
    }))
}

/// What the shared replica cache has done since startup.
#[must_use]
pub fn replica_cache_stats() -> ReplicaCacheStats {
    shared_replica_cache().stats()
}

impl ReplicaEngine {
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>, metadata_path: impl Into<PathBuf>) -> Self {
        // Writers publish under the canonical table directory; reading the
        // same spelling is what lets the stamp find their generations.
        let data_dir = data_dir.into();
        let data_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
        Self {
            data_dir,
            metadata_path: metadata_path.into(),
            memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
            admission: shared_admission(),
            cache: shared_replica_cache(),
            signatures: Arc::new(Mutex::new(HashMap::new())),
            signature_reader: Arc::new(Mutex::new(None)),
            listings: Arc::new(Mutex::new(HashMap::new())),
            proofs: Arc::new(Mutex::new(HashMap::new())),
            recopy_wait: default_recopy_wait(),
            plans: shared_plan_cache(),
        }
    }

    /// Gives this engine a plan cache of its own with the given bounds, in
    /// place of the one the process shares; zero for either turns the
    /// cache off for this engine.
    #[must_use]
    pub fn with_plan_cache(mut self, entries: usize, bytes: usize) -> Self {
        self.plans = (entries > 0 && bytes > 0).then(|| Arc::new(PlanCache::new(entries, bytes)));
        self
    }

    /// What this engine's plan cache has done; all zero when it has none.
    #[must_use]
    pub fn plan_cache_stats(&self) -> PlanCacheStats {
        self.plans
            .as_ref()
            .map_or_else(PlanCacheStats::default, |plans| plans.stats())
    }

    /// The metadata signature for `files`, from the memo when the files are
    /// the ones last read, otherwise from the store. A store that cannot be
    /// read falls back to a hash of the files themselves, which is the old
    /// behaviour: safe, and no worse.
    fn metadata_signature(
        &self,
        database_id: &str,
        files: &[FileStamp],
        generation: Option<u64>,
    ) -> u64 {
        // Read against these same files before: the reading stands, unless
        // it was made close behind a write and is now due its second look.
        let settling = match self.signatures.lock() {
            Ok(memo) => match memo.get(database_id) {
                Some(known) if known.files == files && known.generation == generation => {
                    if known.settled || known.read_at.elapsed() < SIGNATURE_SETTLES_AFTER {
                        return known.signature;
                    }
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        let read_at = Instant::now();
        let signature = {
            let mut reader = self
                .signature_reader
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reader.is_none() {
                *reader = MetaStore::open(&self.metadata_path).ok();
            }
            let read = reader
                .as_ref()
                .and_then(|store| store.replica_signature(database_id).ok());
            if read.is_none() {
                // Reopen next time rather than keep a connection that failed.
                *reader = None;
            }
            read
        };
        let signature = signature.unwrap_or_else(|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(files, &mut hasher);
            std::hash::Hash::hash(&generation, &mut hasher);
            std::hash::Hasher::finish(&hasher)
        });
        if let Ok(mut memo) = self.signatures.lock() {
            memo.insert(
                database_id.to_owned(),
                SignatureMemo {
                    files: files.to_vec(),
                    generation,
                    signature,
                    read_at,
                    settled: settling,
                },
            );
        }
        signature
    }

    /// Forgets the signature held for `database_id`, so the next stamp
    /// reads the store whatever its files look like.
    ///
    /// For a statement refused because a table is not ready: the refusal
    /// may rest on a signature read just behind the commit that made the
    /// table ready, and whoever asks again - this statement, waiting for
    /// the copy, or the next one - must see every change committed before
    /// it asked.
    fn forget_signature(&self, database_id: &str) {
        if let Ok(mut memo) = self.signatures.lock() {
            memo.remove(database_id);
        }
    }

    /// Everything that can change what a query sees: the metadata store
    /// plus, per table, either the generation its writer published or its
    /// files. The stamp keeps tables apart, so the reload after a change
    /// touches only the table that changed.
    ///
    /// A table whose writer is open in this process - every table under
    /// replication - is stamped with the writer's generation, which moves
    /// after every change to the files a reader opens and costs no file
    /// system call to read. Any other table is walked: its manifests, WALs
    /// and immutable segment set, where any apply, flush, compaction or
    /// schema change alters at least one (path, len, mtime) triple.
    fn replica_stamp(&self, database_id: &str) -> ReplicaStamp {
        fn record(files: &mut Vec<FileStamp>, path: &Path) {
            if let Ok(meta) = std::fs::metadata(path) {
                files.push((path.to_path_buf(), meta.len(), meta.modified().ok()));
            }
        }
        let mut stamp = ReplicaStamp::default();
        // A process that is the only writer of its data directory is told
        // of every metadata commit and every new table directory by the
        // code that makes them, and asks the file system nothing here.
        // Taken before the signature is read, so a commit after this point
        // moves it past what the signature is remembered against.
        let sole_writer = pintail_store::writer_locks_retained();
        let generation = sole_writer.then(pintail_meta::write_generation);
        if !sole_writer {
            record(&mut stamp.metadata.files, &self.metadata_path);
            // Metadata writes land in SQLite's WAL, not the main file —
            // without it a replica cached between a table's files appearing
            // and its metadata rows committing stays stale until unrelated
            // data churn.
            let mut wal = self.metadata_path.as_os_str().to_owned();
            wal.push("-wal");
            record(&mut stamp.metadata.files, Path::new(&wal));
        }
        stamp.metadata.signature =
            self.metadata_signature(database_id, &stamp.metadata.files, generation);
        let Some(entries) = self.table_entries(&self.tables_root(database_id), sole_writer) else {
            return stamp;
        };
        for (name, table, is_directory) in entries.iter().cloned() {
            // A table no writer here has opened since the process started
            // is leased on first sight, so it is walked once, not per query.
            if is_directory
                && let Some(generation) = pintail_store::published_generation(&table)
                    .or_else(|| pintail_store::lease_unwritten_table(&table))
            {
                stamp.tables.insert(name, TableStamp::Published(generation));
                continue;
            }
            let mut files = Vec::new();
            if !is_directory {
                record(&mut files, &table);
                stamp.tables.insert(name, TableStamp::Files(files));
                continue;
            }
            let mut directories = vec![table];
            while let Some(directory) = directories.pop() {
                let Ok(entries) = std::fs::read_dir(&directory) else {
                    continue;
                };
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect();
                paths.sort();
                for path in paths {
                    if path.is_dir() {
                        directories.push(path);
                    } else {
                        record(&mut files, &path);
                    }
                }
            }
            stamp.tables.insert(name, TableStamp::Files(files));
        }
        self.cache
            .record_walk(stamp.files() - stamp.metadata.files.len());
        stamp
    }

    /// The entries of the tables directory `root`: from the last listing
    /// when the directory has not moved since, otherwise listed afresh.
    ///
    /// Adding, removing or renaming an entry moves a directory's
    /// modification time, but only at the file system's clock resolution, so
    /// a change in the same tick as the listing could leave it unmoved. A
    /// listing is therefore trusted only when the directory had already been
    /// still for a second when it was taken.
    fn table_entries(
        &self,
        root: &Path,
        sole_writer: bool,
    ) -> Option<Arc<[(String, PathBuf, bool)]>> {
        const SETTLED: Duration = Duration::from_secs(1);
        // The only writer of this data directory knows when its set of
        // table directories changed: the listing taken at an epoch stands
        // until the epoch moves. The epoch is read before the directory is
        // listed, so a directory that appears after the listing was taken
        // has moved it.
        let epoch = sole_writer.then(pintail_store::directory_epoch);
        if let Some(epoch) = epoch
            && let Ok(listings) = self.listings.lock()
            && let Some(listing) = listings.get(root)
            && listing.epoch == Some(epoch)
        {
            return Some(Arc::clone(&listing.entries));
        }
        let modified = if sole_writer {
            None
        } else {
            std::fs::metadata(root)
                .and_then(|meta| meta.modified())
                .ok()
        };
        if let Some(modified) = modified
            && let Ok(listings) = self.listings.lock()
            && let Some(listing) = listings.get(root)
            && listing.epoch.is_none()
            && listing.modified == Some(modified)
            && modified
                .checked_add(SETTLED)
                .is_some_and(|settled| settled < listing.listed_at)
        {
            return Some(Arc::clone(&listing.entries));
        }
        let listed_at = std::time::SystemTime::now();
        let entries: Arc<[(String, PathBuf, bool)]> = std::fs::read_dir(root)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                    entry.file_type().is_ok_and(|kind| kind.is_dir()),
                )
            })
            .collect();
        if (modified.is_some() || epoch.is_some())
            && let Ok(mut listings) = self.listings.lock()
        {
            listings.insert(
                root.to_path_buf(),
                TableListing {
                    modified,
                    listed_at,
                    epoch,
                    entries: Arc::clone(&entries),
                },
            );
        }
        Some(entries)
    }

    /// The three numbers a change a query could see moves at least one of,
    /// in the only writer of the data directory; `None` in any other
    /// process, which proves nothing by them.
    fn change_counts() -> Option<(u64, u64, u64)> {
        pintail_store::writer_locks_retained().then(|| {
            (
                pintail_meta::write_generation(),
                pintail_store::directory_epoch(),
                pintail_store::publication_epoch(),
            )
        })
    }

    /// Whether `candidate` was proved current under `counts` recently
    /// enough for the proof to stand ([`CurrentProof`]).
    fn proved_current(
        &self,
        database_id: &str,
        candidate: &LoadedReplica,
        counts: Option<(u64, u64, u64)>,
    ) -> bool {
        let Some((metadata, directories, publications)) = counts else {
            return false;
        };
        self.proofs.lock().is_ok_and(|proofs| {
            proofs.get(database_id).is_some_and(|proof| {
                proof.load_id == candidate.load_id
                    && proof.metadata == metadata
                    && proof.directories == directories
                    && proof.publications == publications
                    && proof.proved_at.elapsed() < PROOF_STANDS
            })
        })
    }

    /// `candidate` if it is the replica cached under `key` and `stamp`
    /// proves it current. `counts` must have been read before `stamp` was
    /// taken: the proof then stands until one of them moves.
    fn proved_by_stamp(
        &self,
        database_id: &str,
        key: &CacheKey,
        stamp: &ReplicaStamp,
        candidate: &Arc<LoadedReplica>,
        counts: Option<(u64, u64, u64)>,
    ) -> Option<Arc<LoadedReplica>> {
        let replica = revalidated(&self.cache, key, stamp, candidate)?;
        // A replica holding a table that would not open is tried again on
        // the cache's own schedule, which only the stamp's path keeps.
        if let Some((metadata, directories, publications)) = counts
            && replica
                .targets
                .iter()
                .all(|target| target.unreadable.is_none())
            && let Ok(mut proofs) = self.proofs.lock()
        {
            proofs.insert(
                database_id.to_owned(),
                CurrentProof {
                    load_id: replica.load_id,
                    metadata,
                    directories,
                    publications,
                    proved_at: Instant::now(),
                },
            );
        }
        Some(replica)
    }

    /// `candidate`, the replica cached under `key`, if it is still what
    /// this database answers from: proved by the stamp, or - in the only
    /// writer of the data directory - by nothing having moved since the
    /// stamp last proved it ([`CurrentProof`]).
    fn still_current(
        &self,
        database_id: &str,
        key: &CacheKey,
        candidate: &Arc<LoadedReplica>,
    ) -> Option<Arc<LoadedReplica>> {
        // Read before the stamp is taken.
        let counts = Self::change_counts();
        if self.proved_current(database_id, candidate, counts) {
            return Some(Arc::clone(candidate));
        }
        let stamp = self.replica_stamp(database_id);
        self.proved_by_stamp(database_id, key, &stamp, candidate, counts)
    }

    fn tables_root(&self, database_id: &str) -> PathBuf {
        self.data_dir
            .join("databases")
            .join(database_id)
            .join("tables")
    }

    fn cache_key(&self, database_id: &str) -> CacheKey {
        (self.data_dir.clone(), database_id.to_owned())
    }

    // Classification reads only cached metadata. Freshness is checked under
    // the permit; a stale candidate releases it before requesting general
    // capacity to load storage.
    //
    // A statement whose cost must be known is prepared to cost it, and that
    // preparation is the one execution runs. `None` sends the statement down
    // the general path, which loads the replica and prepares it there.
    fn classify(
        &self,
        database_id: &str,
        sql: &str,
        statement: &Statement,
    ) -> Result<Classified, Unclassified> {
        if !pintail_sql::has_bounded_planning_shape(statement) {
            return Err(Unclassified::Shape);
        }
        let key = self.cache_key(database_id);
        let replica = self.cache.peek(&key).ok_or(Unclassified::Unready)?;
        // What proves the replica current as the statement arrives: a
        // standing proof, or the stamp taken now.
        let counts = Self::change_counts();
        let stamp = (!self.proved_current(database_id, &replica, counts))
            .then(|| self.replica_stamp(database_id));
        let current_on_arrival = || match &stamp {
            Some(stamp) => self.proved_by_stamp(database_id, &key, stamp, &replica, counts),
            None => Some(Arc::clone(&replica)),
        };
        // Measured on the cached replica, which is proved current as of
        // the statement's arrival before anything runs: a short query never
        // reads a snapshot a commit has already superseded, and the sizes
        // screened are that snapshot's.
        let tiny = pintail_sql::has_bounded_admission_shape(statement) && replica.is_tiny();
        if tiny {
            return current_on_arrival()
                .map(|replica| Classified {
                    replica,
                    prepared: None,
                    short: true,
                    inline: pintail_sql::has_bounded_table_less_shape(statement),
                })
                .ok_or(Unclassified::Unready);
        }
        let prepared = Self::prepare_select(
            statement,
            sql,
            replica.catalog().map_err(|_| Unclassified::Unready)?,
            replica.facts(),
            &replica.database.name,
            true,
        )
        .map_err(|_| Unclassified::Shape)?;
        let cost = build_provider(&replica)
            .map_err(|_| Unclassified::Unready)?
            .admission_cost(&prepared.physical);
        // A statement that reads no table does work bounded by its own
        // text, whatever its expressions are. One that reads a table is not
        // run inline however few rows it reads: measured, a ten-row scan
        // run inline answered sooner alone and with a worse tail at eight
        // connections, where a thread held up inside a scan holds up every
        // connection it serves.
        let inline = pintail_sql::has_bounded_table_less_shape(statement);
        // A plan classification cannot bound still runs the preparation made
        // here, on general capacity: it is the one execution would make.
        let short = QueryClass::from_cost(cost) == QueryClass::Short;
        // Preparing took time a commit could land in: prove the replica
        // current as things are now. A statement that reads no table reads
        // no snapshot a commit could supersede, so what proved the replica
        // current when it arrived is all the proof it needs - and a second
        // stamp is as many file-system calls again.
        let replica = if inline {
            current_on_arrival()
        } else {
            self.still_current(database_id, &key, &replica)
        }
        .ok_or(Unclassified::Unready)?;
        Ok(Classified {
            replica,
            prepared: Some(prepared),
            short,
            inline,
        })
    }

    fn load_replica_cached(&self, database_id: &str) -> Result<Arc<LoadedReplica>, QueryError> {
        // Every query pays the stamp before it plans anything. A miss used
        // to pay a reload of EVERY table's store - on a replica under active
        // CDC, where any commit changes the stamp, a trivial query cost more
        // in setup than in execution. A changed stamp now reopens only the
        // tables whose files moved; the log line carries both counts, and it
        // is the line to ask an operator for when a cheap query is
        // inexplicably slow.
        let stamp_started = Instant::now();
        let stamp = self.replica_stamp(database_id);
        let stamped = stamp_started.elapsed();
        let key = self.cache_key(database_id);
        if let Lookup::Hit(replica) = self.cache.lookup(&key, &stamp) {
            pintail_log::log_debug!(
                "query setup db={database_id} stamp={:.1}ms files={} published={} replica=cached",
                stamped.as_secs_f64() * 1_000.0,
                stamp.files(),
                stamp.published()
            );
            return Ok(replica);
        }
        // Something moved. Only one query reloads a database at a time; the
        // rest wait here and, more often than not, find the reload they were
        // about to repeat already in the cache.
        let reload = self.cache.reload_guard(&key);
        let _reloading = reload.lock().expect("replica reload lock");
        let previous = match self.cache.lookup(&key, &stamp) {
            Lookup::Hit(replica) => {
                pintail_log::log_debug!(
                    "query setup db={database_id} stamp={:.1}ms files={} published={} replica=coalesced",
                    stamped.as_secs_f64() * 1_000.0,
                    stamp.files(),
                    stamp.published()
                );
                return Ok(replica);
            }
            Lookup::Stale(replica, previous) => Some((replica, previous)),
            Lookup::Miss => None,
        };
        let load_started = Instant::now();
        let (replica, opened) = self.load_replica(
            database_id,
            previous
                .as_ref()
                .map(|(replica, stamp)| (replica.as_ref(), stamp)),
            &stamp,
        )?;
        let replica = Arc::new(replica);
        let resident = replica
            .targets
            .iter()
            .map(|target| target.snapshot.estimated_memtable_bytes())
            .sum::<usize>();
        pintail_log::log_debug!(
            "query setup db={database_id} stamp={:.1}ms files={} published={} replica=reloaded in {:.1}ms \
             tables={} opened={opened} resident={resident}B",
            stamped.as_secs_f64() * 1_000.0,
            stamp.files(),
            stamp.published(),
            load_started.elapsed().as_secs_f64() * 1_000.0,
            replica.targets.len()
        );
        // A table that could not be opened may open on the next attempt with
        // nothing on disk having moved, so a replica holding one is kept
        // only until the retry falls due - where it used to be served and
        // dropped, which made every query on that database reload every
        // table of it, for as long as the table stayed shut.
        let unreadable = replica
            .targets
            .iter()
            .any(|target| target.unreadable.is_some());
        // The load this one replaces answers nothing from here on, so the
        // plans prepared against it are only held memory.
        if let (Some(plans), Some((replaced, _))) = (&self.plans, &previous) {
            plans.forget_replica(replaced.load_id);
        }
        self.cache.insert(
            key,
            stamp,
            Arc::clone(&replica),
            resident,
            opened,
            unreadable.then_some(UNREADABLE_TABLE_RETRY),
        );
        Ok(replica)
    }

    #[must_use]
    pub const fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// Bounds concurrent query execution. Zero is unbounded.
    #[must_use]
    pub fn with_max_concurrent_queries(mut self, limit: usize) -> Self {
        self.admission = std::sync::Arc::new(QueryAdmission::new(limit));
        self
    }

    /// Sets the longest a statement waits for a table recopied after a
    /// schema change; zero refuses at once.
    #[must_use]
    pub const fn with_recopy_wait(mut self, wait: Duration) -> Self {
        self.recopy_wait = wait;
        self
    }

    /// Whether a table of `database_id` is being copied again after a schema
    /// change: its copy is running and its schema history holds more than
    /// the generation it was first copied with. A first copy, and a copy an
    /// operator asked for, are not: nobody can say when those end.
    fn schema_recopy_running(&self, database_id: &str) -> bool {
        let Ok(metadata) = MetaStore::open(&self.metadata_path) else {
            return false;
        };
        let Ok(tables) = metadata.tables(database_id) else {
            return false;
        };
        tables.iter().any(|table| {
            table.state == "snapshotting"
                && !table.copy_complete
                && metadata
                    .schema_history(database_id, &table.name)
                    .is_ok_and(|history| history.len() > 1)
        })
    }

    /// The configured concurrency ceiling; zero means unbounded.
    #[must_use]
    pub fn max_concurrent_queries(&self) -> usize {
        self.admission.limit()
    }

    /// Executes one read-only MySQL-dialect statement.
    ///
    /// # Errors
    ///
    /// Returns an error when the database is absent or unready, the statement
    /// is invalid or mutating, or storage/execution fails.
    pub fn execute(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
    ) -> Result<QueryOutput, QueryError> {
        self.execute_with_deadline(database_id, sql, max_rows, None)
    }

    /// Executes one statement with an optional monotonic deadline.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute`], plus
    /// [`QueryError::Interrupted`] when the deadline elapses.
    pub fn execute_with_deadline(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        match self.execute_answer(database_id, sql, max_rows, deadline, None)? {
            Answer::Whole(output) => Ok(output),
            Answer::Streamed { .. } => Err(QueryError::Internal(
                "a result streamed with nowhere to go".to_owned(),
            )),
        }
    }

    /// Executes one statement, streaming a result larger than
    /// [`STREAM_AFTER_ROWS`] into `sink` when there is one.
    ///
    /// A streamed result is never handed to identical requests waiting on
    /// this one: they execute on their own, as they would after a failure.
    /// Reaching `max_rows` while streaming is an error after the rows
    /// already sent, where a held result reports itself truncated.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute_with_deadline`].
    ///
    /// A statement that reads a table being recopied after a schema change
    /// waits for the copy, up to the engine's recopy wait and its own
    /// deadline, rather than being refused: a source `ALTER TABLE` whose new
    /// column needs values for the rows already held recopies the table, and
    /// for the second or so that takes a client would otherwise see an error
    /// where the source itself only made it wait. Past the wait the refusal
    /// stands. Nothing is retried once any of the result has been sent.
    pub fn execute_answer(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
        mut sink: Option<&mut dyn RowSink>,
    ) -> Result<Answer, QueryError> {
        let give_up = Instant::now().checked_add(self.recopy_wait);
        let mut waiting = false;
        let mut asked_again = false;
        loop {
            let mut tracked = sink.as_mut().map(|inner| TrackedSink {
                inner: &mut **inner,
                begun: false,
            });
            let result = self
                .execute_answer_once(
                    database_id,
                    sql,
                    max_rows,
                    deadline,
                    tracked.as_mut().map(|sink| sink as &mut dyn RowSink),
                    Lane::Worker,
                )
                .and_then(|attempt| match attempt {
                    Attempt::Answered(answer) => Ok(answer),
                    Attempt::Declined(_) => Err(QueryError::Internal(
                        "a worker declined a statement".to_owned(),
                    )),
                });
            if matches!(result, Err(QueryError::NotReady(_))) {
                self.forget_signature(database_id);
            }
            let retryable = matches!(result, Err(QueryError::NotReady(_)))
                && !tracked.as_ref().is_some_and(|sink| sink.begun)
                && give_up.is_some_and(|give_up| Instant::now() < give_up)
                && deadline.is_none_or(|deadline| Instant::now() < deadline);
            if !retryable {
                return result;
            }
            if waiting || self.schema_recopy_running(database_id) {
                // Once a recopy has been seen running, the statement is
                // asked again until it answers or the wait runs out: the
                // copy ending is only seen by asking.
                waiting = true;
                std::thread::sleep(RECOPY_POLL);
            } else if asked_again {
                return result;
            } else {
                // No recopy runs now, but one may have ended between the
                // refusal and this look: ask once more before it stands.
                asked_again = true;
            }
        }
    }

    /// Executes one statement on the calling thread when its work is small
    /// and nothing about it has to wait, and declines otherwise without
    /// having executed anything.
    ///
    /// The caller is a thread that serves other connections, so everything
    /// done here is bounded: the statement text is short and reads no
    /// table, its replica is already loaded and current, a slot is free
    /// now, and its result is held whole. A statement declined here is given to
    /// [`Self::execute_answer`] on a thread that may block, which starts it
    /// from the beginning. Identical concurrent statements are not joined:
    /// following one costs more than a statement this small.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute_answer`] for a statement
    /// it ran.
    pub fn execute_answer_inline(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
    ) -> Result<InlineAnswer, QueryError> {
        if sql.len() > INLINE_STATEMENT_BYTES {
            return Ok(InlineAnswer::NotBounded);
        }
        match self.execute_answer_once(database_id, sql, max_rows, deadline, None, Lane::Inline) {
            Ok(Attempt::Answered(answer)) => Ok(InlineAnswer::Answered(answer)),
            Ok(Attempt::Declined(declined)) => Ok(declined),
            // A table being recopied is waited for, and waiting is a
            // worker's to do.
            Err(QueryError::NotReady(_)) => Ok(InlineAnswer::NotNow),
            Err(error) => Err(error),
        }
    }

    /// Executes one statement on the calling thread when it is kept
    /// prepared and its work is small: one that reads no table, or a small
    /// read - a key lookup or a scan of at most [`SMALL_READ_ROWS`] stored
    /// rows that classifies as a short query. Anything else is declined
    /// with nothing executed, and - unlike [`Self::execute_answer_inline`] -
    /// without the statement being parsed: what is not kept costs a lookup.
    ///
    /// A small read may wait on a file, which the thread that received the
    /// statement must not do while other connections are queued behind it.
    /// Its execution therefore runs inside `in_place`, which the caller
    /// supplies to do whatever makes blocking on this thread safe - hand
    /// the thread's other work to another thread first - and which must
    /// call what it is given exactly once. A statement that reads no table
    /// runs without it.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute_answer`] for a statement
    /// it ran.
    pub fn execute_answer_in_place(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
        in_place: InPlace<'_>,
    ) -> Result<InlineAnswer, QueryError> {
        let started = Instant::now();
        if sql.len() > INLINE_STATEMENT_BYTES {
            return Ok(InlineAnswer::NotBounded);
        }
        let Some(plans) = &self.plans else {
            return Ok(InlineAnswer::NotBounded);
        };
        let attempt = self.execute_kept(
            plans,
            database_id,
            sql,
            max_rows,
            deadline,
            &mut None,
            Lane::Inline,
            Some(in_place),
            started,
        );
        match attempt {
            Some(Ok(Attempt::Answered(answer))) => Ok(InlineAnswer::Answered(answer)),
            Some(Ok(Attempt::Declined(declined))) => Ok(declined),
            // Not kept, or kept against a replica that has been replaced:
            // a worker prepares it. And a table being recopied is waited
            // for, which is a worker's to do as well.
            None | Some(Err(QueryError::NotReady(_))) => Ok(InlineAnswer::NotNow),
            Some(Err(error)) => Err(error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn execute_answer_once(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
        mut sink: Option<&mut dyn RowSink>,
        lane: Lane,
    ) -> Result<Attempt, QueryError> {
        let started = Instant::now();
        if sql.len() <= KEPT_STATEMENT_BYTES
            && let Some(plans) = &self.plans
            && let Some(attempt) = self.execute_kept(
                plans,
                database_id,
                sql,
                max_rows,
                deadline,
                &mut sink,
                lane,
                None,
                started,
            )
        {
            return attempt;
        }
        // What the statement had raised when it arrived: a preparation
        // that raises a warning of its own is not kept.
        let (_, warned_before) = pintail_exec::session_warning_counts();
        // Whether classification settled this statement's class for good:
        // only then is the class a kept plan would be given the one this
        // execution was.
        let mut settled_class = false;
        // Bound classification work itself. Large statements acquire general
        // capacity before parsing; small ones may qualify for the reserve.
        let (statement, classified, _permit) = if sql.len() <= 8192 {
            let statement =
                parse_statement(sql).map_err(|error| QueryError::Invalid(error.to_string()))?;
            crate::trace::mark("parsed");
            // Decided before classification, which binds and plans: a
            // statement that reads a table is declined for the price of
            // its parse.
            if lane == Lane::Inline && !pintail_sql::has_bounded_table_less_shape(&statement) {
                return Ok(Attempt::Declined(InlineAnswer::NotBounded));
            }
            let classified = self.classify(database_id, sql, &statement);
            settled_class = !matches!(classified, Err(Unclassified::Unready));
            let short = classified.as_ref().is_ok_and(|classified| classified.short);
            crate::trace::mark("classified");
            crate::trace::label("class", if short { "short" } else { "general" });
            let class = if short {
                QueryClass::Short
            } else {
                QueryClass::General
            };
            let permit = if lane == Lane::Inline {
                match &classified {
                    Ok(classified) if classified.inline => {}
                    Ok(_) | Err(Unclassified::Shape) => {
                        return Ok(Attempt::Declined(InlineAnswer::NotBounded));
                    }
                    Err(Unclassified::Unready) => {
                        return Ok(Attempt::Declined(InlineAnswer::NotNow));
                    }
                }
                // The metadata tables are built per statement, which is not
                // bounded work.
                if contains_ignore_ascii_case(sql, "information_schema") {
                    return Ok(Attempt::Declined(InlineAnswer::NotBounded));
                }
                // Waiting for a slot is a worker's to do.
                let Some(permit) = self.admission.try_admit_class_now(class) else {
                    return Ok(Attempt::Declined(InlineAnswer::NotNow));
                };
                permit
            } else {
                // Admission does not wait, so the replica classification
                // proved current is still the one to answer from.
                self.admission
                    .try_admit_class(class)
                    .ok_or(QueryError::Overloaded)?
            };
            crate::trace::mark("admitted");
            (statement, classified.ok(), permit)
        } else {
            let permit = self.admission.try_admit().ok_or(QueryError::Overloaded)?;
            let statement =
                parse_statement(sql).map_err(|error| QueryError::Invalid(error.to_string()))?;
            (statement, None, permit)
        };
        // Writes are routed before any replica is loaded: a write needs no
        // catalog snapshot, and a local database that has not created its
        // first table has none to load.
        if matches!(statement, Statement::CreateTable(_) | Statement::Insert(_)) {
            return self
                .execute_write(database_id, &statement, started)
                .map(|output| Attempt::Answered(Answer::Whole(output)));
        }
        if is_transaction_control(&statement) {
            return Err(self.transaction_control_rejection(database_id));
        }
        let (replica, mut prepared) = match classified {
            Some(classified) => (classified.replica, classified.prepared),
            None => (self.load_replica_cached(database_id)?, None),
        };
        crate::trace::mark("replica");
        let catalog = replica.catalog()?;
        let mut provider = build_provider(&replica)?;
        let table_count = replica.targets.len();
        crate::trace::mark("catalog");
        let execution_time_hint = pintail_sql::max_execution_time_hint(&statement);
        let deadline = hinted_deadline(execution_time_hint, deadline);
        let facts = replica.facts();
        match execute_metadata(&statement, catalog, Some(&replica.database.name), facts) {
            Ok(result) => {
                return Ok(Attempt::Answered(Answer::Whole(metadata_output(
                    result, started,
                ))));
            }
            Err(MetadataError::Unsupported(_)) => {}
            Err(error) => return Err(QueryError::Invalid(error.to_string())),
        }
        crate::trace::mark("metadata");
        if matches!(statement, Statement::Query(_))
            && contains_ignore_ascii_case(sql, "information_schema")
        {
            let mut statement = statement.clone();
            pintail_sql::resolve_database_function(&mut statement, &replica.database.name);
            let (metadata_catalog, metadata_provider) =
                crate::metadata_provider::MetadataProvider::new(catalog, facts)?;
            return self
                .execute_select(
                    &statement,
                    sql,
                    &metadata_catalog,
                    &metadata_provider,
                    &SourceFacts::default(),
                    "information_schema",
                    QueryStats::default(),
                    started,
                    max_rows,
                    deadline,
                    false,
                    None,
                )
                .map(Attempt::Answered);
        }
        let answer = match statement {
            Statement::Query(_) => {
                // Only a statement whose answer cannot depend on the clock,
                // the connection or a random source shares an execution,
                // and only such a statement is kept prepared.
                let repeatable = (lane != Lane::Inline || self.plans.is_some())
                    && pintail_sql::is_repeatable_statement(&statement);
                let keep = self
                    .plans
                    .as_ref()
                    .filter(|_| repeatable && settled_class && sql.len() <= KEPT_STATEMENT_BYTES);
                let mut run = || {
                    let prepared = match prepared.take() {
                        Some(prepared) => prepared,
                        None => Self::prepare_select(
                            &statement,
                            sql,
                            catalog,
                            facts,
                            &replica.database.name,
                            true,
                        )?,
                    };
                    crate::trace::mark("prepared");
                    // A preparation that raised a warning is made again by
                    // every execution, which raises it again. Preparing
                    // counts its divisions by zero from none.
                    if let Some(plans) = keep
                        && pintail_exec::session_warning_counts() == (0, warned_before)
                        && let key =
                            SharedQueryKey::for_current_session(replica.load_id, sql, max_rows)
                        && plans.seen_before(&key)
                    {
                        plans.insert(
                            key,
                            KeptSelect {
                                prepared: prepared.clone(),
                                bounded_planning: pintail_sql::has_bounded_planning_shape(
                                    &statement,
                                ),
                                bounded_admission: pintail_sql::has_bounded_admission_shape(
                                    &statement,
                                ),
                                table_less: pintail_sql::has_bounded_table_less_shape(&statement),
                                execution_time_hint,
                            },
                            plan_cache::estimated_bytes(sql),
                        );
                    }
                    self.run_prepared(
                        prepared,
                        sql,
                        &provider,
                        &replica.database.name,
                        provider_stats(&provider, table_count),
                        started,
                        max_rows,
                        deadline,
                        sink.take(),
                    )
                };
                if lane == Lane::Inline || !repeatable {
                    return run().map(Attempt::Answered);
                }
                let key = SharedQueryKey::for_current_session(replica.load_id, sql, max_rows);
                run_shared(&key, deadline, started, run)
            }
            Statement::Explain { .. } => self
                .execute_explain(
                    &statement,
                    catalog,
                    &mut provider,
                    &replica.database.name,
                    table_count,
                    started,
                    deadline,
                )
                .map(Answer::Whole),
            _ => Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            )),
        };
        answer.map(Attempt::Answered)
    }

    /// Executes `sql` from the plan kept for it, when one is kept against
    /// the replica this database answers from now and under the session
    /// settings installed on this thread. `None` when nothing is kept, or
    /// the replica it was kept against is no longer current: the statement
    /// is then prepared from its text as if there were no cache.
    ///
    /// What an execution from a kept plan does is what the execution that
    /// prepared it did after preparing: the same classification of the same
    /// plan against the replica as it is now, the same admission, the same
    /// deadline and the same sharing of one execution between identical
    /// requests.
    #[allow(clippy::too_many_arguments)]
    fn execute_kept(
        &self,
        plans: &PlanCache<KeptSelect>,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
        sink: &mut Option<&mut dyn RowSink>,
        lane: Lane,
        in_place: Option<InPlace<'_>>,
        started: Instant,
    ) -> Option<Result<Attempt, QueryError>> {
        let cache_key = self.cache_key(database_id);
        let candidate = self.cache.peek(&cache_key)?;
        let key = SharedQueryKey::for_current_session(candidate.load_id, sql, max_rows);
        let kept = plans.get(&key)?;
        let table_less = kept.bounded_planning && kept.table_less;
        if lane == Lane::Inline && !table_less && in_place.is_none() {
            return Some(Ok(Attempt::Declined(InlineAnswer::NotBounded)));
        }
        // Kept against this load: the load has still to be the current one.
        let replica = self.still_current(database_id, &cache_key, &candidate)?;
        plans.used();
        crate::trace::mark("kept");
        Some(self.run_kept(
            &kept, &replica, &key, sql, max_rows, deadline, sink, lane, in_place, started,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_kept(
        &self,
        kept: &KeptSelect,
        replica: &LoadedReplica,
        key: &SharedQueryKey,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
        sink: &mut Option<&mut dyn RowSink>,
        lane: Lane,
        in_place: Option<InPlace<'_>>,
        started: Instant,
    ) -> Result<Attempt, QueryError> {
        let provider = build_provider(replica)?;
        // The class [`Self::classify`] gives this statement over this
        // replica: its shape is kept, its cost is the plan's over the
        // snapshots as they are now.
        let tiny = kept.bounded_planning && kept.bounded_admission && replica.is_tiny();
        let cost = if kept.bounded_planning && !tiny {
            provider.admission_cost(&kept.prepared.physical)
        } else {
            None
        };
        let short = tiny || QueryClass::from_cost(cost) == QueryClass::Short;
        let table_less = kept.bounded_planning && kept.table_less;
        // A small read: a short query that looks at few stored rows - a
        // lookup by a whole key, a scan of a small table or of a narrow key
        // range.
        let small_read = !table_less
            && short
            && (tiny
                || provider
                    .bounded_scan_rows(&kept.prepared.physical)
                    .is_some_and(|rows| rows <= SMALL_READ_ROWS));
        if lane == Lane::Inline && !table_less && !small_read {
            return Ok(Attempt::Declined(InlineAnswer::NotBounded));
        }
        if lane == Lane::Worker && small_read {
            SMALL_READ_SEEN.set(true);
        }
        crate::trace::mark("classified");
        crate::trace::label("class", if short { "short" } else { "general" });
        let class = if short {
            QueryClass::Short
        } else {
            QueryClass::General
        };
        let _permit = if lane == Lane::Inline {
            // Waiting for a slot is a worker's to do.
            let Some(permit) = self.admission.try_admit_class_now(class) else {
                return Ok(Attempt::Declined(InlineAnswer::NotNow));
            };
            permit
        } else {
            self.admission
                .try_admit_class(class)
                .ok_or(QueryError::Overloaded)?
        };
        crate::trace::mark("admitted");
        let deadline = hinted_deadline(kept.execution_time_hint, deadline);
        // Preparation installs the database name its statement resolves
        // unqualified names in; an execution reads it from the same place.
        pintail_sql::set_session_database_name(Some(&replica.database.name));
        let mut run = || {
            crate::trace::mark("prepared");
            self.run_prepared(
                kept.prepared.clone(),
                sql,
                &provider,
                &replica.database.name,
                provider_stats(&provider, replica.targets.len()),
                started,
                max_rows,
                deadline,
                sink.take(),
            )
        };
        if lane == Lane::Inline {
            if let (false, Some(in_place)) = (table_less, in_place) {
                let mut answer = None;
                in_place(&mut || answer = Some(run()));
                return answer
                    .unwrap_or_else(|| {
                        Err(QueryError::Internal(
                            "a small read was given nowhere to run".to_owned(),
                        ))
                    })
                    .map(Attempt::Answered);
            }
            return run().map(Attempt::Answered);
        }
        run_shared(key, deadline, started, run).map(Attempt::Answered)
    }

    /// Why transaction control is refused here: a local database has no
    /// transactions to control, and a replicated one has nothing to write
    /// inside them. The two answers differ because the reasons do, and a
    /// client reading "read-only" on a database it can write into would go
    /// looking for the wrong problem.
    fn transaction_control_rejection(&self, database_id: &str) -> QueryError {
        let local = MetaStore::open(&self.metadata_path)
            .and_then(|metadata| metadata.is_local_database(database_id));
        match local {
            Ok(true) => QueryError::Invalid(TRANSACTION_CONTROL_UNSUPPORTED.to_owned()),
            Ok(false) => QueryError::Invalid("Pintail's query surfaces are read-only".to_owned()),
            Err(error) => QueryError::Internal(error.to_string()),
        }
    }

    /// Executes one mutating statement against a LOCAL database.
    ///
    /// Replicated databases keep the read-only rejection: a row written
    /// into a mirrored table would be destroyed by the next resnapshot and
    /// has no binlog version it could legitimately claim
    /// (`docs/design/writable-mode.md`).
    fn execute_write(
        &self,
        database_id: &str,
        statement: &Statement,
        started: Instant,
    ) -> Result<QueryOutput, QueryError> {
        let metadata = MetaStore::open(&self.metadata_path)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        if !metadata
            .is_local_database(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?
        {
            // Also the answer for a database that does not exist: a write
            // must never be the thing that reports a missing database as
            // writable.
            return Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            ));
        }
        drop(metadata);

        let outcome =
            pintail_write::LocalDatabase::new(&self.data_dir, &self.metadata_path, database_id)
                .execute(statement)
                .map_err(|error| write_error(&error))?;
        // The catalog and the stored rows both changed; the next read must
        // not answer from a replica loaded before this statement.
        self.cache.invalidate(&self.cache_key(database_id));

        let affected = match outcome {
            pintail_write::WriteOutcome::TableCreated { .. } => 0,
            pintail_write::WriteOutcome::RowsInserted { rows, .. } => rows,
        };
        let stats = QueryStats {
            duration_ms: elapsed_ms(started),
            ..QueryStats::default()
        };
        Ok(QueryOutput {
            fields: Vec::new(),
            rows: ResultRows::default(),
            stats,
            truncated: false,
            affected: Some(affected),
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn execute_select(
        &self,
        statement: &Statement,
        sql: &str,
        catalog: &CatalogSnapshot,
        provider: &impl pintail_exec::ScanProvider,
        facts: &SourceFacts,
        database_name: &str,
        stats: QueryStats,
        started: Instant,
        max_rows: usize,
        deadline: Option<Instant>,
        optimize: bool,
        sink: Option<&mut dyn RowSink>,
    ) -> Result<Answer, QueryError> {
        let prepared =
            Self::prepare_select(statement, sql, catalog, facts, database_name, optimize)?;
        crate::trace::mark("prepared");
        self.run_prepared(
            prepared,
            sql,
            provider,
            database_name,
            stats,
            started,
            max_rows,
            deadline,
            sink,
        )
    }

    /// Binds and plans one SELECT, keeping everything its response needs
    /// from binding. Admission classification prepares the statement it
    /// costs, and execution runs that same preparation rather than a
    /// second one.
    fn prepare_select(
        statement: &Statement,
        sql: &str,
        catalog: &CatalogSnapshot,
        facts: &SourceFacts,
        database_name: &str,
        optimize: bool,
    ) -> Result<PreparedSelect, QueryError> {
        // Divisions by zero counted from here are this statement's own:
        // preparation folds its constants once.
        let _ = pintail_exec::take_session_division_warnings();
        pintail_sql::set_session_database_name(Some(database_name));
        // The source's indexes decide how MySQL reads a grouped TIMESTAMP.
        let bound = pintail_sql::with_source_indexes(catalog, &facts.indexes, || {
            Binder::new(catalog, Some(database_name))
                .with_source(sql)
                .bind(statement)
        })
        .map_err(|error| query_bind_error(&error))?;
        let result_nullability = source_result_nullability(&bound, catalog, facts);
        let wire_columns = crate::presentation::columns(&bound, catalog, facts);
        let result_collations = bound
            .projection
            .iter()
            .map(|projection| bound.result_collation(&projection.expr))
            .collect::<Vec<_>>();
        let group_concat = bound
            .projection
            .iter()
            .map(|projection| {
                let pintail_sql::BoundExprKind::Aggregate(slot) = &projection.expr.kind else {
                    return false;
                };
                slot.checked_sub(bound.group_by.len())
                    .and_then(|index| bound.aggregates.get(index))
                    .is_some_and(|aggregate| {
                        aggregate.function == pintail_sql::AggregateFunction::GroupConcat
                    })
            })
            .collect::<Vec<_>>();
        let wire_hints = bound
            .projection
            .iter()
            .map(|projection| {
                let pintail_sql::BoundExprKind::Scalar { function, .. } = &projection.expr.kind
                else {
                    return None;
                };
                match function {
                    pintail_sql::ScalarFunction::SecToTime
                    | pintail_sql::ScalarFunction::MakeTime => Some(WireTypeHint::Time),
                    pintail_sql::ScalarFunction::ConvertTz => Some(WireTypeHint::Datetime),
                    pintail_sql::ScalarFunction::JsonUnquote
                    | pintail_sql::ScalarFunction::JsonExtract { unquote: true } => {
                        Some(WireTypeHint::JsonText)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        // Carried from binding: the binder resolved one collation for this
        // query, and every operator below compares text with it.
        let collation = pintail_exec::collation::Collation::from_mysql_name(bound.text_collation)
            .unwrap_or_default();
        let logical = LogicalPlanner::plan(bound);
        let logical = if optimize {
            Optimizer::optimize(logical)
        } else {
            logical
        };
        let physical = PhysicalPlanner::plan(logical, collation)
            .map_err(|error| QueryError::Invalid(error.to_string()))?;
        Ok(PreparedSelect {
            physical,
            collation,
            wire_columns,
            result_nullability,
            result_collations,
            group_concat,
            wire_hints,
        })
    }

    /// Executes a prepared SELECT and collects its rows.
    #[allow(clippy::too_many_arguments)]
    fn run_prepared(
        &self,
        prepared: PreparedSelect,
        sql: &str,
        provider: &impl pintail_exec::ScanProvider,
        database_name: &str,
        mut stats: QueryStats,
        started: Instant,
        max_rows: usize,
        deadline: Option<Instant>,
        sink: Option<&mut dyn RowSink>,
    ) -> Result<Answer, QueryError> {
        let PreparedSelect {
            physical,
            collation,
            wire_columns,
            result_nullability,
            result_collations,
            group_concat,
            wire_hints,
        } = prepared;
        let mut execution = Execution::start_with_deadline(
            physical,
            provider,
            self.memory_limit,
            deadline,
            collation,
        )
        .map_err(query_execution_error)?;
        crate::trace::mark("started");
        let fields = execution
            .output_fields()
            .iter()
            .enumerate()
            .map(|(index, field)| QueryField {
                wire_column: wire_columns.get(index).cloned(),
                name: field.name.clone(),
                data_type: field.data_type,
                nullable: result_nullability
                    .get(index)
                    .copied()
                    .flatten()
                    .unwrap_or(field.nullable),
                collation: result_collations.get(index).cloned().flatten(),
                group_concat: group_concat.get(index).copied().unwrap_or(false),
                geometry: field.geometry,
                timestamp: field.timestamp,
                wire_hint: wire_hints.get(index).copied().flatten(),
            })
            .collect::<Vec<_>>();
        let collected = match collect_rows(&mut execution, max_rows, &fields, sink) {
            Ok(collected) => collected,
            Err(error) => {
                // The failed query is the one whose operator tree matters
                // most: a memory ceiling hit names only the operator that
                // asked last, and the peaks below name the ones holding it.
                if let Some(profile) = execution.profile() {
                    let shown: String = sql.trim().chars().take(160).collect();
                    pintail_log::log_info!(
                        "pintail profile db={database_name} failed=\"{error}\" sql={shown:?}\n{}",
                        profile.render().trim_end()
                    );
                }
                return Err(error);
            }
        };
        let (row_count, batches) = match &collected {
            Collected::Whole { rows, batches, .. } => (rows.len(), *batches),
            Collected::Streamed { rows, batches } => (*rows, *batches),
        };
        crate::trace::mark("collected");
        crate::trace::label("rows", row_count);
        // Development profiling (PINTAIL_PROFILE): one block per query with
        // every operator's time, rows and peak reservation.
        if let Some(profile) = execution.profile() {
            let statement = sql.trim();
            let shown: String = statement.chars().take(160).collect();
            pintail_log::log_info!(
                "pintail profile db={database_name} rows={row_count} sql={shown:?}\n{}",
                profile.render().trim_end()
            );
        }
        stats.duration_ms = elapsed_ms(started);
        stats.rows = row_count;
        stats.batches = batches;
        Ok(match collected {
            Collected::Whole {
                rows, truncated, ..
            } => Answer::Whole(QueryOutput {
                fields,
                rows,
                stats,
                truncated,
                affected: None,
            }),
            Collected::Streamed { rows, .. } => Answer::Streamed { rows, stats },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_explain(
        &self,
        statement: &Statement,
        catalog: &CatalogSnapshot,
        provider: &mut SnapshotScanProvider<'_>,
        database_name: &str,
        table_count: usize,
        started: Instant,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        let plan = explain_statement(statement, catalog, Some(database_name)).or_else(|_| {
            explain_analyze_statement_with_deadline(
                statement,
                catalog,
                Some(database_name),
                provider,
                self.memory_limit,
                deadline,
            )
        });
        let plan = plan.map_err(query_explain_error)?;
        let mut stats = provider_stats(provider, table_count);
        stats.duration_ms = elapsed_ms(started);
        stats.rows = 1;
        Ok(QueryOutput {
            fields: vec![QueryField {
                wire_column: None,
                name: "plan".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: false,
                collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
                geometry: false,
                timestamp: false,
                wire_hint: None,
            }],
            rows: vec![vec![Value::Utf8(plan)]].into(),
            stats,
            truncated: false,
            affected: None,
        })
    }

    /// Loads `database_id`'s replica, reusing from `previous` every table
    /// whose source definition, schema version and files are unchanged.
    /// Returns the replica and how many table snapshots it had to open.
    fn load_replica(
        &self,
        database_id: &str,
        previous: Option<(&LoadedReplica, &ReplicaStamp)>,
        current: &ReplicaStamp,
    ) -> Result<(LoadedReplica, usize), QueryError> {
        let metadata = MetaStore::open(&self.metadata_path)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let database = metadata
            .database(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?
            .ok_or(QueryError::DatabaseNotFound)?;
        let report: ProbeReport = serde_json::from_str(
            database
                .probe_json
                .as_deref()
                .ok_or_else(|| QueryError::NotReady("database has not been probed".to_owned()))?,
        )
        .map_err(|error| QueryError::Internal(error.to_string()))?;
        let tables = metadata
            .tables(database_id)
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let table_records = tables
            .iter()
            .map(|table| (table.name.to_ascii_lowercase(), table))
            .collect::<BTreeMap<_, _>>();
        let root = self.tables_root(database_id);
        let mut opened = 0;
        let targets = report
            .tables
            .into_iter()
            .filter(|source| table_records.contains_key(&source.name.to_ascii_lowercase()))
            .map(|mut source| {
                let history = metadata
                    .schema_history(database_id, &source.name)
                    .map_err(|error| QueryError::Internal(error.to_string()))?;
                let version = history.last().map_or(1, |record| record.version);
                if let Some(record) = history.last() {
                    source.columns = serde_json::from_str(&record.columns_json)
                        .map_err(|error| QueryError::Internal(error.to_string()))?;
                }
                let record = table_records.get(&source.name.to_ascii_lowercase());
                let ready = record.is_none_or(|record| table_copy_is_complete(record));
                if let (false, Some(record)) = (ready, record) {
                    pintail_log::log_info!(
                        "replica.table_not_ready db={database_id} table={} state={} copy_complete={}",
                        source.name,
                        record.state,
                        record.copy_complete
                    );
                }
                let directory = table_directory(&root, &source.name);
                let directory_name = directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let prior = previous.and_then(|(replica, stamp)| {
                    replica
                        .targets
                        .iter()
                        .find(|target| target.source.name.eq_ignore_ascii_case(&source.name))
                        .filter(|target| target.unreadable.is_none())
                        .map(|target| (target, stamp))
                });
                // Reuse needs all three unchanged: the probe-derived
                // definition (a reprobe can change columns without a new
                // schema version), the schema version, and the table's own
                // files. Everything else in the replica is rebuilt from the
                // metadata store, which is cheap.
                let reusable = prior.and_then(|(target, stamp)| {
                    (target.version == version
                        && target.source == source
                        && stamp.tables.get(&directory_name) == current.tables.get(&directory_name))
                    .then(|| target.snapshot.clone())
                });
                if let Some(snapshot) = reusable {
                    return Ok(ReaderTarget::new(source, version, snapshot, ready, None));
                }
                opened += 1;
                open_target(
                    database_id,
                    directory,
                    source,
                    version,
                    ready,
                    prior.map(|(target, _)| target),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            LoadedReplica {
                server_version: report.server.version,
                load_id: NEXT_REPLICA_LOAD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                database,
                tables,
                targets,
                catalog: OnceLock::new(),
                facts: OnceLock::new(),
            },
            opened,
        ))
    }
}

/// The longest statement text kept prepared: the text is part of the key
/// every execution builds and compares.
const KEPT_STATEMENT_BYTES: usize = 8192;

/// Where a small read runs on the thread that received it: given the read,
/// it makes blocking on this thread safe and runs the read, once. See
/// [`ReplicaEngine::execute_answer_in_place`].
pub type InPlace<'a> = &'a dyn Fn(&mut dyn FnMut());

/// The most stored rows a read may have to look at and still run on the
/// thread that received it ([`ReplicaEngine::execute_answer_in_place`]).
pub const SMALL_READ_ROWS: u64 = 1024;

thread_local! {
    /// Whether the statement a worker last ran on this thread from a kept
    /// plan was a small read.
    static SMALL_READ_SEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the last statement this thread executed as a worker ran from a
/// kept plan and was a small read - one that
/// [`ReplicaEngine::execute_answer_in_place`] would have run - and forgets
/// it. A connection uses it to learn which of its statements to offer
/// there.
#[must_use]
pub fn take_small_read_seen() -> bool {
    SMALL_READ_SEEN.replace(false)
}

/// A statement's deadline under its own `MAX_EXECUTION_TIME` hint.
///
/// `/*+ MAX_EXECUTION_TIME(ms) */` is scoped to the statement and tightens
/// whatever the session already allows - never loosens it, so a hint cannot
/// be used to escape an administrator's ceiling. A hint of 0 means "no
/// ceiling" in `MySQL` and simply leaves the session's in force.
fn hinted_deadline(hint: Option<u64>, deadline: Option<Instant>) -> Option<Instant> {
    match hint {
        Some(milliseconds) if milliseconds > 0 => Instant::now()
            .checked_add(Duration::from_millis(milliseconds))
            .map(|hinted| deadline.map_or(hinted, |held| held.min(hinted)))
            .or(deadline),
        _ => deadline,
    }
}

/// Runs a repeatable statement as one of however many identical requests
/// are asking it of the same snapshot at the same time: several clients
/// asking the same question at once is one question.
fn run_shared(
    key: &SharedQueryKey,
    deadline: Option<Instant>,
    started: Instant,
    run: impl FnOnce() -> Result<Answer, QueryError>,
) -> Result<Answer, QueryError> {
    match shared_queries().join(key, deadline) {
        Join::Alone => {
            crate::trace::label("shared", "alone");
            run()
        }
        Join::Followed(output) => {
            crate::trace::label("shared", "followed");
            Ok(Answer::Whole(followed_output(&output, started)))
        }
        Join::Lead(leader) => {
            crate::trace::label("shared", "lead");
            let result = run();
            // A streamed result went to one reader and is not held:
            // whoever waits executes on their own.
            if let Ok(Answer::Whole(output)) = &result {
                leader.succeeded(output);
            }
            result
        }
    }
}

/// Opens one table's snapshot under `source` at `version`.
///
/// A store that refuses that definition is tried under `prior`'s, the one
/// it was last read under: a source that changed shape without a
/// transition replication could apply leaves the store holding rows
/// written under the previous definition, and the table keeps reading them
/// (differing from its source in content, the documented consequence)
/// rather than becoming unreadable. A store that opens under neither
/// refuses its own reads, with the reason, while the rest of its database
/// answers.
fn open_target(
    database_id: &str,
    directory: PathBuf,
    source: SourceTable,
    version: u32,
    ready: bool,
    prior: Option<&ReaderTarget>,
) -> Result<ReaderTarget, QueryError> {
    let schema = source
        .table_schema_with_version(version)
        .map_err(|error| QueryError::Internal(error.to_string()))?;
    let error = match TableSnapshot::open(&directory, schema.clone()) {
        Ok(snapshot) => {
            return Ok(ReaderTarget::new(source, version, snapshot, ready, None));
        }
        Err(error) => error,
    };
    let held = prior.and_then(|target| {
        let schema = target
            .source
            .table_schema_with_version(target.version)
            .ok()?;
        Some((target, TableSnapshot::open(&directory, schema).ok()?))
    });
    if let Some((target, snapshot)) = held {
        pintail_log::log_info!(
            "replica.table_definition_held db={database_id} table={}: {error}",
            source.name
        );
        return Ok(ReaderTarget::new(
            target.source.clone(),
            target.version,
            snapshot,
            ready,
            None,
        ));
    }
    pintail_log::log_info!(
        "replica.table_unreadable db={database_id} table={}: {error}",
        source.name
    );
    Ok(ReaderTarget::new(
        source,
        version,
        TableSnapshot::empty(directory, schema),
        ready,
        Some(error.to_string()),
    ))
}

/// Statements whose only purpose is a multi-statement transaction boundary.
///
/// `SET TRANSACTION ISOLATION LEVEL` is deliberately absent: it describes how
/// a transaction would observe other writers, which an autocommitted
/// statement satisfies under every level, so accepting it promises nothing.
fn is_transaction_control(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
    )
}

/// A result as collection left it.
enum Collected {
    /// Every row, held; `truncated` when a row lay beyond the limit.
    Whole {
        rows: ResultRows,
        batches: usize,
        truncated: bool,
    },
    /// The rows went to the sink.
    Streamed { rows: usize, batches: usize },
}

/// Collects the result as the batches execution produces, up to `max_rows`
/// rows.
///
/// A held result keeps the rows under the limit from the batch that
/// crosses it and reports itself truncated. With a sink, a result that
/// grows past [`STREAM_AFTER_ROWS`] starts streaming: the sink gets the
/// fields and the rows so far, then each batch as it comes, and a streamed
/// result that crosses the limit ends with the ceiling's error after the
/// rows under it.
fn collect_rows(
    execution: &mut Execution,
    max_rows: usize,
    fields: &[QueryField],
    mut sink: Option<&mut dyn RowSink>,
) -> Result<Collected, QueryError> {
    let mut rows = ResultRows::default();
    let mut sent = 0_usize;
    let mut streaming = false;
    let mut batches = 0;
    while let Some(mut batch) = execution.next_batch().map_err(query_execution_error)? {
        batches += 1;
        let room = max_rows - sent - rows.len();
        let over = batch.visible_row_count() > room;
        if over {
            let mut selection = batch.selection().clone();
            for row in batch.selection().selected_rows().skip(room) {
                selection
                    .set(row, false)
                    .map_err(|error| QueryError::Internal(error.to_string()))?;
            }
            batch
                .set_selection(selection)
                .map_err(|error| QueryError::Internal(error.to_string()))?;
        }
        rows.push_batch(batch);
        if over && !streaming {
            return Ok(Collected::Whole {
                rows,
                batches,
                truncated: true,
            });
        }
        let Some(sink) = sink.as_deref_mut() else {
            continue;
        };
        if !streaming && rows.len() > STREAM_AFTER_ROWS {
            if !sink.begin(fields) {
                return Err(QueryError::Interrupted);
            }
            streaming = true;
        }
        if streaming && !rows.is_empty() {
            sent += rows.len();
            if !sink.rows(std::mem::take(&mut rows)) {
                return Err(QueryError::Interrupted);
            }
        }
        if over {
            return Err(result_ceiling_error(max_rows));
        }
    }
    Ok(if streaming {
        Collected::Streamed {
            rows: sent,
            batches,
        }
    } else {
        Collected::Whole {
            rows,
            batches,
            truncated: false,
        }
    })
}

/// The refusal of a result with more rows than the wire's ceiling allows.
pub(crate) fn result_ceiling_error(ceiling: usize) -> QueryError {
    QueryError::Invalid(format!(
        "the result has more than {ceiling} rows, the ceiling PINTAIL_MAX_RESULT_ROWS sets; \
         narrow it with a filter or LIMIT, or raise the ceiling"
    ))
}

/// Whether a table's store holds everything its source had when the copy
/// ran. A snapshot in progress, a first copy that failed part-way, and a
/// copy a restart interrupted (flagged for resync with the copy still
/// owed) leave a store the engine must not answer from. A table flagged
/// for a resync it has not started, a table under replication, and a local
/// table all keep serving: their stores are complete, if possibly behind.
fn table_copy_is_complete(record: &pintail_meta::TableRecord) -> bool {
    record.copy_complete
        || (!record.copy_pending
            && !matches!(record.state.as_str(), "snapshotting" | "error" | "pending"))
}

fn query_execution_error(error: ExecError) -> QueryError {
    match error {
        ExecError::QueryTimedOut | ExecError::QueryCancelled => QueryError::Interrupted,
        ExecError::TableNotReady { .. } | ExecError::TableUnreadable { .. } => {
            QueryError::NotReady(error.to_string())
        }
        // MySQL answers a row-wise numeric overflow with 1690/22003, not
        // an internal error - clients branch on the code.
        ExecError::NumericOverflow | ExecError::OutOfRange(_) => QueryError::Rejected {
            rejection: SqlRejection::OutOfRange,
            message: error.to_string(),
        },
        // MySQL's own texts: clients and ORMs match on them.
        ExecError::ScalarSubqueryRows { .. } => QueryError::Rejected {
            rejection: SqlRejection::SubqueryRows,
            message: "Subquery returns more than 1 row".to_owned(),
        },
        ExecError::CharacterConversion(_) => QueryError::Rejected {
            rejection: SqlRejection::CharacterConversion,
            message: error.to_string(),
        },
        ExecError::WrongArguments(_) => QueryError::Rejected {
            rejection: SqlRejection::WrongArguments,
            message: error.to_string(),
        },
        ExecError::BinaryBitwiseLength => QueryError::Rejected {
            rejection: SqlRejection::BinaryBitwiseLength,
            message: error.to_string(),
        },
        ExecError::BinaryBitwiseAggregateWidth => QueryError::Rejected {
            rejection: SqlRejection::BinaryBitwiseAggregateWidth,
            message: error.to_string(),
        },
        ExecError::InvalidJsonPath { .. } => QueryError::Rejected {
            rejection: SqlRejection::InvalidJsonPath,
            message: error.to_string(),
        },
        ExecError::Spatial { kind, message } => QueryError::Rejected {
            rejection: SqlRejection::Spatial(kind),
            message,
        },
        error => QueryError::Internal(error.to_string()),
    }
}

/// Classifies binder rejections into the `MySQL` error classes the wire
/// protocol distinguishes. Anything unclassified keeps today's behaviour
/// (1064 via [`QueryError::Invalid`]).
fn query_bind_error(error: &pintail_sql::BindError) -> QueryError {
    use pintail_sql::BindError;
    let rejection = match &error {
        BindError::UnknownDatabase(_) => SqlRejection::UnknownDatabase,
        BindError::UnknownTable { .. } => SqlRejection::UnknownTable,
        // An unknown relation qualifier surfaces in MySQL as an unknown
        // column ("Unknown column 'u.x' in 'field list'").
        BindError::UnknownColumn(_) | BindError::UnknownRelation(_) => SqlRejection::UnknownColumn,
        BindError::AmbiguousColumn(_)
        | BindError::AmbiguousRelation(_)
        | BindError::AmbiguousOrderBy(_) => SqlRejection::AmbiguousColumn,
        BindError::UngroupedColumn(_) | BindError::UngroupedSubquery => {
            SqlRejection::UngroupedColumn
        }
        BindError::GroupFunctionMisplaced(_) => SqlRejection::GroupFunctionMisplaced,
        BindError::ParameterCount(_) => SqlRejection::ParameterCount,
        BindError::IllegalCollationMix { pair: true, .. } => SqlRejection::CollationMixOfTwo,
        BindError::IllegalCollationMix { pair: false, .. } => SqlRejection::CollationMixOfSeveral,
        _ => return QueryError::Invalid(error.to_string()),
    };
    QueryError::Rejected {
        rejection,
        message: error.to_string(),
    }
}

fn query_explain_error(error: ExplainError) -> QueryError {
    match error {
        ExplainError::Exec(ExecError::QueryTimedOut | ExecError::QueryCancelled) => {
            QueryError::Interrupted
        }
        ExplainError::Exec(ExecError::TableNotReady { .. } | ExecError::TableUnreadable { .. }) => {
            QueryError::NotReady(error.to_string())
        }
        error => QueryError::Invalid(error.to_string()),
    }
}

fn metadata_output(result: pintail_sql::MetadataResult, started: Instant) -> QueryOutput {
    QueryOutput {
        fields: result
            .fields
            .into_iter()
            .map(|field| QueryField {
                wire_column: None,
                name: field.name,
                data_type: Some(field.data_type),
                nullable: field.nullable,
                collation: (field.data_type == DataType::Utf8)
                    .then(|| DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
                geometry: false,
                timestamp: false,
                wire_hint: None,
            })
            .collect(),
        stats: QueryStats {
            duration_ms: elapsed_ms(started),
            rows: result.rows.len(),
            ..QueryStats::default()
        },
        rows: result.rows.into(),
        truncated: false,
        affected: None,
    }
}

/// Probe-derived facts the catalog schema does not carry, for
/// `information_schema.columns` fidelity.
fn column_facts(replica: &LoadedReplica) -> SourceFacts {
    let mut facts = SourceFacts {
        server_version: Some(replica.server_version.clone()),
        ..SourceFacts::default()
    };
    for target in &replica.targets {
        let source = &target.source;
        for column in &source.columns {
            facts.columns.push(ColumnFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                column: column.name.clone(),
                default_value: column.default_value.clone(),
                default_generated: column.default_generated,
                nullable: Some(column.nullable),
                auto_increment: column.auto_increment,
                generated_stored: column.generated_stored,
                generation_expression: column.generation_expression.clone(),
                extra: column.extra.clone(),
                unique_single: source
                    .unique_keys
                    .iter()
                    .any(|key| key.len() == 1 && key[0].eq_ignore_ascii_case(&column.name)),
                character_set: column.character_set.clone(),
                collation: column.collation.clone(),
                mysql_data_type: Some(column.mysql_data_type.clone()),
                mysql_column_type: Some(column.mysql_column_type.clone()),
            });
        }
        let chosen_unique = matches!(source.key.mode, pintail_types::KeyMode::Unique);
        if chosen_unique {
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                index_name: source
                    .key
                    .index_name
                    .clone()
                    .unwrap_or_else(|| "unique_key".to_owned()),
                unique: true,
                columns: source.key.columns.clone(),
            });
        }
        for key in &source.foreign_keys {
            facts.foreign_keys.push(pintail_sql::ForeignKeyFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                name: key.name.clone(),
                columns: key.columns.clone(),
                referenced_table: key.referenced_table.clone(),
                referenced_columns: key.referenced_columns.clone(),
                unique_constraint_name: key.unique_constraint_name.clone(),
                update_rule: key.update_rule.clone(),
                delete_rule: key.delete_rule.clone(),
            });
        }
        for index in &source.secondary_indexes {
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                index_name: index.name.clone(),
                unique: false,
                columns: index.columns.clone(),
            });
        }
        for (position, unique) in source.unique_keys.iter().enumerate() {
            let is_chosen = chosen_unique
                && unique.len() == source.key.columns.len()
                && unique
                    .iter()
                    .zip(&source.key.columns)
                    .all(|(left, right)| left.eq_ignore_ascii_case(right));
            if is_chosen {
                continue;
            }
            facts.indexes.push(IndexFacts {
                database: replica.database.name.clone(),
                table: source.name.clone(),
                // The probe keeps unique column sets but not their index
                // names; a synthesized stable name beats hiding the key.
                index_name: format!("unique_{}", position + 1),
                unique: true,
                columns: unique.clone(),
            });
        }
    }
    facts
}

/// Restores source-declared nullability for direct result columns without
/// changing the executor's deliberately permissive physical schema. The
/// physical carrier must allow normalized invalid temporals to become NULL;
/// `MySQL` result metadata still describes a direct source column by its source
/// declaration. Outer-join extension takes precedence over that declaration.
fn collect_null_extended_columns(
    query: &BoundQuery,
    inherited: bool,
    columns: &mut BTreeSet<(DatabaseId, TableId, u32)>,
) {
    for source in &query.from {
        if inherited {
            columns.extend(
                source
                    .base
                    .columns
                    .iter()
                    .map(|column| (column.database_id, column.table_id, column.column_id)),
            );
        }
        if let Some(input) = &source.base.input {
            collect_null_extended_columns(input, inherited, columns);
        }
        for join in &source.joins {
            let right_extended =
                inherited || matches!(join.kind, BoundJoinKind::Left | BoundJoinKind::Scalar);
            if right_extended {
                columns.extend(
                    join.table
                        .columns
                        .iter()
                        .map(|column| (column.database_id, column.table_id, column.column_id)),
                );
            }
            if let Some(input) = &join.table.input {
                collect_null_extended_columns(input, right_extended, columns);
            }
        }
    }
}

fn source_result_nullability(
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    facts: &SourceFacts,
) -> Vec<Option<bool>> {
    if query.union_distinct || !query.union_all.is_empty() || !query.set_ops.is_empty() {
        return vec![None; query.projection.len()];
    }
    let mut null_extended = BTreeSet::new();
    collect_null_extended_columns(query, false, &mut null_extended);

    query
        .projection
        .iter()
        .map(|projection| {
            let BoundExprKind::Column(column) = &projection.expr.kind else {
                return None;
            };
            if null_extended.contains(&(column.database_id, column.table_id, column.column_id)) {
                return Some(true);
            }
            let database = catalog.database_by_id(column.database_id)?;
            let table = database.table_by_id(column.table_id)?;
            facts
                .columns
                .iter()
                .find(|fact| {
                    fact.database.eq_ignore_ascii_case(database.name())
                        && fact.table.eq_ignore_ascii_case(table.name())
                        && fact.column.eq_ignore_ascii_case(&column.name)
                })
                .and_then(|fact| fact.nullable)
        })
        .collect()
}

/// The cached replica, but only when `stamp` - taken from disk - says
/// nothing has moved since it was loaded, and only when the copy returned
/// is the one the caller's checks were made against.
///
/// A short query skips `load_replica_cached`, so this is the ONLY place
/// its snapshot is proved current. Judging it against the stamp the cache
/// already holds would prove nothing: that stamp was recorded when the
/// replica was loaded, and the commit this query must see may have landed
/// since.
/// Whether `text` holds `needle` (given in lower case), compared without
/// regard to ASCII case and without copying `text`.
fn contains_ignore_ascii_case(text: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    text.as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn revalidated(
    cache: &ReplicaCache<LoadedReplica>,
    key: &CacheKey,
    stamp: &ReplicaStamp,
    replica: &Arc<LoadedReplica>,
) -> Option<Arc<LoadedReplica>> {
    match cache.lookup(key, stamp) {
        Lookup::Hit(current) if Arc::ptr_eq(replica, &current) => Some(current),
        _ => None,
    }
}

fn build_catalog(replica: &LoadedReplica) -> Result<CatalogSnapshot, QueryError> {
    let row_counts = replica
        .tables
        .iter()
        .map(|table| (table.name.to_ascii_lowercase(), table.rows_synced))
        .collect::<BTreeMap<_, _>>();
    // A source with case-sensitive table names can hold `T1` and `t1`, which
    // names here cannot tell apart. Both stay out of the catalog: a query
    // naming either is refused as an unknown table rather than answered from
    // the other, and every other table in the database stays queryable.
    let mut spellings = BTreeMap::<String, usize>::new();
    for target in &replica.targets {
        *spellings
            .entry(target.source.name.to_ascii_lowercase())
            .or_default() += 1;
    }
    let entries = replica
        .targets
        .iter()
        .enumerate()
        .filter(|(_, target)| spellings[&target.source.name.to_ascii_lowercase()] == 1)
        .map(|(index, target)| {
            let id = table_id(index)?;
            let rows = row_counts
                .get(&target.source.name.to_ascii_lowercase())
                .copied()
                .or(target.source.estimated_rows)
                .unwrap_or(0);
            // rows_synced advances with the snapshot, not with CDC, so it
            // is an estimate: join-size guards may use it, but COUNT(*)
            // must execute (the settled memo and segment SMAs keep that
            // fast) — an exact claim here served stale counts during
            // replication (found by the e2e control-plane gate).
            let entry = TableEntry::new(
                id,
                &target.source.name,
                target.snapshot.schema().clone(),
                TableStatistics::with_estimated_row_count(rows),
            )
            .map_err(|error| QueryError::Internal(error.to_string()))?
            // Assembled on first use - only a join asks - and then shared
            // by every query this load serves.
            .with_column_statistics({
                let snapshot = target.snapshot.clone();
                pintail_catalog::LazyColumnStatistics::new(move || snapshot.column_statistics())
            });
            let key_columns = target.source.key_column_ids();
            if key_columns.is_empty() {
                Ok(entry)
            } else {
                entry
                    .with_key_columns(key_columns)
                    .map_err(|error| QueryError::Internal(error.to_string()))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let database = DatabaseEntry::new(DatabaseId::new(1), &replica.database.name, entries)
        .map_err(|error| QueryError::Internal(error.to_string()))?;
    CatalogSnapshot::new([database]).map_err(|error| QueryError::Internal(error.to_string()))
}

fn build_provider(replica: &LoadedReplica) -> Result<SnapshotScanProvider<'_>, QueryError> {
    let database_id = DatabaseId::new(1);
    let indexed = replica
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| Ok((database_id, table_id(index)?, &target.snapshot)))
        .collect::<Result<Vec<_>, QueryError>>()?;
    let mut provider = SnapshotScanProvider::new(indexed)
        .map_err(|error| QueryError::Internal(error.to_string()))?;
    for (index, target) in replica.targets.iter().enumerate() {
        if let Some(reason) = &target.unreadable {
            provider.mark_unreadable(
                database_id,
                table_id(index)?,
                target.source.name.clone(),
                reason.clone(),
            );
        } else if !target.ready {
            provider.mark_not_ready(database_id, table_id(index)?, target.source.name.clone());
        }
        let storage_key = target.source.key_column_ids();
        let unique_keys = target
            .source
            .unique_keys
            .iter()
            .map(|key| {
                key.iter()
                    .filter_map(|name| {
                        target
                            .source
                            .columns
                            .iter()
                            .find(|column| column.name.eq_ignore_ascii_case(name))
                            .map(|column| column.id)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|key| !key.is_empty())
            .filter(|key| *key != storage_key)
            .collect::<Vec<_>>();
        if !unique_keys.is_empty() && unique_collisions_possible(&replica.database, &target.source)
        {
            provider
                .enable_unique_visibility_policy(database_id, table_id(index)?, unique_keys)
                .map_err(|error| QueryError::Internal(error.to_string()))?;
        }
    }
    Ok(provider)
}

/// Whether two live rows of this table can share a secondary UNIQUE value.
///
/// The read policy that hides the lower-versioned side of such a collision
/// has to see every projected row to find one, so the scan behind it holds
/// the whole projection in memory, bounded by the query budget. That is
/// affordable only where a collision can arise at all: polling, where a hard
/// delete stays visible until reconciliation and the source may reuse the
/// unique value first, and CDC tables flagged for periodic reconciliation.
/// A native CDC table replays the delete before the reinsert, so it streams;
/// applying the policy there made a one-row COUNT over a large mirrored
/// table fail with the query memory limit.
fn unique_collisions_possible(database: &DatabaseRecord, source: &SourceTable) -> bool {
    let mode = database
        .effective_mode
        .as_deref()
        .unwrap_or(database.mode.as_str());
    mode != "cdc" || source.requires_reconciliation
}

fn provider_stats(provider: &SnapshotScanProvider<'_>, table_count: usize) -> QueryStats {
    let mut output = QueryStats::default();
    for index in 0..table_count {
        let Ok(table_id) = table_id(index) else {
            break;
        };
        let Some(stats) = provider.scan_stats(DatabaseId::new(1), table_id) else {
            continue;
        };
        output.segments_read += stats.segments_read;
        output.segments_pruned += stats.segments_pruned;
        output.blocks_read += stats.blocks_read;
        output.blocks_pruned += stats.blocks_pruned;
        output.blocks_decoded += stats.blocks_decoded;
    }
    output
}

fn table_id(index: usize) -> Result<TableId, QueryError> {
    let id = u64::try_from(index)
        .ok()
        .and_then(|index| index.checked_add(1))
        .ok_or_else(|| QueryError::Internal("table catalog ID overflow".to_owned()))?;
    Ok(TableId::new(id))
}

/// Returns the stable on-disk directory for one source table.
///
/// Delegates to the single definition in `pintail_store`: readers and
/// writers that disagree here address different directories silently.
#[must_use]
pub fn table_directory(root: &Path, table: &str) -> PathBuf {
    pintail_store::table_directory(root, table)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Maps a write rejection onto the wire's rejection, preserving the `MySQL`
/// error number and SQLSTATE the client branches on.
fn write_error(error: &pintail_write::WriteError) -> QueryError {
    let message = error.to_string();
    let rejection = match error.mysql_code() {
        1050 => SqlRejection::TableExists,
        1062 => SqlRejection::DuplicateKey,
        1048 => SqlRejection::NotNull,
        1406 => SqlRejection::DataTooLong,
        1146 => SqlRejection::UnknownTable,
        1054 => SqlRejection::UnknownColumn,
        // Everything else is a statement Pintail understood and refused,
        // which is 1064 on the wire like any other unsupported statement.
        _ => return QueryError::Invalid(message),
    };
    QueryError::Rejected { rejection, message }
}

/// One execution's answer, presented to a request that waited for it.
///
/// The physical counters stay as they were measured: they describe how
/// these rows were produced, and they were produced once. The duration is
/// this request's own, because what it waited is not what the leader
/// spent, and a client reading its own query time should see its own.
fn followed_output(shared: &QueryOutput, started: Instant) -> QueryOutput {
    let mut output = shared.clone();
    output.stats.duration_ms = elapsed_ms(started);
    output
}

#[cfg(test)]
#[path = "engine_plan_cache_tests.rs"]
mod plan_cache_tests;

#[cfg(test)]
mod admission_tests {
    use super::*;
    use pintail_write::LocalDatabase;

    /// Every authenticated request writes to the metadata store - an audit
    /// record, an API-key touch - and the store's file stamp moved with each,
    /// so a warm replica was judged stale on every request and an otherwise
    /// eligible short query fell back to general admission. The stamp's
    /// metadata half is now a signature of the rows a load reads: the files
    /// move, the signature does not, and the replica stays warm. Schema,
    /// table-state and mode changes still move it.
    #[test]
    fn bookkeeping_metadata_writes_keep_the_replica_warm_and_semantic_ones_do_not() {
        const NOW: &str = "2026-09-06T00:00:00Z";
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let mut meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", NOW).unwrap();
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a VALUES (1)",
        ] {
            writer.execute(&parse_statement(sql).unwrap()).unwrap();
        }
        let engine = ReplicaEngine::new(directory.path(), &metadata_path);
        engine
            .execute("db", "SELECT id FROM a WHERE id = 1", 10)
            .unwrap();
        let warm = engine.replica_stamp("db");
        assert!(matches!(
            engine.cache.lookup(&engine.cache_key("db"), &warm),
            Lookup::Hit(_)
        ));

        meta.set_setting("probe.cadence", "5").unwrap();
        meta.create_workspace("ws", "Workspace", "ws", NOW).unwrap();
        meta.record_audit_event(&pintail_meta::NewAuditEvent {
            id: "evt-1",
            workspace_id: "ws",
            actor_type: "user",
            actor_id: "usr_1",
            actor_label: "operator",
            action: "query.execute",
            target_type: Some("database"),
            target_id: Some("db"),
            detail_json: None,
            created_at: NOW,
            client_ip: None,
        })
        .unwrap();
        let after_bookkeeping = engine.replica_stamp("db");
        assert_ne!(
            warm.metadata.files, after_bookkeeping.metadata.files,
            "the metadata store's files moved"
        );
        assert_eq!(warm, after_bookkeeping, "the stamp did not");
        assert!(matches!(
            engine
                .cache
                .lookup(&engine.cache_key("db"), &after_bookkeeping),
            Lookup::Hit(_)
        ));

        meta.record_schema_history(
            "db",
            "a",
            2,
            Some("ALTER TABLE a ADD COLUMN note TEXT"),
            r#"[{"id":1,"name":"id"},{"id":2,"name":"note"}]"#,
            NOW,
        )
        .unwrap();
        let evolved = engine.replica_stamp("db");
        assert_ne!(
            after_bookkeeping, evolved,
            "a schema generation is a change"
        );
        assert!(matches!(
            engine.cache.lookup(&engine.cache_key("db"), &evolved),
            Lookup::Stale(..)
        ));
        meta.set_database_mode("db", "paused", NOW).unwrap();
        assert_ne!(
            evolved,
            engine.replica_stamp("db"),
            "a mode change is a change"
        );
    }

    #[test]
    fn a_table_whose_copy_is_running_is_not_ready_rather_than_empty() {
        const NOW: &str = "2026-09-07T00:00:00Z";
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", NOW).unwrap();
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a VALUES (1), (2)",
            "CREATE TABLE b (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO b VALUES (7)",
        ] {
            writer.execute(&parse_statement(sql).unwrap()).unwrap();
        }
        let engine = ReplicaEngine::new(directory.path(), &metadata_path);
        let served = engine.execute("db", "SELECT COUNT(*) FROM a", 10).unwrap();
        assert_eq!(served.rows, vec![vec![Value::UInt64(2)]]);

        // The copy of `a` starts over: its store is no longer an answer.
        meta.begin_table_resnapshot("db", "a").unwrap();
        let refused = engine.execute("db", "SELECT COUNT(*) FROM a", 10);
        let Err(QueryError::NotReady(message)) = refused else {
            panic!("a table mid-copy must be refused, got {refused:?}");
        };
        assert!(message.contains("table a"), "{message}");
        assert!(
            matches!(
                engine.execute("db", "EXPLAIN ANALYZE SELECT id FROM a", 10),
                Err(QueryError::NotReady(_))
            ),
            "profiling opens the same scan"
        );
        // Other tables of the database still answer, and so does metadata.
        assert_eq!(
            engine.execute("db", "SELECT id FROM b", 10).unwrap().rows,
            vec![vec![Value::UInt64(7)]]
        );
        assert!(engine.execute("db", "SHOW TABLES", 10).is_ok());

        // The copy completes and the table serves again.
        meta.finish_table_resnapshot("db", "a", "ready").unwrap();
        let served = engine.execute("db", "SELECT COUNT(*) FROM a", 10).unwrap();
        assert_eq!(served.rows, vec![vec![Value::UInt64(2)]]);

        // Flagged for a resync it has not started (a quarantine): the store
        // is whole and keeps serving.
        meta.mark_table_needs_resync("db", "a", "ambiguous keyless rows")
            .unwrap();
        let served = engine.execute("db", "SELECT COUNT(*) FROM a", 10).unwrap();
        assert_eq!(served.rows, vec![vec![Value::UInt64(2)]]);

        // A copy interrupted part-way and flagged for retry: the store is
        // partial and is refused until the retry completes.
        meta.begin_table_resnapshot("db", "a").unwrap();
        meta.fail_table_copy("db", "a", "process stopped mid-copy", true)
            .unwrap();
        assert!(
            matches!(
                engine.execute("db", "SELECT COUNT(*) FROM a", 10),
                Err(QueryError::NotReady(_))
            ),
            "an interrupted copy stays refused"
        );
        meta.finish_table_resnapshot("db", "a", "ready").unwrap();
        assert!(engine.execute("db", "SELECT COUNT(*) FROM a", 10).is_ok());
    }

    /// A source ALTER whose new column needs values recopies the table; the
    /// statements that arrive meanwhile wait for the copy instead of being
    /// refused, and are refused only when the copy outlasts the wait.
    #[test]
    fn a_statement_waits_for_a_table_recopied_after_a_schema_change() {
        const NOW: &str = "2026-09-07T00:00:00Z";
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let mut meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", NOW).unwrap();
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a VALUES (1), (2)",
        ] {
            writer.execute(&parse_statement(sql).unwrap()).unwrap();
        }
        let engine = ReplicaEngine::new(directory.path(), &metadata_path)
            .with_recopy_wait(Duration::from_secs(20));
        let count = |engine: &ReplicaEngine| engine.execute("db", "SELECT COUNT(*) FROM a", 10);
        assert_eq!(count(&engine).unwrap().rows, vec![vec![Value::UInt64(2)]]);

        // A second schema generation, as a streamed ALTER records one, and
        // the recopy it queued begins.
        let columns = meta
            .schema_history("db", "a")
            .unwrap()
            .last()
            .map(|record| record.columns_json.clone())
            .or_else(|| {
                let database = meta.database("db").unwrap().unwrap();
                let report: ProbeReport =
                    serde_json::from_str(database.probe_json.as_deref()?).ok()?;
                let table = report.tables.into_iter().find(|table| table.name == "a")?;
                serde_json::to_string(&table.columns).ok()
            })
            .expect("the table's columns");
        let version = meta
            .schema_history("db", "a")
            .unwrap()
            .last()
            .map_or(2, |record| record.version + 1);
        meta.record_schema_history(
            "db",
            "a",
            version,
            Some("ALTER TABLE a ADD COLUMN region INT NOT NULL DEFAULT 7"),
            &columns,
            NOW,
        )
        .unwrap();
        if meta.schema_history("db", "a").unwrap().len() < 2 {
            meta.record_schema_history(
                "db",
                "a",
                version + 1,
                Some("ALTER TABLE a"),
                &columns,
                NOW,
            )
            .unwrap();
        }
        meta.begin_table_resnapshot("db", "a").unwrap();

        // The copy outlasts a short wait: refused, after waiting.
        let impatient = ReplicaEngine::new(directory.path(), &metadata_path)
            .with_recopy_wait(Duration::from_millis(150));
        let started = Instant::now();
        assert!(matches!(count(&impatient), Err(QueryError::NotReady(_))));
        assert!(started.elapsed() >= Duration::from_millis(150));
        // No wait at all: refused at once, as before.
        let unwilling =
            ReplicaEngine::new(directory.path(), &metadata_path).with_recopy_wait(Duration::ZERO);
        assert!(matches!(count(&unwilling), Err(QueryError::NotReady(_))));

        // The copy ends while a statement waits: it answers.
        let finisher = {
            let metadata_path = metadata_path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                MetaStore::open(&metadata_path)
                    .unwrap()
                    .finish_table_resnapshot("db", "a", "ready")
                    .unwrap();
            })
        };
        let started = Instant::now();
        let answered = count(&engine).expect("the statement waits for the copy");
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert_eq!(answered.rows.len(), 1);
        finisher.join().unwrap();
    }

    /// A table under copy, its engine holding the replica that says so,
    /// then the copy finished - and the engine left holding what a signature
    /// read just behind that commit leaves: the files as the commit wrote
    /// them, remembered against the signature from before it.
    fn engine_holding_a_signature_read_behind_a_commit(
        recopy_wait: Duration,
    ) -> (tempfile::TempDir, ReplicaEngine) {
        const NOW: &str = "2026-10-02T00:00:00Z";
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", NOW).unwrap();
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a VALUES (1), (2)",
        ] {
            writer.execute(&parse_statement(sql).unwrap()).unwrap();
        }
        let engine =
            ReplicaEngine::new(directory.path(), &metadata_path).with_recopy_wait(Duration::ZERO);
        meta.begin_table_resnapshot("db", "a").unwrap();
        assert!(matches!(
            engine.execute("db", "SELECT COUNT(*) FROM a", 10),
            Err(QueryError::NotReady(_))
        ));
        let copying = engine.replica_stamp("db").metadata.signature;
        meta.finish_table_resnapshot("db", "a", "ready").unwrap();
        let files = engine.replica_stamp("db").metadata.files;
        engine.signatures.lock().unwrap().insert(
            "db".to_owned(),
            SignatureMemo {
                files,
                generation: None,
                signature: copying,
                read_at: Instant::now(),
                settled: false,
            },
        );
        assert_eq!(
            engine.replica_stamp("db").metadata.signature,
            copying,
            "the engine holds the signature from before the commit"
        );
        (directory, engine.with_recopy_wait(recopy_wait))
    }

    /// A statement refused for a table still being copied asks the store
    /// again rather than the signature it holds: that signature may have
    /// been read just behind the commit that finished the copy, with the
    /// files already as the commit left them, and nothing would ever move
    /// them again. The statement that waits must answer at its next look.
    #[test]
    fn a_refused_statement_sees_a_copy_that_finished_before_it_asked_again() {
        let (_directory, engine) =
            engine_holding_a_signature_read_behind_a_commit(Duration::from_secs(600));
        let started = Instant::now();
        let answered = engine
            .execute("db", "SELECT COUNT(*) FROM a", 10)
            .expect("the copy finished before the statement arrived");
        assert_eq!(answered.rows, vec![vec![Value::UInt64(2)]]);
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the statement waited out a copy that had finished"
        );
    }

    /// The same for a statement that does not wait: its refusal stands, as
    /// what it was told was read before the commit, and the next statement
    /// is answered.
    #[test]
    fn a_refusal_is_not_repeated_from_a_signature_read_behind_the_commit() {
        let (_directory, engine) = engine_holding_a_signature_read_behind_a_commit(Duration::ZERO);
        assert!(matches!(
            engine.execute("db", "SELECT COUNT(*) FROM a", 10),
            Err(QueryError::NotReady(_))
        ));
        assert_eq!(
            engine
                .execute("db", "SELECT COUNT(*) FROM a", 10)
                .unwrap()
                .rows,
            vec![vec![Value::UInt64(2)]]
        );
    }

    /// With no refusal to prompt it, a signature read close behind a write
    /// is read once more when its files have settled, and corrected.
    #[test]
    fn a_signature_read_behind_a_commit_is_read_again_once_its_files_settle() {
        let (_directory, engine) = engine_holding_a_signature_read_behind_a_commit(Duration::ZERO);
        let held = engine.replica_stamp("db").metadata.signature;
        // As if it had been read longer ago than a commit takes to publish.
        {
            let mut memo = engine.signatures.lock().unwrap();
            let entry = memo.get_mut("db").unwrap();
            entry.read_at = Instant::now()
                .checked_sub(SIGNATURE_SETTLES_AFTER * 2)
                .expect("a clock that has run for two seconds");
        }
        let settled = engine.replica_stamp("db").metadata.signature;
        assert_ne!(settled, held, "the second reading sees the commit");
        assert!(engine.signatures.lock().unwrap()["db"].settled);
        // And a settled reading is not read again.
        assert_eq!(engine.replica_stamp("db").metadata.signature, settled);
        assert_eq!(
            engine
                .execute("db", "SELECT COUNT(*) FROM a", 10)
                .unwrap()
                .rows,
            vec![vec![Value::UInt64(2)]]
        );
    }

    #[test]
    fn reserved_execution_rechecks_real_replica_size_and_freshness() {
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", "2026-09-05T00:00:00Z")
            .unwrap();
        drop(meta);
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
            "INSERT INTO a VALUES (1)",
        ] {
            writer.execute(&parse_statement(sql).unwrap()).unwrap();
        }
        let mut engine = ReplicaEngine::new(directory.path(), &metadata_path);
        engine.admission = Arc::new(QueryAdmission::with_wait(4, Duration::from_millis(1)));
        let sql = "SELECT id FROM a WHERE id = 1";
        // Cold load cannot claim the reserve, even for a small LIMIT.
        let permits = (0..3)
            .map(|_| engine.admission.try_admit().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            engine.execute("db", sql, 10),
            Err(QueryError::Overloaded)
        ));
        drop(permits);
        engine.execute("db", sql, 10).unwrap();
        let permits = (0..3)
            .map(|_| engine.admission.try_admit().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            engine.execute("db", sql, 10).unwrap().rows,
            vec![vec![Value::UInt64(1)]]
        );
        assert!(matches!(
            engine.execute("db", "SELECT COUNT(*) FROM a", 10),
            Err(QueryError::Overloaded)
        ));
        // A write behind the reader makes the cached classification stale.
        writer
            .execute(&parse_statement("INSERT INTO a VALUES (2)").unwrap())
            .unwrap();
        assert!(matches!(
            engine.execute("db", sql, 10),
            Err(QueryError::Overloaded)
        ));
        drop(permits);
        engine.execute("db", sql, 10).unwrap();
        // The physical bound includes memtable/CDC rows, not rows_synced.
        let values = (3..=1025)
            .map(|id| format!("({id})"))
            .collect::<Vec<_>>()
            .join(",");
        writer
            .execute(&parse_statement(&format!("INSERT INTO a VALUES {values}")).unwrap())
            .unwrap();
        engine.execute("db", sql, 10).unwrap();
        let _permits = (0..3)
            .map(|_| engine.admission.try_admit().unwrap())
            .collect::<Vec<_>>();
        assert!(engine.execute("db", "SELECT id FROM a LIMIT 1", 10).is_ok());
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;
    use pintail_write::LocalDatabase;

    /// A local database holding table `a` with ids `1..=rows`, and an
    /// engine whose replica of it is loaded.
    fn loaded(rows: u64) -> (tempfile::TempDir, LocalDatabase, ReplicaEngine) {
        let directory = tempfile::tempdir().unwrap();
        let metadata_path = directory.path().join("meta.db");
        let meta = MetaStore::open(&metadata_path).unwrap();
        meta.create_local_database("db", "scratch", "2026-10-02T00:00:00Z")
            .unwrap();
        drop(meta);
        std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
        writer.recover().unwrap();
        let values = (1..=rows)
            .map(|id| format!("({id}, {})", id % 7))
            .collect::<Vec<_>>()
            .join(",");
        for sql in [
            "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, n BIGINT NOT NULL, PRIMARY KEY (id))"
                .to_owned(),
            format!("INSERT INTO a VALUES {values}"),
        ] {
            writer.execute(&parse_statement(&sql).unwrap()).unwrap();
        }
        let engine = ReplicaEngine::new(directory.path(), &metadata_path);
        engine
            .execute("db", "SELECT id FROM a WHERE id = 1", 10)
            .unwrap();
        (directory, writer, engine)
    }

    fn inline(engine: &ReplicaEngine, sql: &str) -> InlineAnswer {
        engine.execute_answer_inline("db", sql, 1000, None).unwrap()
    }

    /// The inline lane is the same engine on another thread: what it runs
    /// it answers exactly as a worker does.
    #[test]
    fn a_statement_that_reads_no_table_runs_inline_and_answers_as_a_worker_does() {
        let (_directory, _writer, engine) = loaded(10);
        for sql in [
            "SELECT 1 + 1",
            "SELECT UPPER('abc'), CONCAT('a', 'b') AS joined, 7 / 0",
            "SELECT 3 AS n, NULL, 'x' ORDER BY 1 LIMIT 1",
            "SELECT 1 WHERE 1 = 0",
        ] {
            let InlineAnswer::Answered(Answer::Whole(answered)) = inline(&engine, sql) else {
                panic!("{sql} is bounded and must run inline");
            };
            let worker = engine.execute("db", sql, 1000).unwrap();
            assert_eq!(answered.rows, worker.rows, "{sql}");
            assert_eq!(
                answered
                    .fields
                    .iter()
                    .map(|field| (&field.name, field.data_type, field.nullable))
                    .collect::<Vec<_>>(),
                worker
                    .fields
                    .iter()
                    .map(|field| (&field.name, field.data_type, field.nullable))
                    .collect::<Vec<_>>(),
                "{sql}"
            );
        }
        // An error is the statement's answer on either thread.
        for sql in ["SELECT nope", "SELEC 1"] {
            let inline = engine.execute_answer_inline("db", sql, 10, None);
            let worker = engine.execute("db", sql, 10);
            assert!(worker.is_err(), "{sql}");
            assert_eq!(
                inline.err().map(|error| error.to_string()),
                worker.err().map(|error| error.to_string()),
                "{sql}"
            );
        }
    }

    /// The thread a statement arrives on serves other connections, so
    /// anything that reads a table, or whose work its text does not bound,
    /// is left for a worker - and left untouched, so the worker starts it
    /// clean.
    #[test]
    fn work_that_is_not_bounded_is_declined_inline() {
        let (_directory, _writer, engine) = loaded(10);
        let long = format!("SELECT '{}'", "x".repeat(INLINE_STATEMENT_BYTES));
        for sql in [
            "SELECT id FROM a WHERE id = 3",
            "SELECT COUNT(*) FROM a",
            "SELECT id FROM a ORDER BY n LIMIT 1",
            "SELECT 'aaaa' REGEXP '(a+)+$'",
            "SELECT REPEAT('a', 4096) LIKE CONCAT('%', REPEAT('a', 2048), 'b')",
            "SELECT @payload LIKE '%b'",
            "SELECT REPLACE(REPEAT('a', 4096), 'a', REPEAT('a', 4096))",
            "SELECT (SELECT 1)",
            "SELECT 1 UNION SELECT 2",
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 9) SELECT i FROM c",
            "SELECT table_name FROM information_schema.tables",
            "SELECT 'information_schema'",
            "SHOW TABLES",
            long.as_str(),
        ] {
            assert!(
                matches!(inline(&engine, sql), InlineAnswer::NotBounded),
                "{sql} must go to a worker"
            );
        }
    }

    /// A statement in the inline lane is stopped by what stops it on a
    /// worker: a cancelled execution (`KILL QUERY`) and an elapsed deadline
    /// (`max_execution_time`) are read from the same place by the same
    /// code, so each gives the answer a worker gives. A statement that
    /// would take time or wait is never in the lane to begin with.
    #[test]
    fn a_killed_or_timed_out_statement_ends_inline_as_it_does_on_a_worker() {
        let (_directory, _writer, engine) = loaded(10);
        let outcome = |result: Result<String, QueryError>| match result {
            Ok(rows) => format!("ok {rows}"),
            Err(error) => format!("err {error}"),
        };
        let inline_rows = |engine: &ReplicaEngine, sql: &str, deadline| {
            engine
                .execute_answer_inline("db", sql, 1000, deadline)
                .map(|answer| match answer {
                    InlineAnswer::Answered(Answer::Whole(output)) => format!("{:?}", output.rows),
                    other => panic!("{sql} must run inline, got {other:?}"),
                })
        };
        let worker_rows = |engine: &ReplicaEngine, sql: &str, deadline| {
            engine
                .execute_with_deadline("db", sql, 1000, deadline)
                .map(|output| format!("{:?}", output.rows))
        };
        for sql in [
            "SELECT 1 + 1",
            "SELECT REPEAT('ab', 2000), UPPER('abc'), MD5('a')",
        ] {
            // A deadline that passed before the statement started.
            let elapsed = Instant::now().checked_sub(Duration::from_millis(1));
            assert_eq!(
                outcome(inline_rows(&engine, sql, elapsed)),
                outcome(worker_rows(&engine, sql, elapsed)),
                "{sql} past its deadline"
            );
            // An execution cancelled before the statement started.
            let killed = |run: &dyn Fn() -> Result<String, QueryError>| {
                let cancellation = pintail_exec::ExecutionCancellation::new();
                cancellation.cancel();
                pintail_exec::with_execution_cancellation(cancellation, run)
            };
            assert_eq!(
                outcome(killed(&|| inline_rows(&engine, sql, None))),
                outcome(killed(&|| worker_rows(&engine, sql, None))),
                "{sql} killed"
            );
        }
        // What takes time or waits is declined before anything runs.
        for sql in [
            "SELECT SLEEP(5)",
            "SELECT BENCHMARK(100000000, MD5('a'))",
            "SELECT GET_LOCK('a', 10)",
        ] {
            assert!(
                matches!(
                    engine.execute_answer_inline("db", sql, 10, None),
                    Ok(InlineAnswer::NotBounded)
                ),
                "{sql} must go to a worker"
            );
        }
    }

    /// Waiting for capacity is a worker's to do: with every slot taken the
    /// inline lane declines at once rather than holding its thread.
    #[test]
    fn a_statement_is_not_run_inline_while_no_slot_is_free() {
        let (_directory, _writer, mut engine) = loaded(10);
        engine.admission = Arc::new(QueryAdmission::with_wait(4, Duration::from_secs(30)));
        let permits = (0..4)
            .map(|_| engine.admission.try_admit_class(QueryClass::Short).unwrap())
            .collect::<Vec<_>>();
        let asked = Instant::now();
        assert!(matches!(
            inline(&engine, "SELECT 1 + 1"),
            InlineAnswer::NotNow
        ));
        assert!(matches!(
            inline(&engine, "SELECT UPPER('abc')"),
            InlineAnswer::NotNow
        ));
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "the inline lane waited for a slot"
        );
        drop(permits);
        assert!(matches!(
            inline(&engine, "SELECT 1 + 1"),
            InlineAnswer::Answered(_)
        ));
    }

    /// A replica a commit has superseded is reloaded by a worker, never on
    /// the receiving thread, and never answered from stale.
    #[test]
    fn a_superseded_replica_is_left_for_a_worker_to_reload() {
        let (_directory, writer, engine) = loaded(10);
        writer
            .execute(&parse_statement("INSERT INTO a VALUES (11, 4)").unwrap())
            .unwrap();
        assert!(matches!(
            inline(&engine, "SELECT 1 + 1"),
            InlineAnswer::NotNow
        ));
        assert_eq!(
            engine
                .execute("db", "SELECT id FROM a WHERE id = 11", 10)
                .unwrap()
                .rows,
            vec![vec![Value::UInt64(11)]]
        );
        assert!(matches!(
            inline(&engine, "SELECT 1 + 1"),
            InlineAnswer::Answered(_)
        ));
    }
}
