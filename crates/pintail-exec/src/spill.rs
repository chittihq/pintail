//! Length-framed binary records for on-disk spill files.
//!
//! Sort runs, aggregation runs, and grace-join partitions all write rows the
//! query could not hold in memory, which at scale means tens of gigabytes.
//! A self-describing text format costs several times the bytes and spends
//! most of the decode budget on parsing, so spill records carry a compact
//! tagged encoding instead. The format is private to one query's temporary
//! files: nothing persists it, and no reader outside this process sees it.

use std::io::{Read, Write};

use pintail_types::{Float64, Value};

const TAG_NULL: u8 = 0;
const TAG_BOOLEAN: u8 = 1;
const TAG_INT64: u8 = 2;
const TAG_UINT64: u8 = 3;
const TAG_FLOAT64: u8 = 4;
const TAG_UTF8: u8 = 5;
const TAG_BINARY: u8 = 6;

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
        ]
    }

    #[test]
    fn every_value_variant_survives_a_round_trip() {
        let mut encoder = Encoder::new();
        encoder.values(&sample());
        let bytes = encoder.finish();
        let mut decoder = Decoder::new(&bytes);
        assert_eq!(decoder.values().expect("decode"), sample());
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
static SPILL_DIRECTORY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Sets the directory spill files are created in. Called once during
/// startup; later calls are ignored so a running process cannot have spill
/// files stranded across two directories.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or written to, so
/// an unusable location fails at startup rather than mid-query.
pub fn set_spill_directory(directory: std::path::PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(&directory)?;
    // Prove writability now: discovering it at spill time means failing a
    // query that had already done all of its work.
    tempfile::Builder::new()
        .prefix("pintail-spill-probe-")
        .tempfile_in(&directory)?;
    let _ = SPILL_DIRECTORY.set(directory);
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
pub fn reclaim_orphaned_spill(directory: &std::path::Path) -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("pintail-"))
            && entry.file_type().is_ok_and(|kind| kind.is_file())
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Creates one spill file in the configured directory, falling back to the
/// system temp directory when startup never set one (tests, embedded use).
pub(crate) fn spill_file(prefix: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let builder = &mut tempfile::Builder::new();
    let builder = builder.prefix(prefix);
    match SPILL_DIRECTORY.get() {
        Some(directory) => builder.tempfile_in(directory),
        None => builder.tempfile(),
    }
}

#[cfg(test)]
mod directory_tests {
    use super::{reclaim_orphaned_spill, spill_file};

    /// The three operators that spill — sort runs, aggregation runs and
    /// grace-join partitions — all reach disk through `spill_file`, so
    /// asserting each prefix lands in the configured directory covers every
    /// path without running three spilling queries. `set_spill_directory`
    /// writes a process-wide `OnceLock`, so this is the only test in the
    /// binary permitted to set it.
    #[test]
    fn every_spill_path_lands_in_the_configured_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        super::set_spill_directory(directory.path().to_path_buf()).expect("configure");

        for prefix in [
            "pintail-sort-spill-",
            "pintail-aggregate-spill-",
            "pintail-join-spill-",
        ] {
            let file = spill_file(prefix).expect("spill file");
            assert_eq!(
                file.path().parent(),
                Some(directory.path()),
                "{prefix} must not fall back to the system temp directory"
            );
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
        let file = spill_file("pintail-sort-spill-").expect("fallback spill file");
        assert!(file.path().exists());
    }

    #[test]
    fn reclaim_removes_only_our_leftovers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let ours = directory.path().join("pintail-join-spill-abc");
        let theirs = directory.path().join("someone-elses.db");
        std::fs::write(&ours, b"stale").expect("write ours");
        std::fs::write(&theirs, b"keep").expect("write theirs");

        assert_eq!(
            reclaim_orphaned_spill(directory.path()).expect("reclaim"),
            1
        );
        assert!(!ours.exists(), "a stale spill file must be removed");
        assert!(theirs.exists(), "unrelated files must survive");
    }
}
