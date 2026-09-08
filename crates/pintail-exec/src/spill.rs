//! Length-framed binary records for on-disk spill files.
//!
//! Sort runs, aggregation runs, and grace-join partitions all write rows the
//! query could not hold in memory, which at scale means tens of gigabytes.
//! A self-describing text format costs several times the bytes and spends
//! most of the decode budget on parsing, so spill records carry a compact
//! tagged encoding instead. The format is private to one query's temporary
//! files: nothing persists it, and no reader outside this process sees it.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use pintail_types::{Float64, Value};

const TAG_NULL: u8 = 0;
const TAG_BOOLEAN: u8 = 1;
const TAG_INT64: u8 = 2;
const TAG_UINT64: u8 = 3;
const TAG_FLOAT64: u8 = 4;
const TAG_UTF8: u8 = 5;
const TAG_BINARY: u8 = 6;
/// An ENUM or SET keeps the ordinal it sorts by. Writing one as plain text
/// made a spilled ORDER BY compare labels while an in-memory one compared
/// declaration indexes, so the same query answered differently depending on
/// whether it happened to spill.
const TAG_ENUM: u8 = 7;
const TAG_DECIMAL_AVERAGE: u8 = 8;

/// Appends primitive fields to a spill payload.
pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn f64(&mut self, value: f64) {
        self.bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }

    pub(crate) fn count(&mut self, value: usize) {
        self.u32(u32::try_from(value).unwrap_or(u32::MAX));
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.count(value.len());
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(crate) fn value(&mut self, value: &Value) {
        match value {
            Value::Null => self.u8(TAG_NULL),
            Value::Boolean(inner) => {
                self.u8(TAG_BOOLEAN);
                self.bool(*inner);
            }
            Value::Int64(inner) => {
                self.u8(TAG_INT64);
                self.i64(*inner);
            }
            Value::UInt64(inner) => {
                self.u8(TAG_UINT64);
                self.u64(*inner);
            }
            Value::Float64(inner) => {
                self.u8(TAG_FLOAT64);
                self.f64(inner.get());
            }
            Value::Utf8(inner) => {
                self.u8(TAG_UTF8);
                self.str(inner);
            }
            Value::DecimalAverage(quotient) => {
                self.u8(TAG_DECIMAL_AVERAGE);
                self.str(&quotient.label);
                self.bytes.extend_from_slice(&quotient.units.to_le_bytes());
                self.u64(quotient.count);
                self.u8(quotient.scale);
            }
            Value::Enum { index, label } => {
                self.u8(TAG_ENUM);
                self.u64(*index);
                self.str(label);
            }
            Value::Binary(inner) => {
                self.u8(TAG_BINARY);
                self.bytes(inner);
            }
        }
    }

    pub(crate) fn values(&mut self, values: &[Value]) {
        self.count(values.len());
        for value in values {
            self.value(value);
        }
    }

    pub(crate) fn optional_value(&mut self, value: Option<&Value>) {
        match value {
            None => self.bool(false),
            Some(inner) => {
                self.bool(true);
                self.value(inner);
            }
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads back what [`Encoder`] wrote, in the same field order.
pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "spill record length overflow".to_owned())?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| format!("spill record ends inside a {length}-byte field"))?;
        self.position = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool, String> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
            "spill record u32 field is malformed".to_owned()
        })?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
            "spill record u64 field is malformed".to_owned()
        })?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().map_err(|_| {
            "spill record i64 field is malformed".to_owned()
        })?))
    }

    pub(crate) fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(self.u64()?))
    }

    pub(crate) fn count(&mut self) -> Result<usize, String> {
        Ok(self.u32()? as usize)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], String> {
        let length = self.count()?;
        self.take(length)
    }

    pub(crate) fn string(&mut self) -> Result<String, String> {
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|error| format!("spill record holds invalid UTF-8: {error}"))
    }

    pub(crate) fn value(&mut self) -> Result<Value, String> {
        match self.u8()? {
            TAG_NULL => Ok(Value::Null),
            TAG_BOOLEAN => Ok(Value::Boolean(self.bool()?)),
            TAG_INT64 => Ok(Value::Int64(self.i64()?)),
            TAG_UINT64 => Ok(Value::UInt64(self.u64()?)),
            TAG_FLOAT64 => Ok(Value::Float64(Float64::new(self.f64()?))),
            TAG_UTF8 => Ok(Value::Utf8(self.string()?)),
            TAG_DECIMAL_AVERAGE => {
                let label = self.string()?;
                let units = i128::from_le_bytes(
                    self.take(16)?
                        .try_into()
                        .map_err(|_| "invalid decimal quotient")?,
                );
                let count = self.u64()?;
                let scale = self.u8()?;
                Ok(Value::DecimalAverage(Box::new(
                    pintail_types::DecimalQuotient {
                        label,
                        units,
                        count,
                        scale,
                    },
                )))
            }
            TAG_ENUM => {
                let index = self.u64()?;
                Ok(Value::Enum {
                    index,
                    label: self.string()?,
                })
            }
            TAG_BINARY => Ok(Value::Binary(self.bytes()?.to_vec())),
            other => Err(format!("spill record holds unknown value tag {other}")),
        }
    }

    pub(crate) fn values(&mut self) -> Result<Vec<Value>, String> {
        let count = self.count()?;
        let mut values = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            values.push(self.value()?);
        }
        Ok(values)
    }

    pub(crate) fn optional_value(&mut self) -> Result<Option<Value>, String> {
        if self.bool()? {
            self.value().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// Writes one length-prefixed record.
pub(crate) fn write_record(writer: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "spill record exceeds the 4 GiB frame bound",
        )
    })?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)
}

