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
use crate::replica_cache::{
    self, CacheKey, FileStamp, Lookup, ReplicaCache, ReplicaCacheStats, ReplicaStamp,
};
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
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
    pub truncated: bool,
    /// Rows a WRITE changed, when the statement changed rows instead of
    /// returning them. `None` is a result set - every read answers `None`,
    /// so a query can never be mistaken for a write - and `Some` makes the
    /// server answer with an OK packet carrying this count.
    pub affected: Option<u64>,
}

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
    /// 1242: a scalar subquery produced more than one row.
    SubqueryRows,
    /// 3143: a JSON path expression does not parse.
    InvalidJsonPath,
}

/// The metadata files a database's signature was last read against, and
/// that signature.
type SignatureMemo = (Vec<FileStamp>, u64);

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
}

static SHARED_REPLICA_CACHE: OnceLock<Arc<ReplicaCache<LoadedReplica>>> = OnceLock::new();

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
        Self {
            data_dir: data_dir.into(),
            metadata_path: metadata_path.into(),
            memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
            admission: shared_admission(),
            cache: shared_replica_cache(),
            signatures: Arc::new(Mutex::new(HashMap::new())),
            signature_reader: Arc::new(Mutex::new(None)),
        }
    }

    /// The metadata signature for `files`, from the memo when the files are
    /// the ones last read, otherwise from the store. A store that cannot be
    /// read falls back to a hash of the files themselves, which is the old
    /// behaviour: safe, and no worse.
    fn metadata_signature(&self, database_id: &str, files: &[FileStamp]) -> u64 {
        if let Ok(memo) = self.signatures.lock()
            && let Some((known_files, signature)) = memo.get(database_id)
            && known_files == files
        {
            return *signature;
        }
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
            std::hash::Hasher::finish(&hasher)
        });
        if let Ok(mut memo) = self.signatures.lock() {
            memo.insert(database_id.to_owned(), (files.to_vec(), signature));
        }
        signature
    }

    /// Every file whose content can change what a query sees: the metadata
    /// store plus each table directory's entries (manifests, WALs and the
    /// immutable segment set). Any CDC apply, flush, compaction or schema
    /// change alters at least one (path, len, mtime) triple - and the stamp
    /// keeps them per table, so the reload that follows touches only the
    /// table that changed.
    fn replica_stamp(&self, database_id: &str) -> ReplicaStamp {
        fn record(files: &mut Vec<FileStamp>, path: &Path) {
            if let Ok(meta) = std::fs::metadata(path) {
                files.push((path.to_path_buf(), meta.len(), meta.modified().ok()));
            }
        }
        let mut stamp = ReplicaStamp::default();
        record(&mut stamp.metadata.files, &self.metadata_path);
        // Metadata writes land in SQLite's WAL, not the main file — without
        // it a replica cached between a table's files appearing and its
        // metadata rows committing stays stale until unrelated data churn.
        let mut wal = self.metadata_path.as_os_str().to_owned();
        wal.push("-wal");
        record(&mut stamp.metadata.files, Path::new(&wal));
        stamp.metadata.signature = self.metadata_signature(database_id, &stamp.metadata.files);
        let Ok(entries) = std::fs::read_dir(self.tables_root(database_id)) else {
            return stamp;
        };
        let mut tables: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        tables.sort();
        for table in tables {
            let name = table
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let files = stamp.tables.entry(name).or_default();
            if !table.is_dir() {
                record(files, &table);
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
                        record(files, &path);
                    }
                }
            }
        }
        stamp
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
    fn short_query_replica(
        &self,
        database_id: &str,
        statement: &Statement,
    ) -> Option<Arc<LoadedReplica>> {
        if !pintail_sql::has_bounded_planning_shape(statement) {
            return None;
        }
        let key = self.cache_key(database_id);
        let replica = self.cache.peek(&key)?;
        let stamp = self.replica_stamp(database_id);
        let tiny = pintail_sql::has_bounded_admission_shape(statement)
            && replica.targets.len() <= 16
            && replica.targets.iter().fold(0_u64, |rows, table| {
                rows.saturating_add(table.snapshot.physical_row_upper_bound())
            }) <= 1024
            && replica
                .targets
                .iter()
                .map(|table| table.snapshot.schema().columns().len())
                .sum::<usize>()
                <= 128
            && {
                // Taken from disk, not from what the cache happens to hold:
                // the same stamp screens the size below and proves the
                // replica current at the end, so a short query never reads
                // a snapshot a commit has already superseded.
                stamp.files() <= 128
                    && stamp
                        .tables
                        .values()
                        .flatten()
                        .fold(0_u64, |bytes, file| bytes.saturating_add(file.1))
                        <= 4 * 1024 * 1024
            };
        if tiny {
            return revalidated(&self.cache, &key, &stamp, &replica);
        }
        let catalog = build_catalog(&replica).ok()?;
        let bound = Binder::new(&catalog, Some(&replica.database.name))
            .bind(statement)
            .ok()?;
        let collation = pintail_exec::collation::Collation::from_mysql_name(bound.text_collation)
            .unwrap_or_default();
        let physical =
            PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)), collation)
                .ok()?;
        let provider = build_provider(&replica).ok()?;
        let cost = provider.admission_cost(&physical)?;
        if QueryClass::from_cost(Some(cost)) != QueryClass::Short {
            return None;
        }
        revalidated(&self.cache, &key, &stamp, &replica)
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
                "query setup db={database_id} stamp={:.1}ms files={} replica=cached",
                stamped.as_secs_f64() * 1_000.0,
                stamp.files()
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
                    "query setup db={database_id} stamp={:.1}ms files={} replica=coalesced",
                    stamped.as_secs_f64() * 1_000.0,
                    stamp.files()
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
            "query setup db={database_id} stamp={:.1}ms files={} replica=reloaded in {:.1}ms \
             tables={} opened={opened} resident={resident}B",
            stamped.as_secs_f64() * 1_000.0,
            stamp.files(),
            load_started.elapsed().as_secs_f64() * 1_000.0,
            replica.targets.len()
        );
        self.cache
            .insert(key, stamp, Arc::clone(&replica), resident, opened);
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
    #[allow(clippy::too_many_lines)]
    pub fn execute_with_deadline(
        &self,
        database_id: &str,
        sql: &str,
        max_rows: usize,
        deadline: Option<Instant>,
    ) -> Result<QueryOutput, QueryError> {
        let started = Instant::now();
        // Bound classification work itself. Large statements acquire general
        // capacity before parsing; small ones may qualify for the reserve.
        let (statement, short_replica, _permit) = if sql.len() <= 8192 {
            let statement =
                parse_statement(sql).map_err(|error| QueryError::Invalid(error.to_string()))?;
            let replica = self.short_query_replica(database_id, &statement);
            let class = if replica.is_some() {
                QueryClass::Short
            } else {
                QueryClass::General
            };
            let mut permit = self
                .admission
                .try_admit_class(class)
                .ok_or(QueryError::Overloaded)?;
            let replica = replica.filter(|candidate| {
                matches!(self.cache.lookup(&self.cache_key(database_id), &self.replica_stamp(database_id)),
                    Lookup::Hit(current) if Arc::ptr_eq(candidate, &current))
            });
            if class == QueryClass::Short && replica.is_none() {
                drop(permit);
                permit = self.admission.try_admit().ok_or(QueryError::Overloaded)?;
            }
            (statement, replica, permit)
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
            return self.execute_write(database_id, &statement, started);
        }
        if is_transaction_control(&statement) {
            return Err(self.transaction_control_rejection(database_id));
        }
        let replica = match short_replica {
            Some(replica) => replica,
            None => self.load_replica_cached(database_id)?,
        };
        let catalog = build_catalog(&replica)?;
        let mut provider = build_provider(&replica)?;
        let table_count = replica.targets.len();
        // `/*+ MAX_EXECUTION_TIME(ms) */` is scoped to the statement and
        // tightens whatever the session already allows - never loosens it, so
        // a hint cannot be used to escape an administrator's ceiling. A hint
        // of 0 means "no ceiling" in MySQL and simply leaves the session's in
        // force.
        let deadline = match pintail_sql::max_execution_time_hint(&statement) {
            Some(milliseconds) if milliseconds > 0 => Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .map(|hinted| deadline.map_or(hinted, |held| held.min(hinted)))
                .or(deadline),
            _ => deadline,
        };
        let facts = column_facts(&replica);
        match execute_metadata(&statement, &catalog, Some(&replica.database.name), &facts) {
            Ok(result) => return Ok(metadata_output(result, started)),
            Err(MetadataError::Unsupported(_)) => {}
            Err(error) => return Err(QueryError::Invalid(error.to_string())),
        }
        if matches!(statement, Statement::Query(_))
            && sql.to_ascii_lowercase().contains("information_schema")
        {
            let mut statement = statement.clone();
            pintail_sql::resolve_database_function(&mut statement, &replica.database.name);
            let (metadata_catalog, metadata_provider) =
                crate::metadata_provider::MetadataProvider::new(&catalog, &facts)?;
            return self.execute_select(
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
            );
        }
        match statement {
            Statement::Query(_) => {
                let run = || {
                    self.execute_select(
                        &statement,
                        sql,
                        &catalog,
                        &provider,
                        &facts,
                        &replica.database.name,
                        provider_stats(&provider, table_count),
                        started,
                        max_rows,
                        deadline,
                        true,
                    )
                };
                // Several clients asking the same question of the same
                // snapshot at the same time is one question. Only a
                // statement whose answer cannot depend on the clock, the
                // connection or a random source is offered; everything
                // else executes as it always did.
                if !pintail_sql::is_repeatable_statement(&statement) {
                    return run();
                }
                let key = SharedQueryKey::for_current_session(replica.load_id, sql, max_rows);
                match shared_queries().join(&key, deadline) {
                    Join::Alone => run(),
                    Join::Followed(output) => Ok(followed_output(&output, started)),
                    Join::Lead(leader) => {
                        let result = run();
                        if let Ok(output) = &result {
                            leader.succeeded(output);
                        }
                        result
                    }
                }
            }
            Statement::Explain { .. } => self.execute_explain(
                &statement,
                &catalog,
                &mut provider,
                &replica.database.name,
                table_count,
                started,
                deadline,
            ),
            _ => Err(QueryError::Invalid(
                "Pintail's query surfaces are read-only".to_owned(),
            )),
        }
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
            rows: Vec::new(),
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
        mut stats: QueryStats,
        started: Instant,
        max_rows: usize,
        deadline: Option<Instant>,
        optimize: bool,
    ) -> Result<QueryOutput, QueryError> {
        // Admission planning may already have folded this statement's
        // constants; only this execution's divisions by zero are its own.
        let _ = pintail_exec::take_session_division_warnings();
        pintail_sql::set_session_database_name(Some(database_name));
        let bound = Binder::new(catalog, Some(database_name))
            .with_source(sql)
            .bind(statement)
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
        let mut execution = Execution::start_with_deadline(
            physical,
            provider,
            self.memory_limit,
            deadline,
            collation,
        )
        .map_err(query_execution_error)?;
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
            .collect();
        let (rows, batches, truncated) = collect_rows(&mut execution, max_rows)?;
        // Development profiling (PINTAIL_PROFILE): one block per query with
        // every operator's time, rows and peak reservation.
        if let Some(profile) = execution.profile() {
            let statement = sql.trim();
            let shown: String = statement.chars().take(160).collect();
            pintail_log::log_info!(
                "pintail profile db={database_name} rows={} sql={shown:?}\n{}",
                rows.len(),
                profile.render().trim_end()
            );
        }
        stats.duration_ms = elapsed_ms(started);
        stats.rows = rows.len();
        stats.batches = batches;
        Ok(QueryOutput {
            fields,
            rows,
            stats,
            truncated,
            affected: None,
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
            rows: vec![vec![Value::Utf8(plan)]],
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
                // Reuse needs all three unchanged: the probe-derived
                // definition (a reprobe can change columns without a new
                // schema version), the schema version, and the table's own
                // files. Everything else in the replica is rebuilt from the
                // metadata store, which is cheap.
                let reusable = previous.and_then(|(replica, stamp)| {
                    let target = replica
                        .targets
                        .iter()
                        .find(|target| target.source.name.eq_ignore_ascii_case(&source.name))?;
                    (target.version == version
                        && target.source == source
                        && stamp.tables.get(&directory_name) == current.tables.get(&directory_name))
                    .then(|| target.snapshot.clone())
                });
                let snapshot = if let Some(snapshot) = reusable {
                    snapshot
                } else {
                    let schema = source
                        .table_schema_with_version(version)
                        .map_err(|error| QueryError::Internal(error.to_string()))?;
                    opened += 1;
                    TableSnapshot::open(directory, schema)
                        .map_err(|error| QueryError::NotReady(error.to_string()))?
                };
                Ok(ReaderTarget {
                    source,
                    version,
                    snapshot,
                    ready,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            LoadedReplica {
                server_version: report.server.version,
                load_id: NEXT_REPLICA_LOAD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                database,
                tables,
                targets,
            },
            opened,
        ))
    }
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

fn collect_rows(
    execution: &mut Execution,
    max_rows: usize,
) -> Result<(Vec<Vec<Value>>, usize, bool), QueryError> {
    let mut rows = Vec::new();
    let mut batches = 0;
    while let Some(batch) = execution.next_batch().map_err(query_execution_error)? {
        batches += 1;
        for row in batch.selection().selected_rows() {
            if rows.len() == max_rows {
                return Ok((rows, batches, true));
            }
            let values = batch
                .columns()
                .iter()
                .map(|column| {
                    column.value(row).cloned().ok_or_else(|| {
                        QueryError::Internal("query batch has a missing value".to_owned())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
        }
    }
    Ok((rows, batches, false))
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
        ExecError::TableNotReady { .. } => QueryError::NotReady(error.to_string()),
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
        ExecError::InvalidJsonPath { .. } => QueryError::Rejected {
            rejection: SqlRejection::InvalidJsonPath,
            message: error.to_string(),
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
        ExplainError::Exec(ExecError::TableNotReady { .. }) => {
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
        rows: result.rows,
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
    let entries = replica
        .targets
        .iter()
        .enumerate()
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
            .map_err(|error| QueryError::Internal(error.to_string()))?;
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
        if !target.ready {
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
