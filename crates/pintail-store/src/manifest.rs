use std::{fs::OpenOptions, io::Write, path::Path};

use pintail_types::{KeyMode, TableSchema};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder, decode_key, encode_key},
    segment::{
        ColumnSma, SegmentMeta, SegmentSmas, SmaExtremes, SmaSum, schema_fingerprint,
        sync_directory,
    },
};

pub(crate) const FILE_NAME: &str = "manifest.ptm";
const MAGIC: &[u8; 5] = b"PTMAN";
/// v2 adds optional per-segment SMAs; v1 manifests still decode (their
/// segments carry no SMAs and decline the aggregate fast path).
const FORMAT_VERSION: u8 = 3;

#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub(crate) generation: u64,
    pub(crate) schema_version: u32,
    pub(crate) schema_fingerprint: u64,
    pub(crate) key_mode: KeyMode,
    pub(crate) flushed_sequence: u64,
    pub(crate) next_segment_id: u64,
    pub(crate) epoch: u64,
    /// Highest durably committed local-transaction version (format v3;
    /// zero for replicated tables and older manifests).
    pub(crate) committed_version: u64,
    pub(crate) segments: Vec<SegmentMeta>,
}

impl Manifest {
    pub(crate) fn empty(schema: &TableSchema) -> Self {
        Self {
            generation: 0,
            schema_version: schema.version(),
            schema_fingerprint: schema_fingerprint(schema),
            key_mode: schema.key_mode(),
            flushed_sequence: 0,
            next_segment_id: 1,
            epoch: 0,
            committed_version: 0,
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
    let format_version = decoder
        .u8()
        .map_err(|reason| corrupt_here(&decoder, reason))?;
    if !(1..=FORMAT_VERSION).contains(&format_version) {
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
    let key_mode = decode_key_mode(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(&decoder, reason))?,
    )
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
    let committed_version = if format_version >= 3 {
        decoder
            .u64()
            .map_err(|reason| corrupt_here(&decoder, reason))?
    } else {
        0
    };
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
        let min_key = decode_key(&mut decoder).map_err(|reason| corrupt_here(&decoder, reason))?;
        let max_key = decode_key(&mut decoder).map_err(|reason| corrupt_here(&decoder, reason))?;
        let bloom = decoder
            .bytes()
            .map_err(|reason| corrupt_here(&decoder, reason))?
            .to_vec();
        let unique_keys = match decoder
            .u8()
            .map_err(|reason| corrupt_here(&decoder, reason))?
        {
            0 => false,
            1 => true,
            value => {
                return Err(corrupt_here(
                    &decoder,
                    format!("invalid unique-key flag {value}"),
                ));
            }
        };
        let smas = if format_version >= 2 {
            decode_smas(&mut decoder)?
        } else {
            None
        };
        segments.push(SegmentMeta {
            id,
            file_name,
            row_count,
            min_version,
            max_version,
            schema_fingerprint: segment_fingerprint,
            min_key,
            max_key,
            bloom,
            unique_keys,
            smas,
        });
    }
    decoder
        .finish()
        .map_err(|reason| StoreError::corrupt_manifest(checksum_offset, reason))?;

    if schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: schema_version,
        });
    }
    let expected_fingerprint = schema_fingerprint(schema);
    if schema_version == schema.version() && stored_fingerprint != expected_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: expected_fingerprint,
            actual: stored_fingerprint,
        });
    }
    if key_mode != schema.key_mode() {
        return Err(StoreError::IncompatibleSchema(
            "table key mode cannot change after data is created".into(),
        ));
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
        key_mode,
        flushed_sequence,
        next_segment_id,
        epoch,
        committed_version,
        segments,
    })
}

