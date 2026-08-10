use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use pintail_types::{DataType, StoredRow, TableSchema};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder, decode_row, encode_row},
    store::WalSync,
};

const MAGIC: &[u8; 5] = b"PTWAL";
const FORMAT_VERSION: u8 = 1;
const HEADER_LENGTH: usize = MAGIC.len() + 1;
const CHECKSUM_LENGTH: usize = size_of::<u64>();
const MAX_RECORD_LENGTH: usize = 128 * 1024 * 1024;
/// Reserved `table_id` marking a transaction-commit record; real tables
/// never carry this id.
const COMMIT_TABLE_SENTINEL: u64 = u64::MAX;

pub(crate) struct Wal {
    file: File,
    sync_policy: WalSync,
    #[cfg(test)]
    fail_append_after_bytes: Option<usize>,
}

pub(crate) struct Recovery {
    pub(crate) batches: Vec<RecoveredBatch>,
    pub(crate) last_sequence: u64,
    /// The last durable commit record, when the WAL carries transactions.
    pub(crate) last_commit: Option<WalCommit>,
}

/// One recovered transaction-commit marker.
#[derive(Clone, Copy)]
pub(crate) struct WalCommit {
    /// Batches (by count, in order) covered by this commit.
    pub(crate) batches: usize,
    /// The committed transaction version.
    pub(crate) version: u64,
    /// File offset one past the commit record, for truncating
    /// uncommitted tails.
    pub(crate) end_offset: u64,
}

pub(crate) struct RecoveredBatch {
    pub(crate) sequence: u64,
    pub(crate) table_id: u64,
    pub(crate) columns: Vec<WalColumn>,
    pub(crate) rows: Vec<StoredRow>,
}

pub(crate) struct WalColumn {
    pub(crate) id: u32,
    pub(crate) data_type: DataType,
}

impl Wal {
    pub(crate) fn open(path: &Path, sync_policy: WalSync) -> Result<(Self, Recovery), StoreError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| StoreError::io(format!("open WAL {}", path.display()), error))?;

        if file
            .metadata()
            .map_err(|error| StoreError::io("inspect WAL", error))?
            .len()
            == 0
        {
            file.write_all(MAGIC)
                .and_then(|()| file.write_all(&[FORMAT_VERSION]))
                .and_then(|()| file.sync_all())
                .map_err(|error| StoreError::io("initialize WAL", error))?;
        }

        let recovery = recover(&mut file, true)?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| StoreError::io("seek to WAL end", error))?;
        Ok((
            Self {
                file,
                sync_policy,
                #[cfg(test)]
                fail_append_after_bytes: None,
            },
            recovery,
        ))
    }

    pub(crate) fn append(
        &mut self,
        sequence: u64,
        table_id: u64,
        schema: &TableSchema,
        rows: &[StoredRow],
    ) -> Result<(), StoreError> {
        let payload = encode_batch(sequence, table_id, schema, rows)?;
        let length = u32::try_from(payload.len())
            .map_err(|_| StoreError::FormatLimit("WAL record exceeds u32::MAX".into()))?;
        let checksum = xxh3_64(&payload);

        let record_offset = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|error| StoreError::io("seek to WAL end", error))?;
        #[cfg(test)]
        let write_result = if let Some(remaining) = self.fail_append_after_bytes.take() {
            write_record(
                &mut StorageFullWriter {
                    file: &mut self.file,
                    remaining,
                },
                length,
                &payload,
                checksum,
            )
        } else {
            write_record(&mut self.file, length, &payload, checksum)
        };
        #[cfg(not(test))]
        let write_result = write_record(&mut self.file, length, &payload, checksum);
        if let Err(write_error) = write_result {
            self.rollback_failed_append(record_offset, &write_error)?;
            return Err(StoreError::io("append WAL record", write_error));
        }
        if self.sync_policy == WalSync::Always
            && let Err(sync_error) = self.file.sync_data()
        {
            self.rollback_failed_append(record_offset, &sync_error)?;
            return Err(StoreError::io("sync WAL append", sync_error));
        }
        Ok(())
    }

    fn rollback_failed_append(
        &mut self,
        record_offset: u64,
        write_error: &std::io::Error,
    ) -> Result<(), StoreError> {
        self.file
            .set_len(record_offset)
            .and_then(|()| self.file.seek(SeekFrom::Start(record_offset)).map(drop))
            .and_then(|()| {
                if self.sync_policy == WalSync::Off {
                    Ok(())
                } else {
                    self.file.sync_all()
                }
            })
            .map_err(|rollback_error| {
                StoreError::io(
                    format!("roll back failed WAL append after write error: {write_error}"),
                    rollback_error,
                )
            })
    }

    /// Appends a transaction-commit record covering every batch before it.
    pub(crate) fn append_commit(
        &mut self,
        sequence: u64,
        commit_version: u64,
    ) -> Result<(), StoreError> {
        let mut encoder = Encoder::new();
        encoder.u64(sequence);
        encoder.u64(COMMIT_TABLE_SENTINEL);
        encoder.u64(commit_version);
        let payload = encoder.finish();
        let length = u32::try_from(payload.len()).expect("commit records are a handful of bytes");
        let checksum = xxh3_64(&payload);
        let record_offset = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|error| StoreError::io("seek to WAL end", error))?;
        if let Err(write_error) = write_record(&mut self.file, length, &payload, checksum) {
            self.rollback_failed_append(record_offset, &write_error)?;
            return Err(StoreError::io("append WAL commit record", write_error));
        }
        Ok(())
    }

    /// Synchronizes unconditionally: transaction commits are durable at
    /// every policy, unlike per-batch appends.
    pub(crate) fn sync_force(&mut self) -> Result<(), StoreError> {
        self.file
            .sync_data()
            .map_err(|error| StoreError::io("sync WAL commit", error))
    }

    /// Truncates the log to `offset`, discarding uncommitted tail records.
    pub(crate) fn truncate_to(&mut self, offset: u64) -> Result<(), StoreError> {
        self.file
            .set_len(offset)
            .and_then(|()| self.file.seek(SeekFrom::Start(offset)).map(drop))
            .and_then(|()| self.file.sync_all())
            .map_err(|error| StoreError::io("truncate uncommitted WAL tail", error))
    }

    pub(crate) fn sync(&mut self) -> Result<(), StoreError> {
        if self.sync_policy != WalSync::Off {
            self.file
                .sync_data()
                .map_err(|error| StoreError::io("sync WAL", error))?;
        }
        Ok(())
    }

    pub(crate) fn reset(&mut self) -> Result<(), StoreError> {
        self.file
            .set_len(HEADER_LENGTH as u64)
            .and_then(|()| self.file.seek(SeekFrom::Start(HEADER_LENGTH as u64)))
            .map_err(|error| StoreError::io("truncate flushed WAL", error))?;
        if self.sync_policy != WalSync::Off {
            self.file
                .sync_all()
                .map_err(|error| StoreError::io("sync truncated WAL", error))?;
        }
        Ok(())
    }
}