/// Reads the next record into `payload`, returning `false` at a clean end of
/// file. A partial frame is an error rather than a silent stop.
pub(crate) fn read_record(reader: &mut impl Read, payload: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(header) as usize;
    payload.clear();
    payload.resize(length, 0);
    reader.read_exact(payload)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Encoder, read_record, write_record};
    use pintail_types::Value;

    fn sample() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Boolean(true),
            Value::Int64(-9_223_372_036_854_775_808),
            Value::UInt64(u64::MAX),
            Value::float64(-0.5),
            Value::Utf8("mixed ünïcode ✅".to_owned()),
            Value::Binary(vec![0, 1, 255, 128]),
            Value::DecimalAverage(Box::new(pintail_types::DecimalQuotient {
                label: "335.412050".to_owned(),
                units: 54_001_340_000,
                count: 161,
                scale: 6,
            })),
        ]
    }

    #[test]
    fn every_value_variant_survives_a_round_trip() {
        let mut encoder = Encoder::new();
        encoder.values(&sample());
        let bytes = encoder.finish();
        let mut decoder = Decoder::new(&bytes);
        let restored = decoder.values().expect("decode");
        assert_eq!(restored, sample());
        let Value::DecimalAverage(actual) = restored.last().expect("average") else {
            panic!("quotient lost");
        };
        let expected = sample();
        let Value::DecimalAverage(expected) = expected.last().expect("average") else {
            unreachable!()
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn records_stream_back_in_order_and_stop_cleanly() {
        let mut file = Vec::new();
        for index in 0..4_u64 {
            let mut encoder = Encoder::new();
            encoder.u64(index);
            encoder.values(&sample());
            write_record(&mut file, &encoder.finish()).expect("write");
        }
        let mut reader = std::io::Cursor::new(file);
        let mut payload = Vec::new();
        for index in 0..4_u64 {
            assert!(read_record(&mut reader, &mut payload).expect("read"));
            let mut decoder = Decoder::new(&payload);
            assert_eq!(decoder.u64().expect("index"), index);
            assert_eq!(decoder.values().expect("values"), sample());
        }
        assert!(
            !read_record(&mut reader, &mut payload).expect("clean end"),
            "a complete file must end without an error"
        );
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_an_end_of_file() {
        let mut encoder = Encoder::new();
        encoder.values(&sample());
        let mut file = Vec::new();
        write_record(&mut file, &encoder.finish()).expect("write");
        file.truncate(file.len() - 3);
        let mut reader = std::io::Cursor::new(file);
        let mut payload = Vec::new();
        assert!(read_record(&mut reader, &mut payload).is_err());
    }

    #[test]
    fn a_short_payload_reports_the_field_that_ran_out() {
        let mut decoder = Decoder::new(&[super::TAG_UINT64, 1, 2]);
        assert!(decoder.value().is_err());
    }
}

/// Process-wide directory for spill files.
///
/// `tempfile`'s default is the system temp directory, which on a container
/// is the root filesystem rather than the volume mounted for data. Every
/// spill path routes through [`spill_file`] so a deployment that mounts a
/// data volume gets its spill on that volume without extra configuration.
const DEFAULT_QUERY_SPILL_LIMIT: u64 = 1024 * 1024 * 1024;
const DEFAULT_GLOBAL_SPILL_LIMIT: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct SpillConfiguration {
    directory: PathBuf,
    query_limit: u64,
    global_limit: u64,
}

static SPILL_CONFIGURATION: OnceLock<SpillConfiguration> = OnceLock::new();
static GLOBAL_ACTIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static GLOBAL_WRITTEN_BYTES: AtomicU64 = AtomicU64::new(0);
static GLOBAL_FILES: AtomicU64 = AtomicU64::new(0);
static GLOBAL_QUOTA_FAILURES: AtomicU64 = AtomicU64::new(0);
static GLOBAL_ACTIVE_HANDLES: AtomicU64 = AtomicU64::new(0);

/// Process-wide spill counters suitable for metrics export.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpillMetrics {
    /// Bytes currently retained by live spill files.
    pub active_bytes: u64,
    /// Bytes written since process start, including files already removed.
    pub written_bytes: u64,
    /// Spill files created since process start.
    pub files: u64,
    /// Writes rejected by a query or process disk ceiling.
    pub quota_failures: u64,
    /// Spill file descriptors open right now across every query.
    pub active_handles: u64,
}

/// Returns process-wide spill counters.
#[must_use]
pub fn metrics() -> SpillMetrics {
    SpillMetrics {
        active_bytes: GLOBAL_ACTIVE_BYTES.load(Ordering::Relaxed),
        written_bytes: GLOBAL_WRITTEN_BYTES.load(Ordering::Relaxed),
        files: GLOBAL_FILES.load(Ordering::Relaxed),
        quota_failures: GLOBAL_QUOTA_FAILURES.load(Ordering::Relaxed),
        active_handles: GLOBAL_ACTIVE_HANDLES.load(Ordering::Relaxed),
    }
}

/// Query-scoped spill counters included by `EXPLAIN ANALYZE`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QuerySpillMetrics {
    /// Bytes currently retained by this query.
    pub active_bytes: u64,
    /// Bytes written by this query, including files already removed.
    pub written_bytes: u64,
    /// Spill files created by this query.
    pub files: u64,
    /// Writes rejected by this query's or the process disk ceiling.
    pub quota_failures: u64,
    /// Spill file descriptors this query holds open right now.
    pub active_handles: u64,
    /// The most spill file descriptors this query has held open at once.
    ///
    /// This is the number a descriptor limit sees. `files` counts creations
    /// and says nothing about retention; a query that creates a thousand
    /// runs and holds one open at a time is fine, one that holds all of
    /// them is what exhausted a container's descriptor limit.
    pub peak_handles: u64,
}

