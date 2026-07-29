use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use lz4_flex::block::{compress, decompress};
use pintail_types::{DataType, PrimaryKey, StoredRow, TableSchema, Value};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder, decode_key, encode_key},
};

const MAGIC: &[u8; 5] = b"PTSEG";
const FOOTER_MAGIC: &[u8; 5] = b"PTFTR";
const FORMAT_VERSION: u8 = 1;
const ENCODING_PLAIN: u8 = 0;
const COMPRESSION_LZ4: u8 = 1;
const KEY_COLUMN_ID: u32 = u32::MAX - 2;
const VERSION_COLUMN_ID: u32 = u32::MAX - 1;
const TOMBSTONE_COLUMN_ID: u32 = u32::MAX;
const BLOOM_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct SegmentMeta {
    pub(crate) id: u64,
    pub(crate) file_name: String,
    pub(crate) row_count: u64,
    pub(crate) min_version: u64,
    pub(crate) max_version: u64,
    pub(crate) schema_fingerprint: u64,
}

#[derive(Clone, Copy)]
enum LogicalType {
    Boolean = 0,
    Int64 = 1,
    UInt64 = 2,
    Float64 = 3,
    Utf8 = 4,
    Binary = 5,
    PrimaryKey = 6,
}

impl LogicalType {
    fn from_data_type(data_type: DataType) -> Self {
        match data_type {
            DataType::Boolean => Self::Boolean,
            DataType::Int64 => Self::Int64,
            DataType::UInt64 => Self::UInt64,
            DataType::Float64 => Self::Float64,
            DataType::Utf8 => Self::Utf8,
            DataType::Binary => Self::Binary,
        }
    }

    fn decode(tag: u8) -> Result<Self, String> {
        match tag {
            0 => Ok(Self::Boolean),
            1 => Ok(Self::Int64),
            2 => Ok(Self::UInt64),
            3 => Ok(Self::Float64),
            4 => Ok(Self::Utf8),
            5 => Ok(Self::Binary),
            6 => Ok(Self::PrimaryKey),
            _ => Err(format!("unknown logical type {tag}")),
        }
    }
}

enum ColumnSource {
    Key,
    Version,
    Tombstone,
    Value(usize),
}

struct ColumnSpec {
    id: u32,
    logical_type: LogicalType,
    source: ColumnSource,
}

pub(crate) fn schema_fingerprint(schema: &TableSchema) -> u64 {
    let mut encoder = Encoder::new();
    encoder.u32(schema.version());
    encoder.u32(u32::try_from(schema.columns().len()).unwrap_or(u32::MAX));
    for column in schema.columns() {
        encoder.u32(column.id());
        encoder.u8(LogicalType::from_data_type(column.data_type()) as u8);
        encoder.u8(u8::from(column.is_nullable()));
        encoder.raw(column.name().as_bytes());
        encoder.u8(0);
    }
    xxh3_64(&encoder.finish())
}

pub(crate) fn write(
    directory: &Path,
    id: u64,
    schema: &TableSchema,
    rows: &[StoredRow],
    block_rows: usize,
) -> Result<SegmentMeta, StoreError> {
    if rows.is_empty() {
        return Err(StoreError::FormatLimit(
            "cannot write an empty segment".to_owned(),
        ));
    }
    if block_rows == 0 {
        return Err(StoreError::FormatLimit(
            "segment block row target must be non-zero".to_owned(),
        ));
    }

    let fingerprint = schema_fingerprint(schema);
    let min_version = rows.iter().map(StoredRow::version).min().unwrap_or(0);
    let max_version = rows.iter().map(StoredRow::version).max().unwrap_or(0);
    let specs = column_specs(schema);
    let mut encoder = Encoder::new();
    encoder.raw(MAGIC);
    encoder.u8(FORMAT_VERSION);
    encoder.u32(schema.version());
    encoder.u64(fingerprint);
    encoder.u64(rows.len() as u64);
    encoder.length(specs.len(), "segment column count")?;
    encoder.length(block_rows, "segment block row target")?;

    let mut column_offsets = Vec::with_capacity(specs.len());
    for spec in &specs {
        column_offsets.push(encoder.position() as u64);
        write_column(&mut encoder, spec, rows, block_rows)?;
    }

    let footer_offset = encoder.position() as u64;
    let footer_start = encoder.position();
    encoder.raw(FOOTER_MAGIC);
    encoder.u64(rows.len() as u64);
    encoder.u64(min_version);
    encoder.u64(max_version);
    encoder.u64(fingerprint);
    encoder.u64(rows.len() as u64);
    encode_key(&mut encoder, rows.first().expect("non-empty rows").key())?;
    encode_key(&mut encoder, rows.last().expect("non-empty rows").key())?;
    encoder.length(column_offsets.len(), "footer column count")?;
    for offset in column_offsets {
        encoder.u64(offset);
    }

    let sparse_count = rows.len().div_ceil(block_rows);
    encoder.length(sparse_count, "sparse primary-key index")?;
    for row_index in (0..rows.len()).step_by(block_rows) {
        encoder.u64(row_index as u64);
        encode_key(&mut encoder, rows[row_index].key())?;
    }
    encoder.bytes(&build_bloom(rows)?, "primary-key bloom filter")?;

    let footer_checksum = xxh3_64(&encoder.as_slice()[footer_start..]);
    encoder.u64(footer_checksum);
    encoder.u64(footer_offset);

    let file_name = format!("segment-{id:020}.ptseg");
    let path = directory.join(&file_name);
    let temporary = directory.join(format!(".{file_name}.tmp"));
    write_atomic(&temporary, &path, &encoder.finish())?;

    Ok(SegmentMeta {
        id,
        file_name,
        row_count: rows.len() as u64,
        min_version,
        max_version,
        schema_fingerprint: fingerprint,
    })
}