pub(crate) fn recover_read_only(path: &Path) -> Result<Recovery, StoreError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Recovery {
                batches: Vec::new(),
                last_sequence: 0,
                last_commit: None,
            });
        }
        Err(error) => {
            return Err(StoreError::io(
                format!("open WAL reader {}", path.display()),
                error,
            ));
        }
    };
    recover(&mut file, false)
}

#[cfg(test)]
struct StorageFullWriter<'a> {
    file: &'a mut File,
    remaining: usize,
}

#[cfg(test)]
impl Write for StorageFullWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "simulated disk full",
            ));
        }
        let written = buffer.len().min(self.remaining);
        let written = self.file.write(&buffer[..written])?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn write_record(
    writer: &mut impl Write,
    length: u32,
    payload: &[u8],
    checksum: u64,
) -> std::io::Result<()> {
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.write_all(&checksum.to_le_bytes())
}

fn encode_batch(
    sequence: u64,
    table_id: u64,
    schema: &TableSchema,
    rows: &[StoredRow],
) -> Result<Vec<u8>, StoreError> {
    let mut encoder = Encoder::new();
    encoder.u64(sequence);
    encoder.u64(table_id);
    encoder.u32(schema.version());
    encoder.length(schema.columns().len(), "WAL schema column count")?;
    for column in schema.columns() {
        encoder.u32(column.id());
        encoder.u8(encode_data_type(column.data_type()));
    }
    encoder.length(rows.len(), "WAL batch row count")?;
    for row in rows {
        encode_row(&mut encoder, row)?;
    }
    Ok(encoder.finish())
}

/// A commit record's `(sequence, version)`, or `None` for batch payloads.
fn decode_commit(payload: &[u8]) -> Option<(u64, u64)> {
    if payload.len() != 3 * size_of::<u64>() {
        return None;
    }
    let mut decoder = Decoder::new(payload);
    let sequence = decoder.u64().ok()?;
    if decoder.u64().ok()? != COMMIT_TABLE_SENTINEL {
        return None;
    }
    let version = decoder.u64().ok()?;
    decoder.finish().ok()?;
    Some((sequence, version))
}

