use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use pintail_types::StoredRow;
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

pub(crate) struct Wal {
    file: File,
    sync_policy: WalSync,
}

pub(crate) struct Recovery {
    pub(crate) batches: Vec<(u64, Vec<StoredRow>)>,
    pub(crate) last_sequence: u64,
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

        let recovery = recover(&mut file)?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| StoreError::io("seek to WAL end", error))?;
        Ok((Self { file, sync_policy }, recovery))
    }

    pub(crate) fn append(&mut self, sequence: u64, rows: &[StoredRow]) -> Result<(), StoreError> {
        let payload = encode_batch(sequence, rows)?;
        let length = u32::try_from(payload.len())
            .map_err(|_| StoreError::FormatLimit("WAL record exceeds u32::MAX".into()))?;
        let checksum = xxh3_64(&payload);

        self.file
            .write_all(&length.to_le_bytes())
            .and_then(|()| self.file.write_all(&payload))
            .and_then(|()| self.file.write_all(&checksum.to_le_bytes()))
            .map_err(|error| StoreError::io("append WAL record", error))?;
        if self.sync_policy == WalSync::Always {
            self.sync()?;
        }
        Ok(())
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

fn encode_batch(sequence: u64, rows: &[StoredRow]) -> Result<Vec<u8>, StoreError> {
    let mut encoder = Encoder::new();
    encoder.u64(sequence);
    encoder.length(rows.len(), "WAL batch row count")?;
    for row in rows {
        encode_row(&mut encoder, row)?;
    }
    Ok(encoder.finish())
}

fn decode_batch(payload: &[u8]) -> Result<(u64, Vec<StoredRow>), String> {
    let mut decoder = Decoder::new(payload);
    let sequence = decoder.u64()?;
    let row_count = decoder.u32()?;
    let mut rows = Vec::with_capacity(row_count as usize);
    for _ in 0..row_count {
        rows.push(decode_row(&mut decoder)?);
    }
    decoder.finish()?;
    Ok((sequence, rows))
}

fn recover(file: &mut File) -> Result<Recovery, StoreError> {
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

        let (sequence, rows) = decode_batch(payload).map_err(|reason| {
            StoreError::corrupt_wal(record_offset, format!("invalid record payload: {reason}"))
        })?;
        if sequence <= last_sequence {
            return Err(StoreError::corrupt_wal(
                record_offset,
                format!("sequence {sequence} does not follow {last_sequence}"),
            ));
        }
        last_sequence = sequence;
        batches.push((sequence, rows));
        valid_length = position;
    }

    if valid_length != bytes.len() {
        file.set_len(valid_length as u64)
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::io("truncate torn WAL tail", error))?;
    }

    Ok(Recovery {
        batches,
        last_sequence,
    })
}