pub(crate) fn read(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<Vec<StoredRow>, StoreError> {
    let path = directory.join(&meta.file_name);
    let bytes = std::fs::read(&path)
        .map_err(|error| StoreError::io(format!("read segment {}", path.display()), error))?;
    validate_footer(&path, &bytes, meta, schema)?;

    let mut decoder = Decoder::new(&bytes);
    expect_raw(&mut decoder, MAGIC).map_err(|reason| corrupt(&path, 0, reason))?;
    if decoder
        .u8()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?
        != FORMAT_VERSION
    {
        return Err(corrupt(&path, MAGIC.len(), "unsupported format version"));
    }
    let schema_version = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    if schema_version != schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: schema_version,
        });
    }
    let fingerprint = decoder
        .u64()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    let expected_fingerprint = schema_fingerprint(schema);
    if fingerprint != expected_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: expected_fingerprint,
            actual: fingerprint,
        });
    }
    let row_count = usize::try_from(
        decoder
            .u64()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
    )
    .map_err(|_| corrupt_here(&path, &decoder, "row count does not fit usize"))?;
    let column_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))? as usize;
    let _block_rows = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;

    let specs = column_specs(schema);
    if column_count != specs.len() {
        return Err(corrupt_here(
            &path,
            &decoder,
            format!(
                "header declares {column_count} columns, expected {}",
                specs.len()
            ),
        ));
    }

    let mut keys = None;
    let mut versions = None;
    let mut tombstones = None;
    let mut values = vec![None; schema.columns().len()];
    for spec in &specs {
        let column_cells = read_column(&path, &mut decoder, spec, row_count)?;
        match spec.source {
            ColumnSource::Key => keys = Some(column_cells),
            ColumnSource::Version => versions = Some(column_cells),
            ColumnSource::Tombstone => tombstones = Some(column_cells),
            ColumnSource::Value(index) => values[index] = Some(column_cells),
        }
    }

    let keys = keys.ok_or_else(|| corrupt_here(&path, &decoder, "missing key column"))?;
    let versions =
        versions.ok_or_else(|| corrupt_here(&path, &decoder, "missing version column"))?;
    let tombstones =
        tombstones.ok_or_else(|| corrupt_here(&path, &decoder, "missing tombstone column"))?;
    let mut rows = Vec::with_capacity(row_count);
    for row_index in 0..row_count {
        let key = match &keys[row_index] {
            Cell::Key(key) => key.clone(),
            _ => return Err(corrupt_here(&path, &decoder, "invalid key cell")),
        };
        let Cell::UInt64(version) = versions[row_index] else {
            return Err(corrupt_here(&path, &decoder, "invalid version cell"));
        };
        let Cell::Boolean(deleted) = tombstones[row_index] else {
            return Err(corrupt_here(&path, &decoder, "invalid tombstone cell"));
        };
        let row_values = values
            .iter()
            .map(|column| {
                column
                    .as_ref()
                    .and_then(|column| column.get(row_index))
                    .map(Cell::to_value)
                    .ok_or_else(|| corrupt_here(&path, &decoder, "missing user value"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(StoredRow::new(key, row_values, version, deleted));
    }
    Ok(rows)
}

fn column_specs(schema: &TableSchema) -> Vec<ColumnSpec> {
    let mut specs = vec![
        ColumnSpec {
            id: KEY_COLUMN_ID,
            logical_type: LogicalType::PrimaryKey,
            source: ColumnSource::Key,
        },
        ColumnSpec {
            id: VERSION_COLUMN_ID,
            logical_type: LogicalType::UInt64,
            source: ColumnSource::Version,
        },
        ColumnSpec {
            id: TOMBSTONE_COLUMN_ID,
            logical_type: LogicalType::Boolean,
            source: ColumnSource::Tombstone,
        },
    ];
    specs.extend(
        schema
            .columns()
            .iter()
            .enumerate()
            .map(|(index, column)| ColumnSpec {
                id: column.id(),
                logical_type: LogicalType::from_data_type(column.data_type()),
                source: ColumnSource::Value(index),
            }),
    );
    specs
}

fn write_column(
    encoder: &mut Encoder,
    spec: &ColumnSpec,
    rows: &[StoredRow],
    block_rows: usize,
) -> Result<(), StoreError> {
    encoder.u32(spec.id);
    encoder.u8(spec.logical_type as u8);
    encoder.length(rows.len().div_ceil(block_rows), "column block count")?;
    for block in rows.chunks(block_rows) {
        let cells = block
            .iter()
            .map(|row| cell_for(spec, row))
            .collect::<Vec<_>>();
        write_block(encoder, spec.logical_type, &cells)?;
    }
    Ok(())
}

fn write_block(
    encoder: &mut Encoder,
    logical_type: LogicalType,
    cells: &[Cell],
) -> Result<(), StoreError> {
    encoder.length(cells.len(), "block row count")?;
    let mut null_bitmap = vec![0_u8; cells.len().div_ceil(8)];
    let mut plain = Encoder::new();
    let mut encoded_values = Vec::with_capacity(cells.len());
    let mut null_count = 0_u32;
    for (index, cell) in cells.iter().enumerate() {
        if matches!(cell, Cell::Null) {
            null_bitmap[index / 8] |= 1 << (index % 8);
            null_count += 1;
        } else {
            encode_cell(&mut plain, cell)?;
            encoded_values.push(cell.stat_bytes()?);
        }
    }
    encoder.bytes(&null_bitmap, "null bitmap")?;
    encoder.u8(ENCODING_PLAIN);
    encoder.u8(COMPRESSION_LZ4);
    let uncompressed = plain.finish();
    encoder.length(uncompressed.len(), "uncompressed block")?;
    let compressed = compress(&uncompressed);
    encoder.bytes(&compressed, "compressed block")?;
    encoder.u64(xxh3_64(&compressed));
    encoder.u32(null_count);

    encoded_values.sort();
    let min = encoded_values.first().cloned().unwrap_or_default();
    let max = encoded_values.last().cloned().unwrap_or_default();
    encoder.bytes(&min, "block minimum")?;
    encoder.bytes(&max, "block maximum")?;
    let distinct = encoded_values.into_iter().collect::<HashSet<_>>().len() as u64;
    encoder.u64(distinct);
    let _ = logical_type;
    Ok(())
}

fn read_column(
    path: &Path,
    decoder: &mut Decoder<'_>,
    spec: &ColumnSpec,
    expected_rows: usize,
) -> Result<Vec<Cell>, StoreError> {
    let id = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if id != spec.id {
        return Err(corrupt_here(
            path,
            decoder,
            format!("column id {id} does not match {}", spec.id),
        ));
    }
    let logical_type = LogicalType::decode(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    )
    .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if logical_type as u8 != spec.logical_type as u8 {
        return Err(corrupt_here(path, decoder, "column logical type mismatch"));
    }
    let block_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let mut cells = Vec::with_capacity(expected_rows);
    for _ in 0..block_count {
        cells.extend(read_block(path, decoder, logical_type)?);
    }
    if cells.len() != expected_rows {
        return Err(corrupt_here(
            path,
            decoder,
            format!(
                "column has {} rows, header declares {expected_rows}",
                cells.len()
            ),
        ));
    }
    Ok(cells)
}

fn read_block(
    path: &Path,
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
) -> Result<Vec<Cell>, StoreError> {
    let row_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let null_bitmap = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?
        .to_vec();
    if null_bitmap.len() != row_count.div_ceil(8) {
        return Err(corrupt_here(path, decoder, "invalid null bitmap length"));
    }
    if decoder
        .u8()
        .map_err(|reason| corrupt_here(path, decoder, reason))?
        != ENCODING_PLAIN
    {
        return Err(corrupt_here(path, decoder, "unsupported block encoding"));
    }
    if decoder
        .u8()
        .map_err(|reason| corrupt_here(path, decoder, reason))?
        != COMPRESSION_LZ4
    {
        return Err(corrupt_here(path, decoder, "unsupported block compression"));
    }
    let uncompressed_length = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?
        as usize;
    let checksum_offset = decoder.position();
    let compressed = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let expected_checksum = decoder
        .u64()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if xxh3_64(compressed) != expected_checksum {
        return Err(corrupt(path, checksum_offset, "block checksum mismatch"));
    }
    let uncompressed = decompress(compressed, uncompressed_length)
        .map_err(|error| corrupt(path, checksum_offset, format!("invalid LZ4 block: {error}")))?;

    let declared_nulls = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let _minimum = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let _maximum = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let _distinct_estimate = decoder
        .u64()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;

    let actual_nulls = (0..row_count)
        .filter(|index| null_bitmap[index / 8] & (1 << (index % 8)) != 0)
        .count();
    if actual_nulls != declared_nulls {
        return Err(corrupt_here(path, decoder, "null count mismatch"));
    }
    let mut plain = Decoder::new(&uncompressed);
    let mut cells = Vec::with_capacity(row_count);
    for index in 0..row_count {
        if null_bitmap[index / 8] & (1 << (index % 8)) != 0 {
            cells.push(Cell::Null);
        } else {
            cells.push(
                decode_cell(&mut plain, logical_type)
                    .map_err(|reason| corrupt(path, checksum_offset, reason))?,
            );
        }
    }
    plain
        .finish()
        .map_err(|reason| corrupt(path, checksum_offset, reason))?;
    Ok(cells)
}

#[derive(Clone)]
enum Cell {
    Null,
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    Float64(u64),
    Utf8(String),
    Binary(Vec<u8>),
    Key(PrimaryKey),
}

impl Cell {
    fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Boolean(*value),
            Self::Int64(value) => Value::Int64(*value),
            Self::UInt64(value) => Value::UInt64(*value),
            Self::Float64(bits) => {
                Value::Float64(pintail_types::Float64::new(f64::from_bits(*bits)))
            }
            Self::Utf8(value) => Value::Utf8(value.clone()),
            Self::Binary(value) => Value::Binary(value.clone()),
            Self::Key(_) => unreachable!("primary keys are not user values"),
        }
    }

    fn stat_bytes(&self) -> Result<Vec<u8>, StoreError> {
        let mut encoder = Encoder::new();
        encode_cell(&mut encoder, self)?;
        Ok(encoder.finish())
    }
}

fn cell_for(spec: &ColumnSpec, row: &StoredRow) -> Cell {
    match spec.source {
        ColumnSource::Key => Cell::Key(row.key().clone()),
        ColumnSource::Version => Cell::UInt64(row.version()),
        ColumnSource::Tombstone => Cell::Boolean(row.is_deleted()),
        ColumnSource::Value(index) => match &row.values()[index] {
            Value::Null => Cell::Null,
            Value::Boolean(value) => Cell::Boolean(*value),
            Value::Int64(value) => Cell::Int64(*value),
            Value::UInt64(value) => Cell::UInt64(*value),
            Value::Float64(value) => Cell::Float64(value.to_bits()),
            Value::Utf8(value) => Cell::Utf8(value.clone()),
            Value::Binary(value) => Cell::Binary(value.clone()),
        },
    }
}

fn encode_cell(encoder: &mut Encoder, cell: &Cell) -> Result<(), StoreError> {
    match cell {
        Cell::Null => {}
        Cell::Boolean(value) => encoder.u8(u8::from(*value)),
        Cell::Int64(value) => encoder.i64(*value),
        Cell::UInt64(value) | Cell::Float64(value) => encoder.u64(*value),
        Cell::Utf8(value) => encoder.bytes(value.as_bytes(), "UTF-8 block value")?,
        Cell::Binary(value) => encoder.bytes(value, "binary block value")?,
        Cell::Key(value) => encode_key(encoder, value)?,
    }
    Ok(())
}

fn decode_cell(decoder: &mut Decoder<'_>, logical_type: LogicalType) -> Result<Cell, String> {
    match logical_type {
        LogicalType::Boolean => match decoder.u8()? {
            0 => Ok(Cell::Boolean(false)),
            1 => Ok(Cell::Boolean(true)),
            value => Err(format!("invalid boolean value {value}")),
        },
        LogicalType::Int64 => Ok(Cell::Int64(decoder.i64()?)),
        LogicalType::UInt64 => Ok(Cell::UInt64(decoder.u64()?)),
        LogicalType::Float64 => Ok(Cell::Float64(decoder.u64()?)),
        LogicalType::Utf8 => std::str::from_utf8(decoder.bytes()?)
            .map(|value| Cell::Utf8(value.to_owned()))
            .map_err(|error| format!("invalid UTF-8 block value: {error}")),
        LogicalType::Binary => Ok(Cell::Binary(decoder.bytes()?.to_vec())),
        LogicalType::PrimaryKey => Ok(Cell::Key(decode_key(decoder)?)),
    }
}

fn validate_footer(
    path: &Path,
    bytes: &[u8],
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<(), StoreError> {
    if bytes.len() < 16 {
        return Err(corrupt(path, 0, "segment is shorter than its trailer"));
    }
    let footer_offset_position = bytes.len() - size_of::<u64>();
    let footer_offset = usize::try_from(u64::from_le_bytes(
        bytes[footer_offset_position..]
            .try_into()
            .map_err(|_| corrupt(path, footer_offset_position, "invalid footer offset"))?,
    ))
    .map_err(|_| {
        corrupt(
            path,
            footer_offset_position,
            "footer offset does not fit usize",
        )
    })?;
    let checksum_position = footer_offset_position - size_of::<u64>();
    if footer_offset >= checksum_position {
        return Err(corrupt(
            path,
            footer_offset_position,
            "footer offset is outside segment",
        ));
    }
    let expected = u64::from_le_bytes(
        bytes[checksum_position..footer_offset_position]
            .try_into()
            .map_err(|_| corrupt(path, checksum_position, "invalid footer checksum"))?,
    );
    if xxh3_64(&bytes[footer_offset..checksum_position]) != expected {
        return Err(corrupt(path, footer_offset, "footer checksum mismatch"));
    }
    if bytes.get(footer_offset..footer_offset + FOOTER_MAGIC.len()) != Some(FOOTER_MAGIC) {
        return Err(corrupt(path, footer_offset, "invalid footer magic"));
    }
    if meta.schema_fingerprint != schema_fingerprint(schema) {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: schema_fingerprint(schema),
            actual: meta.schema_fingerprint,
        });
    }
    Ok(())
}