fn decode_batch(payload: &[u8]) -> Result<RecoveredBatch, String> {
    let mut decoder = Decoder::new(payload);
    let sequence = decoder.u64()?;
    let table_id = decoder.u64()?;
    let _schema_version = decoder.u32()?;
    let column_count = decoder.u32()?;
    let mut columns = Vec::with_capacity(column_count as usize);
    for _ in 0..column_count {
        columns.push(WalColumn {
            id: decoder.u32()?,
            data_type: decode_data_type(decoder.u8()?)?,
        });
    }
    let row_count = decoder.u32()?;
    let mut rows = Vec::with_capacity(row_count as usize);
    for _ in 0..row_count {
        let row = decode_row(&mut decoder)?;
        if row.values().len() != columns.len() {
            return Err(format!(
                "row has {} values for {} WAL schema columns",
                row.values().len(),
                columns.len()
            ));
        }
        for (column, value) in columns.iter().zip(row.values()) {
            if value
                .data_type()
                .is_some_and(|data_type| !column.data_type.accepts(data_type))
            {
                return Err(format!(
                    "row value does not match WAL column {} type",
                    column.id
                ));
            }
        }
        rows.push(row);
    }
    decoder.finish()?;
    Ok(RecoveredBatch {
        sequence,
        table_id,
        columns,
        rows,
    })
}

fn encode_data_type(data_type: DataType) -> u8 {
    match data_type.storage_type() {
        DataType::Boolean => 0,
        DataType::Int64 => 1,
        DataType::UInt64 => 2,
        DataType::Float64 => 3,
        DataType::Utf8 => 4,
        DataType::Binary => 5,
        _ => unreachable!("storage_type returns a physical scalar type"),
    }
}

fn decode_data_type(tag: u8) -> Result<DataType, String> {
    match tag {
        0 => Ok(DataType::Boolean),
        1 => Ok(DataType::Int64),
        2 => Ok(DataType::UInt64),
        3 => Ok(DataType::Float64),
        4 => Ok(DataType::Utf8),
        5 => Ok(DataType::Binary),
        _ => Err(format!("unknown WAL column type {tag}")),
    }
}