pub(crate) fn publish(directory: &Path, manifest: &Manifest) -> Result<(), StoreError> {
    let bytes = encode(manifest)?;
    let temporary = directory.join(".manifest.ptm.tmp");
    let destination = directory.join(FILE_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| StoreError::io("create temporary manifest", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| StoreError::io("write temporary manifest", error))?;
    std::fs::rename(&temporary, &destination)
        .map_err(|error| StoreError::io("publish manifest", error))?;
    sync_directory(directory)
}

pub(crate) fn encode(manifest: &Manifest) -> Result<Vec<u8>, StoreError> {
    let mut encoder = Encoder::new();
    encoder.raw(MAGIC);
    encoder.u8(FORMAT_VERSION);
    encoder.u64(manifest.generation);
    encoder.u32(manifest.schema_version);
    encoder.u64(manifest.schema_fingerprint);
    encoder.u8(encode_key_mode(manifest.key_mode));
    encoder.u64(manifest.flushed_sequence);
    encoder.u64(manifest.next_segment_id);
    encoder.u64(manifest.epoch);
    encoder.u64(manifest.committed_version);
    encoder.length(manifest.segments.len(), "manifest segment count")?;
    for segment in &manifest.segments {
        encoder.u64(segment.id);
        encoder.bytes(segment.file_name.as_bytes(), "segment file name")?;
        encoder.u64(segment.row_count);
        encoder.u64(segment.min_version);
        encoder.u64(segment.max_version);
        encoder.u64(segment.schema_fingerprint);
        encode_key(&mut encoder, &segment.min_key)?;
        encode_key(&mut encoder, &segment.max_key)?;
        encoder.bytes(&segment.bloom, "segment primary-key bloom")?;
        encoder.u8(u8::from(segment.unique_keys));
        encode_smas(&mut encoder, segment.smas.as_ref())?;
    }
    let checksum = xxh3_64(encoder.as_slice());
    encoder.u64(checksum);
    Ok(encoder.finish())
}

fn corrupt_here(decoder: &Decoder<'_>, reason: impl Into<String>) -> StoreError {
    StoreError::corrupt_manifest(decoder.position(), reason)
}

fn encode_i128(encoder: &mut Encoder, value: i128) {
    #[allow(clippy::cast_sign_loss)]
    let bits = value as u128;
    #[allow(clippy::cast_possible_truncation)]
    encoder.u64(bits as u64);
    encoder.u64((bits >> 64) as u64);
}

fn decode_i128(decoder: &mut Decoder<'_>) -> Result<i128, StoreError> {
    let low = decoder
        .u64()
        .map_err(|reason| corrupt_here(decoder, reason))?;
    let high = decoder
        .u64()
        .map_err(|reason| corrupt_here(decoder, reason))?;
    #[allow(clippy::cast_possible_wrap)]
    Ok(((u128::from(high) << 64) | u128::from(low)) as i128)
}

fn encode_smas(encoder: &mut Encoder, smas: Option<&SegmentSmas>) -> Result<(), StoreError> {
    let Some(smas) = smas else {
        encoder.u8(0);
        return Ok(());
    };
    encoder.u8(1);
    encoder.u64(smas.live_rows);
    encoder.u64(smas.tombstones);
    encoder.length(smas.columns.len(), "segment SMA column count")?;
    for column in &smas.columns {
        encoder.u32(column.column_id);
        encoder.u64(column.non_null);
        match column.sum {
            None => encoder.u8(0),
            Some(SmaSum::Int(total)) => {
                encoder.u8(1);
                encode_i128(encoder, total);
            }
            Some(SmaSum::Float(total)) => {
                encoder.u8(2);
                encoder.u64(total.to_bits());
            }
            Some(SmaSum::DecimalUnits { units, scale }) => {
                encoder.u8(3);
                encode_i128(encoder, units);
                encoder.u8(scale);
            }
        }
        match column.extremes {
            None => encoder.u8(0),
            Some(SmaExtremes::Int { min, max }) => {
                encoder.u8(1);
                #[allow(clippy::cast_sign_loss)]
                {
                    encoder.u64(min as u64);
                    encoder.u64(max as u64);
                }
            }
            Some(SmaExtremes::UInt { min, max }) => {
                encoder.u8(2);
                encoder.u64(min);
                encoder.u64(max);
            }
            Some(SmaExtremes::Float { min, max }) => {
                encoder.u8(3);
                encoder.u64(min.to_bits());
                encoder.u64(max.to_bits());
            }
            Some(SmaExtremes::DecimalUnits { min, max, scale }) => {
                encoder.u8(4);
                encode_i128(encoder, min);
                encode_i128(encoder, max);
                encoder.u8(scale);
            }
            Some(SmaExtremes::Temporal { min, max, units }) => {
                encoder.u8(5);
                #[allow(clippy::cast_sign_loss)]
                {
                    encoder.u64(min as u64);
                    encoder.u64(max as u64);
                }
                match units {
                    crate::segment::NativeUnits::Date => encoder.u8(0),
                    crate::segment::NativeUnits::DateTime { fsp } => {
                        encoder.u8(1);
                        encoder.u8(fsp);
                    }
                    crate::segment::NativeUnits::Decimal { .. } => {
                        return Err(StoreError::FormatLimit(
                            "decimal extremes use the DecimalUnits form".into(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn decode_smas(decoder: &mut Decoder<'_>) -> Result<Option<SegmentSmas>, StoreError> {
    match decoder
        .u8()
        .map_err(|reason| corrupt_here(decoder, reason))?
    {
        0 => return Ok(None),
        1 => {}
        tag => {
            return Err(corrupt_here(decoder, format!("invalid SMA presence {tag}")));
        }
    }
    let live_rows = decoder
        .u64()
        .map_err(|reason| corrupt_here(decoder, reason))?;
    let tombstones = decoder
        .u64()
        .map_err(|reason| corrupt_here(decoder, reason))?;
    let column_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(decoder, reason))?;
    let mut columns = Vec::with_capacity(column_count as usize);
    for _ in 0..column_count {
        let column_id = decoder
            .u32()
            .map_err(|reason| corrupt_here(decoder, reason))?;
        let non_null = decoder
            .u64()
            .map_err(|reason| corrupt_here(decoder, reason))?;
        let sum = match decoder
            .u8()
            .map_err(|reason| corrupt_here(decoder, reason))?
        {
            0 => None,
            1 => Some(SmaSum::Int(decode_i128(decoder)?)),
            2 => Some(SmaSum::Float(f64::from_bits(
                decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?,
            ))),
            3 => {
                let units = decode_i128(decoder)?;
                let scale = decoder
                    .u8()
                    .map_err(|reason| corrupt_here(decoder, reason))?;
                Some(SmaSum::DecimalUnits { units, scale })
            }
            tag => {
                return Err(corrupt_here(decoder, format!("invalid SMA sum tag {tag}")));
            }
        };
        let extremes = match decoder
            .u8()
            .map_err(|reason| corrupt_here(decoder, reason))?
        {
            0 => None,
            1 => {
                #[allow(clippy::cast_possible_wrap)]
                let min = decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?
                    as i64;
                #[allow(clippy::cast_possible_wrap)]
                let max = decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?
                    as i64;
                Some(SmaExtremes::Int { min, max })
            }
            2 => Some(SmaExtremes::UInt {
                min: decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?,
                max: decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?,
            }),
            3 => Some(SmaExtremes::Float {
                min: f64::from_bits(
                    decoder
                        .u64()
                        .map_err(|reason| corrupt_here(decoder, reason))?,
                ),
                max: f64::from_bits(
                    decoder
                        .u64()
                        .map_err(|reason| corrupt_here(decoder, reason))?,
                ),
            }),
            4 => {
                let min = decode_i128(decoder)?;
                let max = decode_i128(decoder)?;
                let scale = decoder
                    .u8()
                    .map_err(|reason| corrupt_here(decoder, reason))?;
                Some(SmaExtremes::DecimalUnits { min, max, scale })
            }
            5 => {
                #[allow(clippy::cast_possible_wrap)]
                let min = decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?
                    as i64;
                #[allow(clippy::cast_possible_wrap)]
                let max = decoder
                    .u64()
                    .map_err(|reason| corrupt_here(decoder, reason))?
                    as i64;
                let units = match decoder
                    .u8()
                    .map_err(|reason| corrupt_here(decoder, reason))?
                {
                    0 => crate::segment::NativeUnits::Date,
                    1 => crate::segment::NativeUnits::DateTime {
                        fsp: decoder
                            .u8()
                            .map_err(|reason| corrupt_here(decoder, reason))?,
                    },
                    tag => {
                        return Err(corrupt_here(
                            decoder,
                            format!("invalid temporal SMA unit tag {tag}"),
                        ));
                    }
                };
                Some(SmaExtremes::Temporal { min, max, units })
            }
            tag => {
                return Err(corrupt_here(
                    decoder,
                    format!("invalid SMA extremes tag {tag}"),
                ));
            }
        };
        columns.push(ColumnSma {
            column_id,
            non_null,
            sum,
            extremes,
        });
    }
    Ok(Some(SegmentSmas {
        live_rows,
        tombstones,
        columns,
    }))
}

fn encode_key_mode(key_mode: KeyMode) -> u8 {
    match key_mode {
        KeyMode::Primary => 0,
        KeyMode::Unique => 1,
        KeyMode::AppendRowId => 2,
    }
}

fn decode_key_mode(tag: u8) -> Result<KeyMode, String> {
    match tag {
        0 => Ok(KeyMode::Primary),
        1 => Ok(KeyMode::Unique),
        2 => Ok(KeyMode::AppendRowId),
        _ => Err(format!("unknown table key mode {tag}")),
    }
}