fn build_bloom(rows: &[StoredRow]) -> Result<Vec<u8>, StoreError> {
    let mut bloom = vec![0_u8; BLOOM_BYTES];
    for row in rows {
        let mut encoder = Encoder::new();
        encode_key(&mut encoder, row.key())?;
        let hash = xxh3_64(&encoder.finish());
        for shift in [0, 21, 42] {
            let bit = usize::try_from((hash >> shift) % (BLOOM_BYTES * 8) as u64)
                .map_err(|_| StoreError::FormatLimit("bloom position does not fit usize".into()))?;
            bloom[bit / 8] |= 1 << (bit % 8);
        }
    }
    Ok(bloom)
}

fn write_atomic(temporary: &Path, destination: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temporary)
        .map_err(|error| StoreError::io(format!("create {}", temporary.display()), error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| StoreError::io(format!("write {}", temporary.display()), error))?;
    std::fs::rename(temporary, destination).map_err(|error| {
        StoreError::io(
            format!(
                "publish segment {} as {}",
                temporary.display(),
                destination.display()
            ),
            error,
        )
    })?;
    sync_directory(destination.parent().expect("segment has parent"))
}

pub(crate) fn sync_directory(directory: &Path) -> Result<(), StoreError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| StoreError::io(format!("sync directory {}", directory.display()), error))
}

fn expect_raw(decoder: &mut Decoder<'_>, expected: &[u8]) -> Result<(), String> {
    if decoder.take(expected.len())? == expected {
        Ok(())
    } else {
        Err("invalid magic".to_owned())
    }
}

fn corrupt(path: &Path, offset: usize, reason: impl Into<String>) -> StoreError {
    StoreError::CorruptSegment {
        path: PathBuf::from(path),
        offset: offset as u64,
        reason: reason.into(),
    }
}

fn corrupt_here(path: &Path, decoder: &Decoder<'_>, reason: impl Into<String>) -> StoreError {
    corrupt(path, decoder.position(), reason)
}
