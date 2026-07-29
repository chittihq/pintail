use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use lz4_flex::block::{compress as lz4_compress, decompress as lz4_decompress};
use pintail_types::{DataType, KeyMode, PrimaryKey, StoredRow, TableSchema, Value};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder, decode_key, encode_key},
};

const MAGIC: &[u8; 5] = b"PTSEG";
const FOOTER_MAGIC: &[u8; 5] = b"PTFTR";
const FORMAT_VERSION: u8 = 1;
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
    pub(crate) min_key: PrimaryKey,
    pub(crate) max_key: PrimaryKey,
    pub(crate) bloom: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) enum Compression {
    Lz4 = 1,
    Zstd = 2,
}

impl Compression {
    fn decode(tag: u8) -> Result<Self, String> {
        match tag {
            1 => Ok(Self::Lz4),
            2 => Ok(Self::Zstd),
            _ => Err(format!("unknown block compression {tag}")),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LogicalType {
    Boolean = 0,
    Int64 = 1,
    UInt64 = 2,
    Float64 = 3,
    Utf8 = 4,
    Binary = 5,
    PrimaryKey = 6,
}

#[derive(Clone, Copy)]
enum Encoding {
    Plain = 0,
    Dictionary = 1,
    RunLength = 2,
    BitPacked = 3,
    DeltaBitPacked = 4,
}

impl Encoding {
    fn decode(tag: u8) -> Result<Self, String> {
        match tag {
            0 => Ok(Self::Plain),
            1 => Ok(Self::Dictionary),
            2 => Ok(Self::RunLength),
            3 => Ok(Self::BitPacked),
            4 => Ok(Self::DeltaBitPacked),
            _ => Err(format!("unknown block encoding {tag}")),
        }
    }
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
    encoder.u8(key_mode_tag(schema.key_mode()));
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

fn key_mode_tag(key_mode: KeyMode) -> u8 {
    match key_mode {
        KeyMode::Primary => 0,
        KeyMode::Unique => 1,
        KeyMode::AppendRowId => 2,
    }
}

pub(crate) fn write(
    directory: &Path,
    id: u64,
    schema: &TableSchema,
    rows: &[StoredRow],
    block_rows: usize,
    compression: Compression,
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
    let min_key = rows.first().expect("non-empty rows").key().clone();
    let max_key = rows.last().expect("non-empty rows").key().clone();
    let bloom = build_bloom(rows)?;
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
        write_column(&mut encoder, spec, rows, block_rows, compression)?;
    }

    let footer_offset = encoder.position() as u64;
    let footer_start = encoder.position();
    encoder.raw(FOOTER_MAGIC);
    encoder.u64(rows.len() as u64);
    encoder.u64(min_version);
    encoder.u64(max_version);
    encoder.u64(fingerprint);
    encoder.u64(rows.len() as u64);
    encode_key(&mut encoder, &min_key)?;
    encode_key(&mut encoder, &max_key)?;
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
    encoder.bytes(&bloom, "primary-key bloom filter")?;

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
        min_key,
        max_key,
        bloom,
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
    if schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: schema_version,
        });
    }
    let fingerprint = decoder
        .u64()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    if fingerprint != meta.schema_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: meta.schema_fingerprint,
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

    let columns = read_segment_columns(
        &path,
        &mut decoder,
        schema,
        schema_version,
        row_count,
        column_count,
    )?;
    assemble_rows(&path, &decoder, &columns, row_count)
}

struct DecodedColumns {
    keys: Vec<Cell>,
    versions: Vec<Cell>,
    tombstones: Vec<Cell>,
    values: Vec<Vec<Cell>>,
}

