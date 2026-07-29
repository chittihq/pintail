use std::{fs::OpenOptions, io::Write, path::Path};

use pintail_types::TableSchema;
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder},
    segment::{SegmentMeta, schema_fingerprint, sync_directory},
};

pub(crate) const FILE_NAME: &str = "manifest.ptm";
const MAGIC: &[u8; 5] = b"PTMAN";
const FORMAT_VERSION: u8 = 1;

#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub(crate) generation: u64,
    pub(crate) schema_version: u32,
    pub(crate) schema_fingerprint: u64,
    pub(crate) flushed_sequence: u64,
    pub(crate) next_segment_id: u64,
    pub(crate) epoch: u64,
    pub(crate) segments: Vec<SegmentMeta>,
}

impl Manifest {
    pub(crate) fn empty(schema: &TableSchema) -> Self {
        Self {
            generation: 0,
            schema_version: schema.version(),
            schema_fingerprint: schema_fingerprint(schema),
            flushed_sequence: 0,
            next_segment_id: 1,
            epoch: 0,
            segments: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn load(directory: &Path, schema: &TableSchema) -> Result<Manifest, StoreError> {
    let path = directory.join(FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Manifest::empty(schema));
        }
        Err(error) => {
            return Err(StoreError::io(
                format!("read manifest {}", path.display()),
                error,
            ));
        }
    };
    if bytes.len() < MAGIC.len() + 1 + size_of::<u64>() {
        return Err(StoreError::corrupt_manifest(0, "manifest is too short"));
    }
    let checksum_offset = bytes.len() - size_of::<u64>();
    let expected = u64::from_le_bytes(
        bytes[checksum_offset..]
            .try_into()
            .map_err(|_| StoreError::corrupt_manifest(checksum_offset, "invalid checksum"))?,
    );
    if xxh3_64(&bytes[..checksum_offset]) != expected {
        return Err(StoreError::corrupt_manifest(
            checksum_offset,
            "checksum mismatch",
        ));
    }

    let mut decoder = Decoder::new(&bytes[..checksum_offset]);
    if decoder
        .take(MAGIC.len())
        .map_err(|reason| corrupt_here(&decoder, reason))?
        != MAGIC
    {
        return Err(StoreError::corrupt_manifest(0, "invalid magic"));
    }
    if decoder
        .u8()
        .map_err(|reason| corrupt_here(&decoder, reason))?
        != FORMAT_VERSION
    {
        return Err(corrupt_here(&decoder, "unsupported format version"));
    }
    let generation = decoder
        .u64()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let schema_version = decoder
        .u32()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let stored_fingerprint = decoder
        .u64()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let flushed_sequence = decoder
        .u64()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let next_segment_id = decoder
        .u64()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let epoch = decoder
        .u64()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let segment_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    let mut segments = Vec::with_capacity(segment_count as usize);
    for _ in 0..segment_count {
        let id = decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?;
        let file_name = std::str::from_utf8(
            decoder
                .bytes()
                .map_err(|reason| corrupt_here(&decoder, reason))?,
        )
        .map(str::to_owned)
        .map_err(|error| corrupt_here(&decoder, format!("invalid segment file name: {error}")))?;
        let row_count = decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?;
        let min_version = decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?;
        let max_version = decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?;
        let segment_fingerprint = decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?;
        segments.push(SegmentMeta {
            id,
            file_name,
            row_count,
            min_version,
            max_version,
            schema_fingerprint: segment_fingerprint,
        });
    }
    decoder
        .finish()
        .map_err(|reason| StoreError::corrupt_manifest(checksum_offset, reason))?;

    if schema_version != schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: schema_version,
        });
    }
    let expected_fingerprint = schema_fingerprint(schema);
    if stored_fingerprint != expected_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: expected_fingerprint,
            actual: stored_fingerprint,
        });
    }
    if next_segment_id == 0 || segments.iter().any(|segment| segment.id >= next_segment_id) {
        return Err(StoreError::corrupt_manifest(
            0,
            "next segment id does not follow published segments",
        ));
    }

    Ok(Manifest {
        generation,
        schema_version,
        schema_fingerprint: stored_fingerprint,
        flushed_sequence,
        next_segment_id,
        epoch,
        segments,
    })
}

pub(crate) fn publish(directory: &Path, manifest: &Manifest) -> Result<(), StoreError> {
    let mut encoder = Encoder::new();
    encoder.raw(MAGIC);
    encoder.u8(FORMAT_VERSION);
    encoder.u64(manifest.generation);
    encoder.u32(manifest.schema_version);
    encoder.u64(manifest.schema_fingerprint);
    encoder.u64(manifest.flushed_sequence);
    encoder.u64(manifest.next_segment_id);
    encoder.u64(manifest.epoch);
    encoder.length(manifest.segments.len(), "manifest segment count")?;
    for segment in &manifest.segments {
        encoder.u64(segment.id);
        encoder.bytes(segment.file_name.as_bytes(), "segment file name")?;
        encoder.u64(segment.row_count);
        encoder.u64(segment.min_version);
        encoder.u64(segment.max_version);
        encoder.u64(segment.schema_fingerprint);
    }
    let checksum = xxh3_64(encoder.as_slice());
    encoder.u64(checksum);

    let temporary = directory.join(".manifest.ptm.tmp");
    let destination = directory.join(FILE_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| StoreError::io("create temporary manifest", error))?;
    file.write_all(&encoder.finish())
        .and_then(|()| file.sync_all())
        .map_err(|error| StoreError::io("write temporary manifest", error))?;
    std::fs::rename(&temporary, &destination)
        .map_err(|error| StoreError::io("publish manifest", error))?;
    sync_directory(directory)
}

fn corrupt_here(decoder: &Decoder<'_>, reason: impl Into<String>) -> StoreError {
    StoreError::corrupt_manifest(decoder.position(), reason)
}