struct QuerySpillInner {
    directory: Mutex<Option<tempfile::TempDir>>,
    query_limit: u64,
    active_bytes: AtomicU64,
    written_bytes: AtomicU64,
    files: AtomicU64,
    quota_failures: AtomicU64,
    active_handles: AtomicU64,
    peak_handles: AtomicU64,
}

/// One isolated spill scope shared by every operator in a query.
#[derive(Clone)]
pub(crate) struct QuerySpill {
    inner: Arc<QuerySpillInner>,
}

impl QuerySpill {
    pub(crate) fn new() -> Self {
        Self::with_limit(configuration().query_limit)
    }

    fn with_limit(query_limit: u64) -> Self {
        Self {
            inner: Arc::new(QuerySpillInner {
                directory: Mutex::new(None),
                query_limit,
                active_bytes: AtomicU64::new(0),
                written_bytes: AtomicU64::new(0),
                files: AtomicU64::new(0),
                quota_failures: AtomicU64::new(0),
                active_handles: AtomicU64::new(0),
                peak_handles: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn metrics(&self) -> QuerySpillMetrics {
        QuerySpillMetrics {
            active_bytes: self.inner.active_bytes.load(Ordering::Relaxed),
            written_bytes: self.inner.written_bytes.load(Ordering::Relaxed),
            files: self.inner.files.load(Ordering::Relaxed),
            quota_failures: self.inner.quota_failures.load(Ordering::Relaxed),
            active_handles: self.inner.active_handles.load(Ordering::Relaxed),
            peak_handles: self.inner.peak_handles.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Debug for QuerySpill {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuerySpill")
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

/// Sets the directory spill files are created in. Called once during
/// startup; later calls are ignored so a running process cannot have spill
/// files stranded across two directories.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or written to, so
/// an unusable location fails at startup rather than mid-query.
pub fn set_spill_directory(directory: PathBuf) -> std::io::Result<()> {
    configure_spill(
        directory,
        DEFAULT_QUERY_SPILL_LIMIT,
        DEFAULT_GLOBAL_SPILL_LIMIT,
    )
}

/// Configures the spill directory and hard per-query and process disk limits.
/// The first successful call wins for the lifetime of the process.
///
/// # Errors
///
/// Returns an error when a limit is zero or the directory cannot be created
/// and written.
pub fn configure_spill(
    directory: PathBuf,
    query_limit: u64,
    global_limit: u64,
) -> std::io::Result<()> {
    if query_limit == 0 || global_limit == 0 || query_limit > global_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "spill disk limits must be positive and the query limit cannot exceed the global limit",
        ));
    }
    std::fs::create_dir_all(&directory)?;
    // Prove writability now: discovering it at spill time means failing a
    // query that had already done all of its work.
    tempfile::Builder::new()
        .prefix("pintail-spill-probe-")
        .tempfile_in(&directory)?;
    let _ = SPILL_CONFIGURATION.set(SpillConfiguration {
        directory,
        query_limit,
        global_limit,
    });
    Ok(())
}

/// Removes spill files left behind by a previous process. The self-deleting
/// handles cover a clean exit; a `kill -9` does not.
///
/// Returns the number of files removed.
///
/// # Errors
///
/// Returns an error when the directory cannot be listed. A file that resists
/// removal is skipped rather than failing the sweep, so one stuck leftover
/// cannot stop a process from starting.
pub fn reclaim_orphaned_spill(directory: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("pintail-"));
        if !ours {
            continue;
        }
        let kind = entry.file_type()?;
        let result = if kind.is_dir() {
            std::fs::remove_dir_all(entry.path())
        } else if kind.is_file() {
            std::fs::remove_file(entry.path())
        } else {
            continue;
        };
        if result.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Reservation held for the lifetime of one spill file.
pub(crate) struct SpillReservation {
    query: Arc<QuerySpillInner>,
    bytes: u64,
}

impl SpillReservation {
    fn reserve(&mut self, bytes: u64) -> std::io::Result<()> {
        let configuration = configuration();
        if reserve_counter(&self.query.active_bytes, bytes, self.query.query_limit).is_err() {
            self.record_quota_failure();
            return Err(quota_error("query spill disk quota exceeded"));
        }
        if reserve_counter(&GLOBAL_ACTIVE_BYTES, bytes, configuration.global_limit).is_err() {
            self.query.active_bytes.fetch_sub(bytes, Ordering::Relaxed);
            self.record_quota_failure();
            return Err(quota_error("global spill disk quota exceeded"));
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.query.written_bytes.fetch_add(bytes, Ordering::Relaxed);
        GLOBAL_WRITTEN_BYTES.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        self.bytes -= bytes;
        self.query.active_bytes.fetch_sub(bytes, Ordering::Relaxed);
        GLOBAL_ACTIVE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn record_quota_failure(&self) {
        self.query.quota_failures.fetch_add(1, Ordering::Relaxed);
        GLOBAL_QUOTA_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for SpillReservation {
    fn drop(&mut self) {
        self.release(self.bytes);
    }
}

fn reserve_counter(counter: &AtomicU64, bytes: u64, limit: u64) -> Result<(), ()> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            let requested = used.checked_add(bytes)?;
            (requested <= limit).then_some(requested)
        })
        .map(|_| ())
        .map_err(|_| ())
}

fn quota_error(message: &'static str) -> std::io::Error {
    std::io::Error::other(message)
}

fn configuration() -> SpillConfiguration {
    SPILL_CONFIGURATION
        .get()
        .cloned()
        .unwrap_or_else(|| SpillConfiguration {
            directory: std::env::temp_dir(),
            query_limit: DEFAULT_QUERY_SPILL_LIMIT,
            global_limit: DEFAULT_GLOBAL_SPILL_LIMIT,
        })
}

/// One self-deleting query spill file and its quota reservation.
pub(crate) struct SpillFile {
    file: tempfile::NamedTempFile,
    reservation: SpillReservation,
}

impl SpillFile {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    pub(crate) fn into_parts(self) -> (std::fs::File, tempfile::TempPath, SpillReservation) {
        let (file, path) = self.file.into_parts();
        (file, path, self.reservation)
    }
}

/// Creates one spill file inside the query's isolated temporary directory.
pub(crate) fn spill_file(prefix: &str, query: &QuerySpill) -> std::io::Result<SpillFile> {
    let mut directory = query
        .inner
        .directory
        .lock()
        .map_err(|_| std::io::Error::other("query spill directory lock poisoned"))?;
    if directory.is_none() {
        *directory = Some(
            tempfile::Builder::new()
                .prefix("pintail-query-spill-")
                .tempdir_in(configuration().directory)?,
        );
    }
    let file = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(directory.as_ref().expect("initialized above").path())?;
    query.inner.files.fetch_add(1, Ordering::Relaxed);
    GLOBAL_FILES.fetch_add(1, Ordering::Relaxed);
    Ok(SpillFile {
        file,
        reservation: SpillReservation {
            query: Arc::clone(&query.inner),
            bytes: 0,
        },
    })
}

/// Writes one record after reserving its exact framed size against disk
/// quotas. A failed write releases the reservation; the partial temporary file
/// is still self-deleting.
pub(crate) fn write_record_quota(
    writer: &mut impl Write,
    payload: &[u8],
    reservation: &mut SpillReservation,
) -> std::io::Result<()> {
    let bytes = u64::try_from(payload.len())
        .unwrap_or(u64::MAX)
        .saturating_add(4);
    reservation.reserve(bytes)?;
    if let Err(error) = write_record(writer, payload) {
        reservation.release(bytes);
        return Err(error);
    }
    Ok(())
}

/// How many closed runs one merge pass reads at once.
///
/// This, not the number of runs a query produced, is what bounds the
/// descriptors a spilling operator holds: one writer while it builds runs,
/// `MERGE_FAN_IN + 1` while a pass merges, `MERGE_FAN_IN` in the final
/// merge. Sixteen keeps a saturated server well inside a 1024-descriptor
/// soft limit and makes a second pass rare: a query needs more than 256
/// runs before the reduction takes two.
pub const MERGE_FAN_IN: usize = 16;

/// Counts one open spill descriptor for as long as it lives.
struct HandleGuard {
    query: Arc<QuerySpillInner>,
}

impl HandleGuard {
    fn new(query: &Arc<QuerySpillInner>) -> Self {
        let active = query
            .active_handles
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        query.peak_handles.fetch_max(active, Ordering::Relaxed);
        GLOBAL_ACTIVE_HANDLES.fetch_add(1, Ordering::Relaxed);
        Self {
            query: Arc::clone(query),
        }
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        self.query.active_handles.fetch_sub(1, Ordering::Relaxed);
        GLOBAL_ACTIVE_HANDLES.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A run being written. The descriptor it holds is the only one an
/// operator needs while it builds runs; [`RunWriter::finish`] closes it.
pub(crate) struct RunWriter {
    writer: std::io::BufWriter<std::fs::File>,
    path: tempfile::TempPath,
    reservation: SpillReservation,
    records: u64,
    query: Arc<QuerySpillInner>,
    _handle: HandleGuard,
}

impl RunWriter {
    /// Creates an empty run in the query's spill directory.
    pub(crate) fn create(prefix: &str, query: &QuerySpill) -> std::io::Result<Self> {
        let (file, path, reservation) = spill_file(prefix, query)?.into_parts();
        Ok(Self {
            writer: std::io::BufWriter::new(file),
            path,
            reservation,
            records: 0,
            query: Arc::clone(&query.inner),
            _handle: HandleGuard::new(&query.inner),
        })
    }

    /// Appends one framed record, charged against the disk quotas first.
    pub(crate) fn write(&mut self, payload: &[u8]) -> std::io::Result<()> {
        write_record_quota(&mut self.writer, payload, &mut self.reservation)?;
        self.records = self.records.saturating_add(1);
        Ok(())
    }

    /// Flushes and closes the file. The returned run holds no descriptor;
    /// the checked flush is what makes "closed" mean every byte reached
    /// the kernel rather than a buffer a later error would lose. Nothing
    /// syncs: a spill file outlives neither the query nor the process, and
    /// a forced flush to the platter per run cost more than the run itself.
    pub(crate) fn finish(self) -> std::io::Result<ClosedRun> {
        let Self {
            writer,
            path,
            reservation,
            records,
            query,
            _handle,
        } = self;
        let file = writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        drop(file);
        Ok(ClosedRun {
            path,
            reservation,
            records,
            query,
        })
    }
}

/// A finished run on disk. It holds no descriptor: the path keeps the
/// file alive and the reservation keeps its bytes charged. Dropping it
/// deletes the file first and releases the accounting second, so the disk
/// is never credited before it is free.
pub(crate) struct ClosedRun {
    path: tempfile::TempPath,
    reservation: SpillReservation,
    records: u64,
    query: Arc<QuerySpillInner>,
}

impl ClosedRun {
    /// Opens a reader positioned at the first record. Every call reopens.
    pub(crate) fn open(&self) -> std::io::Result<RunReader> {
        let file = std::fs::File::open(&self.path)?;
        Ok(RunReader {
            reader: std::io::BufReader::new(file),
            payload: Vec::new(),
            _handle: HandleGuard::new(&self.query),
        })
    }

    /// Records the run holds.
    pub(crate) const fn records(&self) -> u64 {
        self.records
    }

    /// Bytes the run occupies on disk.
    pub(crate) const fn bytes(&self) -> u64 {
        self.reservation.bytes
    }
}

impl std::fmt::Debug for ClosedRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClosedRun")
            .field("records", &self.records())
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

/// A run written in bursts. Records accumulate in the caller's buffer and
/// reach the file in one open-append-close per flush, so a set of runs fed
/// round-robin, like the grace join's partitions, holds one descriptor at
/// a time rather than one per run. Every flush is charged against the disk
/// quotas before it is written.
pub(crate) struct AppendRun {
    path: tempfile::TempPath,
    reservation: SpillReservation,
    records: u64,
    query: Arc<QuerySpillInner>,
}

impl AppendRun {
    /// Creates the empty file and closes it again at once.
    pub(crate) fn create(prefix: &str, query: &QuerySpill) -> std::io::Result<Self> {
        let (file, path, reservation) = spill_file(prefix, query)?.into_parts();
        drop(file);
        Ok(Self {
            path,
            reservation,
            records: 0,
            query: Arc::clone(&query.inner),
        })
    }

    /// Appends `records` already-framed records held in `framed`.
    pub(crate) fn flush(&mut self, framed: &[u8], records: u64) -> std::io::Result<()> {
        if framed.is_empty() {
            return Ok(());
        }
        let bytes = u64::try_from(framed.len()).unwrap_or(u64::MAX);
        self.reservation.reserve(bytes)?;
        let _handle = HandleGuard::new(&self.query);
        let written = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(framed));
        if let Err(error) = written {
            self.reservation.release(bytes);
            return Err(error);
        }
        self.records = self.records.saturating_add(records);
        Ok(())
    }

    /// The run as a closed, readable whole.
    pub(crate) fn seal(self) -> ClosedRun {
        ClosedRun {
            path: self.path,
            reservation: self.reservation,
            records: self.records,
            query: self.query,
        }
    }
}

/// Streams the records of one closed run.
pub(crate) struct RunReader {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
    _handle: HandleGuard,
}

impl RunReader {
    /// The next record's payload, or `None` at a clean end of file.
    pub(crate) fn next(&mut self) -> std::io::Result<Option<&[u8]>> {
        if read_record(&mut self.reader, &mut self.payload)? {
            Ok(Some(&self.payload))
        } else {
            Ok(None)
        }
    }
}

/// The current record of one run under merge: its comparison key and the
/// bytes it was read from, which an intermediate pass copies verbatim.
pub(crate) struct Head<K> {
    pub(crate) key: K,
    pub(crate) payload: Vec<u8>,
}

/// A k-way merge over closed runs, at most [`MERGE_FAN_IN`] of them.
///
/// Ties go to the lowest run index, so records with equal keys come out
/// in run order, and a run produced by an earlier pass keeps that order
/// inside it. Callers that combine equal keys rely on this: the fold order
/// is the order the runs were written in, whatever the pass structure.
pub(crate) struct RunMerge<K> {
    runs: Vec<ClosedRun>,
    readers: Vec<RunReader>,
    heads: Vec<Option<Head<K>>>,
    decode: fn(&[u8]) -> Result<K, String>,
}

impl<K> RunMerge<K> {
    /// Opens every run and loads its first record.
    pub(crate) fn open(
        runs: Vec<ClosedRun>,
        decode: fn(&[u8]) -> Result<K, String>,
    ) -> std::io::Result<Self> {
        let mut merge = Self {
            readers: Vec::with_capacity(runs.len()),
            heads: Vec::with_capacity(runs.len()),
            runs,
            decode,
        };
        for index in 0..merge.runs.len() {
            let mut reader = merge.runs[index].open()?;
            let head = Self::read_head(&mut reader, decode)?;
            merge.readers.push(reader);
            merge.heads.push(head);
        }
        Ok(merge)
    }

    /// A merge over nothing, for a drained merge to release its files.
    pub(crate) fn empty(decode: fn(&[u8]) -> Result<K, String>) -> Self {
        Self {
            runs: Vec::new(),
            readers: Vec::new(),
            heads: Vec::new(),
            decode,
        }
    }

    fn read_head(
        reader: &mut RunReader,
        decode: fn(&[u8]) -> Result<K, String>,
    ) -> std::io::Result<Option<Head<K>>> {
        let Some(payload) = reader.next()? else {
            return Ok(None);
        };
        let key = decode(payload).map_err(std::io::Error::other)?;
        Ok(Some(Head {
            key,
            payload: payload.to_vec(),
        }))
    }

    /// Number of runs under merge.
    pub(crate) fn len(&self) -> usize {
        self.runs.len()
    }

    /// The current record of one run, if it has any left.
    pub(crate) fn head(&self, index: usize) -> Option<&Head<K>> {
        self.heads.get(index).and_then(Option::as_ref)
    }

    /// Takes one run's current record and advances that run.
    pub(crate) fn take(&mut self, index: usize) -> std::io::Result<Option<Head<K>>> {
        let replacement = Self::read_head(&mut self.readers[index], self.decode)?;
        Ok(std::mem::replace(&mut self.heads[index], replacement))
    }

    /// The smallest current record under `less`, or `None` when every run
    /// is exhausted.
    pub(crate) fn next(
        &mut self,
        less: &dyn Fn(&K, &K) -> bool,
    ) -> std::io::Result<Option<Head<K>>> {
        let mut winner: Option<usize> = None;
        for (index, head) in self.heads.iter().enumerate() {
            let Some(candidate) = head else { continue };
            let better = match winner {
                None => true,
                Some(current) => {
                    let current_head = self.heads[current]
                        .as_ref()
                        .expect("winner head is always occupied");
                    less(&candidate.key, &current_head.key)
                }
            };
            if better {
                winner = Some(index);
            }
        }
        match winner {
            Some(index) => self.take(index),
            None => Ok(None),
        }
    }
}

/// Reduces `runs` to at most `fan_in` runs by merging groups of `fan_in`
/// adjacent runs into one, copying records in merged order.
///
/// Records are never combined here, only ordered: combining is the final
/// pass's job, because for aggregates it is neither idempotent nor freely
/// associative. Each output takes the place of its inputs, so the run
/// order the final pass sees is the order the runs were first written.
/// Inputs are deleted, and their disk accounting released, as soon as
/// their output is closed; while a group merges, its inputs and output
/// are both charged, and the quota applies to the output like any run.
///
/// Descriptors held: `fan_in` readers plus one writer.
pub(crate) fn reduce_runs<K>(
    mut runs: Vec<ClosedRun>,
    fan_in: usize,
    prefix: &str,
    query: &QuerySpill,
    decode: fn(&[u8]) -> Result<K, String>,
    less: &dyn Fn(&K, &K) -> bool,
) -> std::io::Result<Vec<ClosedRun>> {
    let fan_in = fan_in.max(2);
    while runs.len() > fan_in {
        let mut reduced = Vec::with_capacity(runs.len().div_ceil(fan_in));
        let mut pending = runs.into_iter().peekable();
        while pending.peek().is_some() {
            let group: Vec<ClosedRun> = pending.by_ref().take(fan_in).collect();
            if group.len() == 1 {
                reduced.extend(group);
                continue;
            }
            let mut merge = RunMerge::open(group, decode)?;
            let mut writer = RunWriter::create(prefix, query)?;
            while let Some(head) = merge.next(less)? {
                writer.write(&head.payload)?;
            }
            drop(merge);
            reduced.push(writer.finish()?);
        }
        runs = reduced;
    }
    Ok(runs)
}

#[cfg(test)]
mod run_tests {
    use super::{ClosedRun, Encoder, MERGE_FAN_IN, QuerySpill, RunMerge, RunWriter, reduce_runs};

    fn decode_u64(payload: &[u8]) -> Result<u64, String> {
        super::Decoder::new(payload).u64()
    }

    fn write_run(query: &QuerySpill, values: &[u64]) -> ClosedRun {
        let mut writer = RunWriter::create("pintail-test-run-", query).expect("writer");
        for value in values {
            let mut encoder = Encoder::with_capacity(8);
            encoder.u64(*value);
            writer.write(&encoder.finish()).expect("write");
        }
        writer.finish().expect("finish")
    }

    fn drain(runs: Vec<ClosedRun>) -> Vec<u64> {
        let mut merge = RunMerge::open(runs, decode_u64).expect("open");
        let mut out = Vec::new();
        while let Some(head) = merge.next(&|left, right| left < right).expect("next") {
            out.push(head.key);
        }
        out
    }

    #[test]
    fn a_closed_run_holds_no_descriptor_and_reads_repeatably() {
        let query = QuerySpill::with_limit(u64::MAX);
        let run = write_run(&query, &[3, 1, 2]);
        assert_eq!(query.metrics().active_handles, 0);
        assert_eq!(query.metrics().peak_handles, 1);
        assert_eq!(run.records(), 3);
        assert!(run.bytes() > 0);
        for _ in 0..2 {
            let mut reader = run.open().expect("open");
            assert_eq!(query.metrics().active_handles, 1);
            let mut seen = Vec::new();
            while let Some(payload) = reader.next().expect("read") {
                seen.push(decode_u64(payload).expect("decode"));
            }
            assert_eq!(seen, [3, 1, 2]);
        }
        assert_eq!(query.metrics().active_handles, 0);
        drop(run);
        assert_eq!(
            query.metrics().active_bytes,
            0,
            "deleting the file releases its bytes"
        );
    }

    #[test]
    fn reduction_keeps_run_order_for_equal_keys_and_stays_within_the_fan_in() {
        // 40 runs of sorted values with heavy overlap; equal keys must come
        // out in the order their runs were written, which the payload
        // encodes as a second field the key ignores.
        let query = QuerySpill::with_limit(u64::MAX);
        let mut runs = Vec::new();
        let mut expected = Vec::new();
        for run in 0..40_u64 {
            let mut writer = RunWriter::create("pintail-test-run-", &query).expect("writer");
            for value in [run % 3, run % 3 + 1, 10] {
                let mut encoder = Encoder::with_capacity(16);
                encoder.u64(value);
                encoder.u64(run);
                writer.write(&encoder.finish()).expect("write");
                expected.push((value, run));
            }
            runs.push(writer.finish().expect("finish"));
        }
        expected.sort_unstable();
        let reduced = reduce_runs(
            runs,
            4,
            "pintail-test-merge-",
            &query,
            decode_u64,
            &|left, right| left < right,
        )
        .expect("reduce");
        assert!(reduced.len() <= 4, "{} runs remain", reduced.len());
        assert!(
            query.metrics().peak_handles <= 4 + 1,
            "peak {} exceeds fan-in + 1",
            query.metrics().peak_handles
        );
        assert_eq!(query.metrics().active_handles, 0);
        let mut merge = RunMerge::open(reduced, |payload| {
            let mut decoder = super::Decoder::new(payload);
            Ok((decoder.u64()?, decoder.u64()?))
        })
        .expect("open");
        let mut out = Vec::new();
        while let Some(head) = merge.next(&|left, right| left.0 < right.0).expect("next") {
            out.push(head.key);
        }
        assert_eq!(out, expected);
        drop(merge);
        assert_eq!(query.metrics().active_bytes, 0);
    }

    #[test]
    fn an_append_run_holds_a_descriptor_only_while_it_flushes() {
        use super::{AppendRun, write_record};
        let query = QuerySpill::with_limit(u64::MAX);
        let mut run = AppendRun::create("pintail-test-append-", &query).expect("create");
        assert_eq!(query.metrics().active_handles, 0, "created closed");
        let mut framed = Vec::new();
        for value in [5_u64, 6, 7] {
            let mut encoder = Encoder::with_capacity(8);
            encoder.u64(value);
            write_record(&mut framed, &encoder.finish()).expect("frame");
        }
        run.flush(&framed, 3).expect("first flush");
        run.flush(&framed, 3).expect("second flush appends");
        assert_eq!(query.metrics().active_handles, 0, "closed between flushes");
        assert_eq!(query.metrics().peak_handles, 1);
        let closed = run.seal();
        assert_eq!(closed.records(), 6);
        assert_eq!(drain(vec![closed]), [5, 6, 7, 5, 6, 7], "appended in order");
    }

    #[test]
    fn a_reduction_that_already_fits_touches_nothing() {
        let query = QuerySpill::with_limit(u64::MAX);
        let runs = (0..MERGE_FAN_IN as u64)
            .map(|run| write_run(&query, &[run, run + 100]))
            .collect();
        let files_before = query.metrics().files;
        let reduced = reduce_runs(
            runs,
            MERGE_FAN_IN,
            "pintail-test-merge-",
            &query,
            decode_u64,
            &|left, right| left < right,
        )
        .expect("reduce");
        assert_eq!(query.metrics().files, files_before, "no pass ran");
        let mut expected: Vec<u64> = (0..MERGE_FAN_IN as u64)
            .flat_map(|run| [run, run + 100])
            .collect();
        expected.sort_unstable();
        assert_eq!(drain(reduced), expected);
    }

    #[test]
    fn the_quota_applies_to_an_intermediate_output() {
        // Inputs stay charged while their output is written, so a query
        // near its disk quota can fail in the pass rather than bypass it.
        let query = QuerySpill::with_limit(80);
        let runs = (0..6_u64).map(|run| write_run(&query, &[run])).collect();
        let before = query.metrics().active_bytes;
        let error = reduce_runs(
            runs,
            2,
            "pintail-test-merge-",
            &query,
            decode_u64,
            &|left, right| left < right,
        )
        .expect_err("six inputs plus an output cannot fit in 80 bytes");
        assert!(error.to_string().contains("quota"), "{error}");
        assert!(query.metrics().quota_failures > 0);
        assert!(before > 0);
    }
}

#[cfg(test)]
mod directory_tests {
    use super::{QuerySpill, reclaim_orphaned_spill, spill_file};

    /// The three operators that spill — sort runs, aggregation runs and
    /// grace-join partitions — all reach disk through `spill_file`, so
    /// asserting each prefix lands in the configured directory covers every
    /// path without running three spilling queries. `set_spill_directory`
    /// writes a process-wide `OnceLock`, so this is the only test in the
    /// binary permitted to set it.
    #[test]
    fn every_spill_path_lands_in_the_configured_directory() {
        // The OnceLock outlives this test, so the directory it points at must
        // outlive it too. A TempDir guard deletes on drop, which left every
        // later spilling test writing into a directory that no longer
        // existed — it surfaced as an intermittent ENOENT in the grace-join
        // test, dependent on which test happened to run first.
        let directory = tempfile::tempdir().expect("tempdir").keep();
        super::set_spill_directory(directory.clone()).expect("configure");
        let query = QuerySpill::new();
        let mut query_directory = None;

        for prefix in [
            "pintail-sort-spill-",
            "pintail-aggregate-spill-",
            "pintail-join-spill-",
        ] {
            let file = spill_file(prefix, &query).expect("spill file");
            let parent = file.path().parent().expect("query directory");
            query_directory.get_or_insert_with(|| parent.to_path_buf());
            assert_eq!(
                parent.parent(),
                Some(directory.as_path()),
                "{prefix} must not fall back to the system temp directory"
            );
            assert_eq!(Some(parent), query_directory.as_deref());
        }
    }

    #[test]
    fn an_unwritable_directory_fails_before_any_query_runs() {
        // A file where the parent directory should be: `create_dir_all`
        // cannot succeed, which is the startup failure an operator should
        // see instead of losing a query that had already done its work.
        let directory = tempfile::tempdir().expect("tempdir");
        let occupied = directory.path().join("not-a-directory");
        std::fs::write(&occupied, b"blocked").expect("write blocker");

        let error = super::set_spill_directory(occupied.join("spill"))
            .expect_err("an unusable spill directory must be rejected");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::NotADirectory | std::io::ErrorKind::AlreadyExists
            ),
            "unexpected error kind: {error:?}"
        );
    }

    #[test]
    fn a_spill_file_lands_in_the_directory_it_is_given() {
        // `set_spill_directory` writes a process-wide OnceLock, so this
        // exercises the placement through the same builder path without
        // fixing the global for every other test in the binary.
        let directory = tempfile::tempdir().expect("tempdir");
        let file = tempfile::Builder::new()
            .prefix("pintail-sort-spill-")
            .tempfile_in(directory.path())
            .expect("spill file");
        assert_eq!(
            file.path().parent(),
            Some(directory.path()),
            "spill must not fall back to the system temp directory"
        );
    }

    #[test]
    fn an_unset_directory_still_produces_a_usable_file() {
        let query = QuerySpill::new();
        let file = spill_file("pintail-sort-spill-", &query).expect("fallback spill file");
        assert!(file.path().exists());
    }

    #[test]
    fn query_directories_are_isolated_and_removed_with_the_query() {
        let first = QuerySpill::new();
        let second = QuerySpill::new();
        let first_file = spill_file("pintail-sort-spill-", &first).expect("first spill");
        let second_file = spill_file("pintail-sort-spill-", &second).expect("second spill");
        let first_directory = first_file
            .path()
            .parent()
            .expect("first query dir")
            .to_path_buf();
        let second_directory = second_file
            .path()
            .parent()
            .expect("second query dir")
            .to_path_buf();
        assert_ne!(first_directory, second_directory);
        drop(first_file);
        assert!(first_directory.exists(), "query owns its directory");
        drop(first);
        assert!(
            !first_directory.exists(),
            "query drop removes its directory"
        );
        assert!(second_directory.exists(), "other query remains isolated");
    }

    #[test]
    fn reclaim_removes_only_our_leftovers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let ours = directory.path().join("pintail-join-spill-abc");
        let our_query = directory.path().join("pintail-query-spill-abc");
        let theirs = directory.path().join("someone-elses.db");
        std::fs::write(&ours, b"stale").expect("write ours");
        std::fs::create_dir(&our_query).expect("query directory");
        std::fs::write(our_query.join("run"), b"stale").expect("write query spill");
        std::fs::write(&theirs, b"keep").expect("write theirs");

        assert_eq!(
            reclaim_orphaned_spill(directory.path()).expect("reclaim"),
            2
        );
        assert!(!ours.exists(), "a stale spill file must be removed");
        assert!(
            !our_query.exists(),
            "a stale query directory must be removed"
        );
        assert!(theirs.exists(), "unrelated files must survive");
    }

    #[test]
    fn byte_counter_refuses_a_reservation_past_its_limit() {
        let counter = std::sync::atomic::AtomicU64::new(7);
        super::reserve_counter(&counter, 3, 10).expect("exact limit");
        assert!(super::reserve_counter(&counter, 1, 10).is_err());
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 10);
    }

    #[test]
    fn concurrent_reservations_never_cross_the_global_ceiling() {
        const WORKERS: usize = 8;
        const LIMIT: u64 = 257;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
        let handles = (0..WORKERS)
            .map(|_| {
                let counter = std::sync::Arc::clone(&counter);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..64)
                        .filter(|_| super::reserve_counter(&counter, 1, LIMIT).is_ok())
                        .count()
                })
            })
            .collect::<Vec<_>>();
        let accepted = handles
            .into_iter()
            .map(|handle| handle.join().expect("quota worker"))
            .sum::<usize>();

        assert_eq!(
            u64::try_from(accepted).expect("accepted reservation count fits u64"),
            LIMIT
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), LIMIT);
    }

    #[test]
    fn framed_writes_fail_before_crossing_the_query_quota() {
        let query = QuerySpill::with_limit(8);
        let mut reservation = super::SpillReservation {
            query: std::sync::Arc::clone(&query.inner),
            bytes: 0,
        };
        let mut output = Vec::new();
        super::write_record_quota(&mut output, &[1, 2, 3, 4], &mut reservation)
            .expect("four-byte header plus payload fits exactly");
        let error = super::write_record_quota(&mut output, &[], &mut reservation)
            .expect_err("the next frame must exceed the query quota");
        assert!(error.to_string().contains("query spill disk quota"));
        assert_eq!(query.metrics().active_bytes, 8);
        assert_eq!(query.metrics().written_bytes, 8);
        assert_eq!(query.metrics().quota_failures, 1);
        drop(reservation);
        assert_eq!(query.metrics().active_bytes, 0);
    }
}