fn read_segment_columns(
    path: &Path,
    decoder: &mut Decoder<'_>,
    schema: &TableSchema,
    schema_version: u32,
    row_count: usize,
    column_count: usize,
) -> Result<DecodedColumns, StoreError> {
    let mut keys = None;
    let mut versions = None;
    let mut tombstones = None;
    let mut values = vec![None; schema.columns().len()];
    for _ in 0..column_count {
        let (id, logical_type, column_cells) = read_column(path, decoder, row_count)?;
        match id {
            KEY_COLUMN_ID => assign_system_column(
                path,
                decoder,
                &mut keys,
                logical_type,
                LogicalType::PrimaryKey,
                column_cells,
                "primary key",
            )?,
            VERSION_COLUMN_ID => assign_system_column(
                path,
                decoder,
                &mut versions,
                logical_type,
                LogicalType::UInt64,
                column_cells,
                "version",
            )?,
            TOMBSTONE_COLUMN_ID => assign_system_column(
                path,
                decoder,
                &mut tombstones,
                logical_type,
                LogicalType::Boolean,
                column_cells,
                "tombstone",
            )?,
            _ => {
                let Some((index, column)) = schema
                    .columns()
                    .iter()
                    .enumerate()
                    .find(|(_, column)| column.id() == id)
                else {
                    continue;
                };
                let expected = LogicalType::from_data_type(column.data_type());
                if logical_type != expected {
                    return Err(StoreError::IncompatibleSchema(format!(
                        "column {} ({id}) changed physical type",
                        column.name()
                    )));
                }
                if values[index].replace(column_cells).is_some() {
                    return Err(corrupt_here(
                        path,
                        decoder,
                        format!("duplicate user column id {id}"),
                    ));
                }
            }
        }
    }
    for (column, cells) in schema.columns().iter().zip(&mut values) {
        if cells.is_none() {
            if !column.is_nullable() {
                return Err(StoreError::IncompatibleSchema(format!(
                    "required column {} ({}) is absent from schema version {schema_version}",
                    column.name(),
                    column.id()
                )));
            }
            *cells = Some(vec![Cell::Null; row_count]);
        }
    }
    Ok(DecodedColumns {
        keys: keys.ok_or_else(|| corrupt_here(path, decoder, "missing key column"))?,
        versions: versions.ok_or_else(|| corrupt_here(path, decoder, "missing version column"))?,
        tombstones: tombstones
            .ok_or_else(|| corrupt_here(path, decoder, "missing tombstone column"))?,
        values: values
            .into_iter()
            .map(|column| column.ok_or_else(|| corrupt_here(path, decoder, "missing user column")))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn assemble_rows(
    path: &Path,
    decoder: &Decoder<'_>,
    columns: &DecodedColumns,
    row_count: usize,
) -> Result<Vec<StoredRow>, StoreError> {
    let mut rows = Vec::with_capacity(row_count);
    for row_index in 0..row_count {
        let key = match &columns.keys[row_index] {
            Cell::Key(key) => key.clone(),
            _ => return Err(corrupt_here(path, decoder, "invalid key cell")),
        };
        let Cell::UInt64(version) = columns.versions[row_index] else {
            return Err(corrupt_here(path, decoder, "invalid version cell"));
        };
        let Cell::Boolean(deleted) = columns.tombstones[row_index] else {
            return Err(corrupt_here(path, decoder, "invalid tombstone cell"));
        };
        let row_values = columns
            .values
            .iter()
            .map(|column| {
                column
                    .get(row_index)
                    .map(Cell::to_value)
                    .ok_or_else(|| corrupt_here(path, decoder, "missing user value"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(StoredRow::new(key, row_values, version, deleted));
    }
    Ok(rows)
}

pub(crate) fn verify(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<(), StoreError> {
    let path = directory.join(&meta.file_name);
    let bytes = std::fs::read(&path)
        .map_err(|error| StoreError::io(format!("read segment {}", path.display()), error))?;
    validate_footer(&path, &bytes, meta, schema)
}

pub(crate) fn might_contain_key(
    _directory: &Path,
    meta: &SegmentMeta,
    _schema: &TableSchema,
    key: &PrimaryKey,
) -> Result<bool, StoreError> {
    if key < &meta.min_key || key > &meta.max_key {
        return Ok(false);
    }
    let mut encoder = Encoder::new();
    encode_key(&mut encoder, key)?;
    Ok(bloom_might_contain(&meta.bloom, xxh3_64(&encoder.finish())))
}

pub(crate) fn overlaps_key_range(meta: &SegmentMeta, start: &PrimaryKey, end: &PrimaryKey) -> bool {
    meta.min_key <= *end && meta.max_key >= *start
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
    compression: Compression,
) -> Result<(), StoreError> {
    encoder.u32(spec.id);
    encoder.u8(spec.logical_type as u8);
    encoder.length(rows.len().div_ceil(block_rows), "column block count")?;
    for block in rows.chunks(block_rows) {
        let cells = block
            .iter()
            .map(|row| cell_for(spec, row))
            .collect::<Vec<_>>();
        write_block(encoder, spec.logical_type, &cells, compression)?;
    }
    Ok(())
}

fn write_block(
    encoder: &mut Encoder,
    logical_type: LogicalType,
    cells: &[Cell],
    compression: Compression,
) -> Result<(), StoreError> {
    encoder.length(cells.len(), "block row count")?;
    let mut null_bitmap = vec![0_u8; cells.len().div_ceil(8)];
    let mut non_null = Vec::with_capacity(cells.len());
    let mut encoded_values = Vec::with_capacity(cells.len());
    let mut null_count = 0_u32;
    for (index, cell) in cells.iter().enumerate() {
        if matches!(cell, Cell::Null) {
            null_bitmap[index / 8] |= 1 << (index % 8);
            null_count += 1;
        } else {
            non_null.push(cell.clone());
            encoded_values.push(cell.stat_bytes()?);
        }
    }
    encoder.bytes(&null_bitmap, "null bitmap")?;
    let encoding = select_encoding(logical_type, &non_null);
    encoder.u8(encoding as u8);
    encoder.u8(compression as u8);
    let uncompressed = encode_payload(logical_type, encoding, &non_null)?;
    encoder.length(uncompressed.len(), "uncompressed block")?;
    let compressed = compress_block(compression, &uncompressed)?;
    encoder.bytes(&compressed, "compressed block")?;
    encoder.u64(xxh3_64(&compressed));
    encoder.u32(null_count);

    let min = non_null
        .iter()
        .min_by(|left, right| compare_cells(left, right))
        .map(Cell::stat_bytes)
        .transpose()?
        .unwrap_or_default();
    let max = non_null
        .iter()
        .max_by(|left, right| compare_cells(left, right))
        .map(Cell::stat_bytes)
        .transpose()?
        .unwrap_or_default();
    encoder.bytes(&min, "block minimum")?;
    encoder.bytes(&max, "block maximum")?;
    encoder.bytes(&hll_registers(&encoded_values), "block HLL sketch")?;
    Ok(())
}

fn read_column(
    path: &Path,
    decoder: &mut Decoder<'_>,
    expected_rows: usize,
) -> Result<(u32, LogicalType, Vec<Cell>), StoreError> {
    let id = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let logical_type = LogicalType::decode(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    )
    .map_err(|reason| corrupt_here(path, decoder, reason))?;
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
    Ok((id, logical_type, cells))
}

fn assign_system_column(
    path: &Path,
    decoder: &Decoder<'_>,
    destination: &mut Option<Vec<Cell>>,
    actual_type: LogicalType,
    expected_type: LogicalType,
    cells: Vec<Cell>,
    name: &str,
) -> Result<(), StoreError> {
    if actual_type != expected_type {
        return Err(corrupt_here(
            path,
            decoder,
            format!("{name} column has the wrong logical type"),
        ));
    }
    if destination.replace(cells).is_some() {
        return Err(corrupt_here(
            path,
            decoder,
            format!("duplicate {name} column"),
        ));
    }
    Ok(())
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
    let encoding = Encoding::decode(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    )
    .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let compression = Compression::decode(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    )
    .map_err(|reason| corrupt_here(path, decoder, reason))?;
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
    let uncompressed = decompress_block(compression, compressed, uncompressed_length)
        .map_err(|reason| corrupt(path, checksum_offset, reason))?;

    let declared_nulls = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let _minimum = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let _maximum = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let hll = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if hll.len() != 64 {
        return Err(corrupt_here(path, decoder, "invalid HLL register count"));
    }

    let actual_nulls = (0..row_count)
        .filter(|index| null_bitmap[index / 8] & (1 << (index % 8)) != 0)
        .count();
    if actual_nulls != declared_nulls {
        return Err(corrupt_here(path, decoder, "null count mismatch"));
    }
    let non_null_count = row_count - actual_nulls;
    let decoded_values = decode_payload(&uncompressed, logical_type, encoding, non_null_count)
        .map_err(|reason| corrupt(path, checksum_offset, reason))?;
    let mut decoded_values = decoded_values.into_iter();
    let mut cells = Vec::with_capacity(row_count);
    for index in 0..row_count {
        if null_bitmap[index / 8] & (1 << (index % 8)) != 0 {
            cells.push(Cell::Null);
        } else {
            cells.push(decoded_values.next().ok_or_else(|| {
                corrupt(path, checksum_offset, "encoding produced too few values")
            })?);
        }
    }
    if decoded_values.next().is_some() {
        return Err(corrupt(
            path,
            checksum_offset,
            "encoding produced too many values",
        ));
    }
    Ok(cells)
}

fn compress_block(compression: Compression, bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    match compression {
        Compression::Lz4 => Ok(lz4_compress(bytes)),
        Compression::Zstd => zstd::bulk::compress(bytes, 3)
            .map_err(|error| StoreError::io("compress zstd segment block", error)),
    }
}

fn decompress_block(
    compression: Compression,
    bytes: &[u8],
    uncompressed_length: usize,
) -> Result<Vec<u8>, String> {
    match compression {
        Compression::Lz4 => lz4_decompress(bytes, uncompressed_length)
            .map_err(|error| format!("invalid LZ4 block: {error}")),
        Compression::Zstd => zstd::bulk::decompress(bytes, uncompressed_length)
            .map_err(|error| format!("invalid zstd block: {error}")),
    }
}

fn select_encoding(logical_type: LogicalType, cells: &[Cell]) -> Encoding {
    if cells.len() > 1 && cells.iter().all(|cell| cell == &cells[0]) {
        return Encoding::RunLength;
    }
    if matches!(logical_type, LogicalType::Utf8 | LogicalType::Binary)
        && cells.len() >= 4
        && cells.iter().collect::<HashSet<_>>().len() * 10 < cells.len()
    {
        return Encoding::Dictionary;
    }
    if cells.len() >= 3 && is_monotonic_integer(logical_type, cells) {
        return Encoding::DeltaBitPacked;
    }
    if matches!(
        logical_type,
        LogicalType::Boolean | LogicalType::Int64 | LogicalType::UInt64
    ) {
        return Encoding::BitPacked;
    }
    Encoding::Plain
}

fn compare_cells(left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Boolean(left), Cell::Boolean(right)) => left.cmp(right),
        (Cell::Int64(left), Cell::Int64(right)) => left.cmp(right),
        (Cell::UInt64(left), Cell::UInt64(right)) => left.cmp(right),
        (Cell::Float64(left), Cell::Float64(right)) => {
            f64::from_bits(*left).total_cmp(&f64::from_bits(*right))
        }
        (Cell::Utf8(left), Cell::Utf8(right)) => left.cmp(right),
        (Cell::Binary(left), Cell::Binary(right)) => left.cmp(right),
        (Cell::Key(left), Cell::Key(right)) => left.cmp(right),
        _ => unreachable!("a segment block contains one logical type"),
    }
}

fn hll_registers(encoded_values: &[Vec<u8>]) -> [u8; 64] {
    let mut registers = [0_u8; 64];
    for value in encoded_values {
        let hash = xxh3_64(value);
        let index = usize::from(hash.to_le_bytes()[0] & 63);
        let rank = u8::try_from((hash >> 6).leading_zeros() - 5).expect("HLL rank is at most 59");
        registers[index] = registers[index].max(rank);
    }
    registers
}

fn is_monotonic_integer(logical_type: LogicalType, cells: &[Cell]) -> bool {
    match logical_type {
        LogicalType::UInt64 => cells.windows(2).all(|pair| match pair {
            [Cell::UInt64(left), Cell::UInt64(right)] => left <= right,
            _ => false,
        }),
        LogicalType::Int64 => cells.windows(2).all(|pair| match pair {
            [Cell::Int64(left), Cell::Int64(right)] => left <= right,
            _ => false,
        }),
        _ => false,
    }
}

fn encode_payload(
    logical_type: LogicalType,
    encoding: Encoding,
    cells: &[Cell],
) -> Result<Vec<u8>, StoreError> {
    let mut encoder = Encoder::new();
    match encoding {
        Encoding::Plain => {
            for cell in cells {
                encode_cell(&mut encoder, cell)?;
            }
        }
        Encoding::Dictionary => encode_dictionary(&mut encoder, cells)?,
        Encoding::RunLength => encode_runs(&mut encoder, cells)?,
        Encoding::BitPacked => encode_bit_packed(&mut encoder, logical_type, cells)?,
        Encoding::DeltaBitPacked => encode_delta_bit_packed(&mut encoder, logical_type, cells)?,
    }
    Ok(encoder.finish())
}

fn decode_payload(
    bytes: &[u8],
    logical_type: LogicalType,
    encoding: Encoding,
    value_count: usize,
) -> Result<Vec<Cell>, String> {
    let mut decoder = Decoder::new(bytes);
    let values = match encoding {
        Encoding::Plain => (0..value_count)
            .map(|_| decode_cell(&mut decoder, logical_type))
            .collect::<Result<Vec<_>, _>>()?,
        Encoding::Dictionary => decode_dictionary(&mut decoder, logical_type, value_count)?,
        Encoding::RunLength => decode_runs(&mut decoder, logical_type, value_count)?,
        Encoding::BitPacked => decode_bit_packed(&mut decoder, logical_type, value_count)?,
        Encoding::DeltaBitPacked => {
            decode_delta_bit_packed(&mut decoder, logical_type, value_count)?
        }
    };
    decoder.finish()?;
    Ok(values)
}

fn encode_dictionary(encoder: &mut Encoder, cells: &[Cell]) -> Result<(), StoreError> {
    let mut positions = HashMap::new();
    let mut dictionary = Vec::new();
    let mut indices = Vec::with_capacity(cells.len());
    for cell in cells {
        let index = if let Some(index) = positions.get(cell) {
            *index
        } else {
            let index = u32::try_from(dictionary.len())
                .map_err(|_| StoreError::FormatLimit("dictionary exceeds u32::MAX".into()))?;
            positions.insert(cell.clone(), index);
            dictionary.push(cell.clone());
            index
        };
        indices.push(index);
    }
    encoder.length(dictionary.len(), "block dictionary")?;
    for value in &dictionary {
        encode_cell(encoder, value)?;
    }
    for index in indices {
        encoder.u32(index);
    }
    Ok(())
}

fn decode_dictionary(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    value_count: usize,
) -> Result<Vec<Cell>, String> {
    let dictionary_count = decoder.u32()? as usize;
    let dictionary = (0..dictionary_count)
        .map(|_| decode_cell(decoder, logical_type))
        .collect::<Result<Vec<_>, _>>()?;
    (0..value_count)
        .map(|_| {
            let index = decoder.u32()? as usize;
            dictionary
                .get(index)
                .cloned()
                .ok_or_else(|| format!("dictionary index {index} is out of bounds"))
        })
        .collect()
}

fn encode_runs(encoder: &mut Encoder, cells: &[Cell]) -> Result<(), StoreError> {
    let mut runs: Vec<(u32, &Cell)> = Vec::new();
    for cell in cells {
        if let Some((length, previous)) = runs.last_mut()
            && *previous == cell
        {
            *length = length
                .checked_add(1)
                .ok_or_else(|| StoreError::FormatLimit("run length exceeds u32::MAX".into()))?;
            continue;
        }
        runs.push((1, cell));
    }
    encoder.length(runs.len(), "run count")?;
    for (length, value) in runs {
        encoder.u32(length);
        encode_cell(encoder, value)?;
    }
    Ok(())
}

fn decode_runs(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    value_count: usize,
) -> Result<Vec<Cell>, String> {
    let run_count = decoder.u32()?;
    let mut values = Vec::with_capacity(value_count);
    for _ in 0..run_count {
        let length = decoder.u32()? as usize;
        if length == 0 {
            return Err("run length must be non-zero".to_owned());
        }
        let value = decode_cell(decoder, logical_type)?;
        if values.len().saturating_add(length) > value_count {
            return Err("run lengths exceed block value count".to_owned());
        }
        values.extend(std::iter::repeat_n(value, length));
    }
    if values.len() != value_count {
        return Err(format!(
            "run lengths produce {} values, expected {value_count}",
            values.len()
        ));
    }
    Ok(values)
}

fn encode_bit_packed(
    encoder: &mut Encoder,
    logical_type: LogicalType,
    cells: &[Cell],
) -> Result<(), StoreError> {
    let (base, normalized) = normalize_integers(logical_type, cells)?;
    encode_integer_base(encoder, logical_type, base)?;
    encode_packed(encoder, &normalized)
}

fn decode_bit_packed(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    value_count: usize,
) -> Result<Vec<Cell>, String> {
    let base = decode_integer_base(decoder, logical_type)?;
    unpack(decoder, value_count)?
        .into_iter()
        .map(|value| integer_from_base(logical_type, base, value))
        .collect()
}

fn encode_delta_bit_packed(
    encoder: &mut Encoder,
    logical_type: LogicalType,
    cells: &[Cell],
) -> Result<(), StoreError> {
    let first = cells
        .first()
        .ok_or_else(|| StoreError::FormatLimit("delta block cannot be empty".into()))?;
    encode_cell(encoder, first)?;
    let values = integer_values(logical_type, cells)?;
    let deltas = values
        .windows(2)
        .map(|pair| {
            u64::try_from(pair[1] - pair[0])
                .map_err(|_| StoreError::FormatLimit("integer delta exceeds u64".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    encode_packed(encoder, &deltas)
}

fn decode_delta_bit_packed(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    value_count: usize,
) -> Result<Vec<Cell>, String> {
    if value_count == 0 {
        return Err("delta block cannot be empty".to_owned());
    }
    let first = decode_cell(decoder, logical_type)?;
    let mut current = integer_value(logical_type, &first)?;
    let deltas = unpack(decoder, value_count - 1)?;
    let mut values = Vec::with_capacity(value_count);
    values.push(first);
    for delta in deltas {
        current = current
            .checked_add(i128::from(delta))
            .ok_or_else(|| "integer delta overflow".to_owned())?;
        values.push(integer_from_i128(logical_type, current)?);
    }
    Ok(values)
}

fn normalize_integers(
    logical_type: LogicalType,
    cells: &[Cell],
) -> Result<(i128, Vec<u64>), StoreError> {
    let values = integer_values(logical_type, cells)?;
    let base = values.iter().copied().min().unwrap_or(0);
    let normalized = values
        .into_iter()
        .map(|value| {
            u64::try_from(value - base)
                .map_err(|_| StoreError::FormatLimit("integer range exceeds u64".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((base, normalized))
}

fn integer_values(logical_type: LogicalType, cells: &[Cell]) -> Result<Vec<i128>, StoreError> {
    cells
        .iter()
        .map(|cell| {
            integer_value(logical_type, cell)
                .map_err(|reason| StoreError::FormatLimit(reason.to_owned()))
        })
        .collect()
}

fn integer_value(logical_type: LogicalType, cell: &Cell) -> Result<i128, &'static str> {
    match (logical_type, cell) {
        (LogicalType::Boolean, Cell::Boolean(value)) => Ok(i128::from(*value)),
        (LogicalType::Int64, Cell::Int64(value)) => Ok(i128::from(*value)),
        (LogicalType::UInt64, Cell::UInt64(value)) => Ok(i128::from(*value)),
        _ => Err("bit-packed value does not match logical type"),
    }
}

fn encode_integer_base(
    encoder: &mut Encoder,
    logical_type: LogicalType,
    base: i128,
) -> Result<(), StoreError> {
    match logical_type {
        LogicalType::Boolean => encoder.u8(u8::try_from(base)
            .map_err(|_| StoreError::FormatLimit("boolean base does not fit u8".into()))?),
        LogicalType::Int64 => encoder.i64(
            i64::try_from(base)
                .map_err(|_| StoreError::FormatLimit("signed base does not fit i64".into()))?,
        ),
        LogicalType::UInt64 => encoder.u64(
            u64::try_from(base)
                .map_err(|_| StoreError::FormatLimit("unsigned base does not fit u64".into()))?,
        ),
        _ => {
            return Err(StoreError::FormatLimit(
                "logical type cannot be bit-packed".into(),
            ));
        }
    }
    Ok(())
}

fn decode_integer_base(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
) -> Result<i128, String> {
    match logical_type {
        LogicalType::Boolean => Ok(i128::from(decoder.u8()?)),
        LogicalType::Int64 => Ok(i128::from(decoder.i64()?)),
        LogicalType::UInt64 => Ok(i128::from(decoder.u64()?)),
        _ => Err("logical type cannot be bit-packed".to_owned()),
    }
}

fn integer_from_base(
    logical_type: LogicalType,
    base: i128,
    normalized: u64,
) -> Result<Cell, String> {
    let value = base
        .checked_add(i128::from(normalized))
        .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
    integer_from_i128(logical_type, value)
}

fn integer_from_i128(logical_type: LogicalType, value: i128) -> Result<Cell, String> {
    match logical_type {
        LogicalType::Boolean => match value {
            0 => Ok(Cell::Boolean(false)),
            1 => Ok(Cell::Boolean(true)),
            _ => Err(format!("invalid bit-packed boolean {value}")),
        },
        LogicalType::Int64 => i64::try_from(value)
            .map(Cell::Int64)
            .map_err(|_| "bit-packed signed integer overflow".to_owned()),
        LogicalType::UInt64 => u64::try_from(value)
            .map(Cell::UInt64)
            .map_err(|_| "bit-packed unsigned integer overflow".to_owned()),
        _ => Err("logical type cannot be bit-packed".to_owned()),
    }
}

fn encode_packed(encoder: &mut Encoder, values: &[u64]) -> Result<(), StoreError> {
    let maximum = values.iter().copied().max().unwrap_or(0);
    let width = u8::try_from(u64::BITS - maximum.leading_zeros())
        .map_err(|_| StoreError::FormatLimit("bit width does not fit u8".into()))?;
    encoder.u8(width);
    encoder.bytes(&pack(values, width)?, "bit-packed values")
}

fn pack(values: &[u64], width: u8) -> Result<Vec<u8>, StoreError> {
    let total_bits = values
        .len()
        .checked_mul(usize::from(width))
        .ok_or_else(|| StoreError::FormatLimit("bit-packed length overflow".into()))?;
    let mut bytes = vec![0_u8; total_bits.div_ceil(8)];
    for (value_index, value) in values.iter().enumerate() {
        for bit in 0..width {
            if value & (1_u64 << bit) != 0 {
                let position = value_index * usize::from(width) + usize::from(bit);
                bytes[position / 8] |= 1 << (position % 8);
            }
        }
    }
    Ok(bytes)
}

fn unpack(decoder: &mut Decoder<'_>, value_count: usize) -> Result<Vec<u64>, String> {
    let width = decoder.u8()?;
    if width > 64 {
        return Err(format!("invalid bit width {width}"));
    }
    let bytes = decoder.bytes()?;
    let expected_bits = value_count
        .checked_mul(usize::from(width))
        .ok_or_else(|| "bit-packed length overflow".to_owned())?;
    if bytes.len() != expected_bits.div_ceil(8) {
        return Err(format!(
            "bit-packed payload has {} bytes, expected {}",
            bytes.len(),
            expected_bits.div_ceil(8)
        ));
    }
    let mut values = vec![0_u64; value_count];
    for (value_index, value) in values.iter_mut().enumerate() {
        for bit in 0..width {
            let position = value_index * usize::from(width) + usize::from(bit);
            if bytes[position / 8] & (1 << (position % 8)) != 0 {
                *value |= 1_u64 << bit;
            }
        }
    }
    Ok(values)
}

#[derive(Clone, Eq, Hash, PartialEq)]
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
    validate_footer_body(
        path,
        &bytes[footer_offset..checksum_position],
        footer_offset,
        meta,
    )?;
    if bytes.len() < 18 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt(path, 0, "invalid segment header"));
    }
    let segment_schema_version = u32::from_le_bytes(
        bytes[6..10]
            .try_into()
            .map_err(|_| corrupt(path, 6, "invalid schema version"))?,
    );
    if segment_schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: segment_schema_version,
        });
    }
    let segment_fingerprint = u64::from_le_bytes(
        bytes[10..18]
            .try_into()
            .map_err(|_| corrupt(path, 10, "invalid schema fingerprint"))?,
    );
    if meta.schema_fingerprint != segment_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: meta.schema_fingerprint,
            actual: segment_fingerprint,
        });
    }
    Ok(())
}

fn validate_footer_body(
    path: &Path,
    bytes: &[u8],
    footer_offset: usize,
    meta: &SegmentMeta,
) -> Result<(), StoreError> {
    let mut decoder = Decoder::new(bytes);
    expect_raw(&mut decoder, FOOTER_MAGIC)
        .map_err(|reason| corrupt(path, footer_offset, reason))?;
    let row_count = decoder
        .u64()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let min_version = decoder
        .u64()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let max_version = decoder
        .u64()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let fingerprint = decoder
        .u64()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let unique_keys = decoder
        .u64()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    if (row_count, min_version, max_version, fingerprint)
        != (
            meta.row_count,
            meta.min_version,
            meta.max_version,
            meta.schema_fingerprint,
        )
        || unique_keys > row_count
    {
        return Err(corrupt(
            path,
            footer_offset,
            "footer metadata does not match the manifest",
        ));
    }
    let first_key = decode_key(&mut decoder)
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let last_key = decode_key(&mut decoder)
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    let column_count = decoder
        .u32()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    for _ in 0..column_count {
        decoder
            .u64()
            .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    }
    let sparse_count = decoder
        .u32()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    for _ in 0..sparse_count {
        decoder
            .u64()
            .and_then(|_| decode_key(&mut decoder).map(|_| 0))
            .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    }
    let bloom = decoder
        .bytes()
        .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
    if bloom.len() != BLOOM_BYTES {
        return Err(corrupt(
            path,
            footer_offset + decoder.position(),
            "invalid primary-key bloom filter length",
        ));
    }
    if first_key != meta.min_key || last_key != meta.max_key || bloom != meta.bloom {
        return Err(corrupt(
            path,
            footer_offset,
            "footer key index does not match the manifest",
        ));
    }
    decoder
        .finish()
        .map_err(|reason| corrupt(path, footer_offset, reason))?;
    Ok(())
}

fn build_bloom(rows: &[StoredRow]) -> Result<Vec<u8>, StoreError> {
    let mut bloom = vec![0_u8; BLOOM_BYTES];
    for row in rows {
        let mut encoder = Encoder::new();
        encode_key(&mut encoder, row.key())?;
        let hash = xxh3_64(&encoder.finish());
        set_bloom_bits(&mut bloom, hash)?;
    }
    Ok(bloom)
}

fn set_bloom_bits(bloom: &mut [u8], hash: u64) -> Result<(), StoreError> {
    for shift in [0, 21, 42] {
        let bit = usize::try_from((hash >> shift) % (bloom.len() * 8) as u64)
            .map_err(|_| StoreError::FormatLimit("bloom position does not fit usize".into()))?;
        bloom[bit / 8] |= 1 << (bit % 8);
    }
    Ok(())
}

fn bloom_might_contain(bloom: &[u8], hash: u64) -> bool {
    [0, 21, 42].into_iter().all(|shift| {
        let bit = usize::try_from((hash >> shift) % (bloom.len() * 8) as u64)
            .expect("bloom bit index is bounded by the fixed bloom length");
        bloom[bit / 8] & (1 << (bit % 8)) != 0
    })
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