#[allow(clippy::too_many_lines)] // one linear record walk
fn recover(file: &mut File, truncate_torn_tail: bool) -> Result<Recovery, StoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| StoreError::io("seek to WAL start", error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| StoreError::io("read WAL", error))?;

    if bytes.len() < HEADER_LENGTH
        || &bytes[..MAGIC.len()] != MAGIC
        || bytes[MAGIC.len()] != FORMAT_VERSION
    {
        return Err(StoreError::corrupt_wal(
            0,
            "invalid magic or format version",
        ));
    }

    let mut position = HEADER_LENGTH;
    let mut valid_length = HEADER_LENGTH;
    let mut batches = Vec::new();
    let mut last_sequence = 0;
    let mut last_commit = None;
    while position < bytes.len() {
        let record_offset = position;
        let Some(length_bytes) = bytes.get(position..position + size_of::<u32>()) else {
            break;
        };
        let length = u32::from_le_bytes(
            length_bytes
                .try_into()
                .map_err(|_| StoreError::corrupt_wal(position, "invalid record length"))?,
        ) as usize;
        position += size_of::<u32>();

        let Some(record_end) = position
            .checked_add(length)
            .and_then(|end| end.checked_add(CHECKSUM_LENGTH))
        else {
            return Err(StoreError::corrupt_wal(
                record_offset,
                "record length overflow",
            ));
        };
        if record_end > bytes.len() {
            break;
        }
        if length > MAX_RECORD_LENGTH {
            return Err(StoreError::corrupt_wal(
                record_offset,
                format!("record length {length} exceeds limit"),
            ));
        }

        let payload = &bytes[position..position + length];
        position += length;
        let expected = u64::from_le_bytes(
            bytes[position..record_end]
                .try_into()
                .map_err(|_| StoreError::corrupt_wal(position, "invalid checksum length"))?,
        );
        if xxh3_64(payload) != expected {
            return Err(StoreError::corrupt_wal(
                record_offset,
                "record checksum mismatch",
            ));
        }
        position = record_end;

        if let Some((sequence, version)) = decode_commit(payload) {
            if sequence <= last_sequence {
                return Err(StoreError::corrupt_wal(
                    record_offset,
                    format!("sequence {sequence} does not follow {last_sequence}"),
                ));
            }
            last_sequence = sequence;
            valid_length = position;
            last_commit = Some(WalCommit {
                batches: batches.len(),
                version,
                end_offset: valid_length as u64,
            });
            continue;
        }
        let batch = decode_batch(payload).map_err(|reason| {
            StoreError::corrupt_wal(record_offset, format!("invalid record payload: {reason}"))
        })?;
        let sequence = batch.sequence;
        if sequence <= last_sequence {
            return Err(StoreError::corrupt_wal(
                record_offset,
                format!("sequence {sequence} does not follow {last_sequence}"),
            ));
        }
        last_sequence = sequence;
        batches.push(batch);
        valid_length = position;
    }

    if truncate_torn_tail && valid_length != bytes.len() {
        file.set_len(valid_length as u64)
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::io("truncate torn WAL tail", error))?;
    }

    Ok(Recovery {
        batches,
        last_sequence,
        last_commit,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind, Seek, SeekFrom, Write};

    use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
    use xxhash_rust::xxh3::xxh3_64;

    use crate::{StoreError, store::WalSync};

    use super::{FORMAT_VERSION, MAGIC, Wal, encode_batch, recover, write_record};

    #[test]
    fn disk_full_during_wal_append_leaves_the_prior_complete_prefix() {
        let schema = TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)])
            .expect("schema");
        let first = encode_batch(1, 7, &schema, &[row("durable", 1)]).expect("first payload");
        let second =
            encode_batch(2, 7, &schema, &[row(&"x".repeat(1024), 2)]).expect("second payload");
        let mut bytes = [MAGIC.as_slice(), &[FORMAT_VERSION]].concat();
        append_complete(&mut bytes, &first);
        let valid_length = bytes.len();

        let mut limited = FullAfter {
            bytes: &mut bytes,
            remaining: 37,
        };
        let error = write_record(
            &mut limited,
            u32::try_from(second.len()).expect("payload length"),
            &second,
            xxh3_64(&second),
        )
        .expect_err("simulated disk must fill during the record");
        assert_eq!(error.kind(), ErrorKind::StorageFull);
        assert!(
            bytes.len() > valid_length,
            "a torn record prefix was written"
        );

        let mut file = tempfile::tempfile().expect("temporary WAL");
        file.write_all(&bytes).expect("write simulated WAL");
        file.seek(SeekFrom::Start(0)).expect("seek WAL");
        let recovery = recover(&mut file, true).expect("recover complete prefix");
        assert_eq!(recovery.batches.len(), 1);
        assert_eq!(recovery.batches[0].sequence, 1);
        assert_eq!(
            file.metadata().expect("WAL metadata").len(),
            u64::try_from(valid_length).expect("valid length")
        );
    }

    #[test]
    fn a_failed_append_is_rolled_back_before_the_live_handle_retries() {
        let directory = tempfile::tempdir().expect("temporary WAL directory");
        let path = directory.path().join("database.wal");
        let schema = TableSchema::new(1, vec![Column::new(1, "value", DataType::Utf8, false)])
            .expect("schema");
        let (mut wal, _) = Wal::open(&path, WalSync::Always).expect("open WAL");
        wal.fail_append_after_bytes = Some(19);
        let error = wal
            .append(1, 7, &schema, &[row("partial", 1)])
            .expect_err("injected append must fail");
        assert!(matches!(
            error,
            StoreError::Io { source, .. } if source.kind() == ErrorKind::StorageFull
        ));
        assert_eq!(wal.file.metadata().expect("WAL metadata").len(), 6);
        wal.append(1, 7, &schema, &[row("retry", 1)])
            .expect("retry append");
        drop(wal);

        let (_, recovery) = Wal::open(&path, WalSync::Always).expect("recover retry");
        assert_eq!(recovery.batches.len(), 1);
        assert_eq!(recovery.batches[0].sequence, 1);
    }

    fn append_complete(bytes: &mut Vec<u8>, payload: &[u8]) {
        write_record(
            bytes,
            u32::try_from(payload.len()).expect("payload length"),
            payload,
            xxh3_64(payload),
        )
        .expect("append complete record");
    }

    fn row(value: &str, version: u64) -> StoredRow {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(1)]).expect("key"),
            vec![Value::Utf8(value.into())],
            version,
            false,
        )
    }

    struct FullAfter<'a> {
        bytes: &'a mut Vec<u8>,
        remaining: usize,
    }

    impl Write for FullAfter<'_> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(Error::new(ErrorKind::StorageFull, "simulated disk full"));
            }
            let written = buffer.len().min(self.remaining);
            self.bytes.extend_from_slice(&buffer[..written]);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
