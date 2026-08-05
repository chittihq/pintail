use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

use lz4_flex::block::{compress as lz4_compress, decompress as lz4_decompress};
use pintail_types::{DataType, KeyMode, PrimaryKey, StoredRow, TableSchema, Value};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    StoreError,
    codec::{Decoder, Encoder, decode_key, encode_key},
    store::DecodedColumn,
};

const MAGIC: &[u8; 5] = b"PTSEG";
const FOOTER_MAGIC: &[u8; 5] = b"PTFTR";
const FORMAT_VERSION: u8 = 2;

/// Segment versions this reader understands: v1 stores text carriers for
/// every Utf8-storage column; v2 additionally stores fixed-width native
/// units (wire type Int64) for eligible Decimal/Date32/DateTime64 columns.
const fn format_version_supported(version: u8) -> bool {
    matches!(version, 1 | 2)
}
const KEY_COLUMN_ID: u32 = u32::MAX - 2;
const VERSION_COLUMN_ID: u32 = u32::MAX - 1;
const TOMBSTONE_COLUMN_ID: u32 = u32::MAX;
const BLOOM_BYTES: usize = 256;

trait DecodePosition {
    fn decode_position(&self) -> usize;
}

impl DecodePosition for Decoder<'_> {
    fn decode_position(&self) -> usize {
        self.position()
    }
}

struct FileDecoder {
    reader: BufReader<File>,
    position: usize,
}

impl FileDecoder {
    fn open(path: &Path) -> Result<Self, StoreError> {
        let file = File::open(path)
            .map_err(|error| StoreError::io(format!("open segment {}", path.display()), error))?;
        Ok(Self {
            reader: BufReader::new(file),
            position: 0,
        })
    }

    fn u8(&mut self) -> Result<u8, String> {
        let mut bytes = [0_u8; 1];
        self.read_exact(&mut bytes)?;
        Ok(bytes[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut bytes = [0_u8; 4];
        self.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut bytes = [0_u8; 8];
        self.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn raw(&mut self, length: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0_u8; length];
        self.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), String> {
        self.reader
            .read_exact(bytes)
            .map_err(|error| error.to_string())?;
        self.position = self.position.saturating_add(bytes.len());
        Ok(())
    }

    fn seek_to(&mut self, position: usize) -> Result<(), String> {
        self.reader
            .seek(SeekFrom::Start(
                u64::try_from(position).map_err(|_| "file offset exceeds u64".to_owned())?,
            ))
            .map_err(|error| error.to_string())?;
        self.position = position;
        Ok(())
    }

    fn skip(&mut self, length: usize) -> Result<(), String> {
        let position = self
            .position
            .checked_add(length)
            .ok_or_else(|| "file offset overflow".to_owned())?;
        self.seek_to(position)
    }
}

impl DecodePosition for FileDecoder {
    fn decode_position(&self) -> usize {
        self.position
    }
}

pub(crate) struct ScanMemoryBudget<'a> {
    used: &'a AtomicUsize,
    limit: usize,
}

impl<'a> ScanMemoryBudget<'a> {
    pub(crate) const fn new(used: &'a AtomicUsize, limit: usize) -> Self {
        Self { used, limit }
    }

    pub(crate) fn reserve(&self, requested: usize) -> Result<(), StoreError> {
        reserve_scan_memory(self.used, requested, self.limit)
    }

    pub(crate) fn release(&self, released: usize) {
        self.used.fetch_sub(released, AtomicOrdering::Relaxed);
    }

    fn reserve_temporary(&self, requested: usize) -> Result<ScanMemoryReservation<'a>, StoreError> {
        self.reserve(requested)?;
        Ok(ScanMemoryReservation {
            used: self.used,
            reserved: requested,
        })
    }
}

struct ScanMemoryReservation<'a> {
    used: &'a AtomicUsize,
    reserved: usize,
}

impl Drop for ScanMemoryReservation<'_> {
    fn drop(&mut self) {
        self.used.fetch_sub(self.reserved, AtomicOrdering::Relaxed);
    }
}

fn reserve_scan_memory(
    used: &AtomicUsize,
    requested: usize,
    limit: usize,
) -> Result<(), StoreError> {
    let mut current = used.load(AtomicOrdering::Relaxed);
    loop {
        let next = current.saturating_add(requested);
        if next > limit {
            return Err(StoreError::MemoryLimitExceeded {
                used: current,
                requested,
                limit,
            });
        }
        match used.compare_exchange_weak(
            current,
            next,
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

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
    pub(crate) unique_keys: bool,
    /// Small materialized aggregates carried in the manifest since format
    /// v2; `None` for segments recorded by a v1 manifest (they decline the
    /// aggregate fast path and scan normally).
    pub(crate) smas: Option<SegmentSmas>,
}

/// Per-segment small materialized aggregates (WS3-B): enough to fold bare
/// COUNT/SUM/AVG/MIN/MAX over a segment without touching its blocks, as
/// long as merge-on-read cannot overlay the segment (no tombstones inside,
/// no newer key in its range — the caller checks both).
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentSmas {
    /// Rows that survive merge-on-read within this segment alone.
    pub live_rows: u64,
    /// Delete markers stored in this segment. Any tombstone disables the
    /// fold: MIN/MAX cannot be delta-adjusted under deletes.
    pub tombstones: u64,
    /// One entry per schema column that carries foldable statistics.
    pub columns: Vec<ColumnSma>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSma {
    /// Schema column id (stable across projection and evolution).
    pub column_id: u32,
    /// Live rows whose value is non-NULL.
    pub non_null: u64,
    pub sum: Option<SmaSum>,
    pub extremes: Option<SmaExtremes>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmaSum {
    /// Exact integer total (i128 cannot overflow from u64/i64 addends).
    Int(i128),
    Float(f64),
    /// Exact decimal total in scaled units.
    DecimalUnits {
        units: i128,
        scale: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmaExtremes {
    Int {
        min: i64,
        max: i64,
    },
    UInt {
        min: u64,
        max: u64,
    },
    Float {
        min: f64,
        max: f64,
    },
    DecimalUnits {
        min: i128,
        max: i128,
        scale: u8,
    },
    /// Native temporal units (days for `Date32`, microseconds for
    /// `DateTime64`); consumers must format through [`NativeUnits`].
    Temporal {
        min: i64,
        max: i64,
        units: NativeUnits,
    },
}

/// One scan-predicate value bound in a column's SMA domain, used to prune
/// whole segments whose extremes are provably disjoint from it.
#[derive(Clone, Copy, Debug)]
pub struct ColumnBounds {
    /// Stable schema column id the bound constrains.
    pub column_id: u32,
    /// Which extremes family the bound values live in.
    pub domain: BoundDomain,
    /// Inclusive lower bound in the domain's integer units.
    pub lower: Option<i128>,
    /// Inclusive upper bound in the domain's integer units.
    pub upper: Option<i128>,
}

/// The extremes family a [`ColumnBounds`] compares against. A mismatched
/// family never prunes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundDomain {
    Int,
    UInt,
    /// Native temporal units of the named kind.
    Temporal(NativeUnits),
}

/// Whether a segment's statistics prove every live row fails one of the
/// bounds. NULL values fail range predicates, so non-NULL extremes decide;
/// an all-NULL column fails them all. Callers gate on clean, key-disjoint
/// manifests — under overlapping row versions value pruning is unsound.
pub(crate) fn sma_disjoint(meta: &SegmentMeta, bounds: &[ColumnBounds]) -> bool {
    let Some(smas) = &meta.smas else { return false };
    if smas.tombstones > 0 {
        return false;
    }
    for bound in bounds {
        let Some(column) = smas
            .columns
            .iter()
            .find(|column| column.column_id == bound.column_id)
        else {
            continue;
        };
        if column.non_null == 0 && smas.live_rows > 0 {
            return true;
        }
        let Some(extremes) = column.extremes else {
            continue;
        };
        let range = match (bound.domain, extremes) {
            (BoundDomain::Int, SmaExtremes::Int { min, max }) => {
                Some((i128::from(min), i128::from(max)))
            }
            (BoundDomain::UInt, SmaExtremes::UInt { min, max }) => {
                Some((i128::from(min), i128::from(max)))
            }
            (
                BoundDomain::Temporal(units),
                SmaExtremes::Temporal {
                    min,
                    max,
                    units: kind,
                },
            ) if units == kind => Some((i128::from(min), i128::from(max))),
            _ => None,
        };
        let Some((min, max)) = range else { continue };
        if bound.lower.is_some_and(|lower| max < lower)
            || bound.upper.is_some_and(|upper| min > upper)
        {
            return true;
        }
    }
    false
}

/// Computes per-column SMAs for a segment's rows. Columns whose type or
/// contents fall outside the supported statistics keep only the non-NULL
/// count, which still answers COUNT(column).
#[allow(clippy::too_many_lines)]
pub(crate) fn compute_segment_smas(schema: &TableSchema, rows: &[StoredRow]) -> SegmentSmas {
    let tombstones = rows.iter().filter(|row| row.is_deleted()).count() as u64;
    let live_rows = rows.len() as u64 - tombstones;
    let columns = schema
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut non_null = 0_u64;
            let mut int_sum = Some(0_i128);
            let mut int_extremes: Option<(i128, i128)> = None;
            let mut float_sum = Some(0.0_f64);
            let mut float_extremes: Option<(f64, f64)> = None;
            let decimal_scale = match column.data_type() {
                DataType::Decimal { scale, .. } => Some(scale),
                _ => None,
            };
            // Temporal columns fold their native units (days/microseconds)
            // into extremes; sums over dates are meaningless and stay unset.
            let temporal_units = match column.data_type() {
                DataType::Date32 | DataType::DateTime64 { .. } => {
                    NativeUnits::for_data_type(column.data_type())
                }
                _ => None,
            };
            // One numeric family per column; the first value outside it
            // clears every statistic except the non-NULL count.
            let mut family: Option<u8> = None;
            let mut supported = true;
            for row in rows {
                if row.is_deleted() {
                    continue;
                }
                let value = &row.values()[index];
                if matches!(value, Value::Null) {
                    continue;
                }
                non_null += 1;
                if !supported {
                    continue;
                }
                let observed = match (value, decimal_scale, temporal_units) {
                    (Value::Utf8(text), Some(scale), _) => {
                        pintail_types::parse_decimal_scaled(text, scale)
                            .map(|units| (3, units, 0.0))
                    }
                    (Value::Utf8(text), None, Some(units)) => units
                        .parse_exact(text)
                        .map(|value| (5, i128::from(value), 0.0)),
                    (Value::Int64(value), None, None) => Some((1, i128::from(*value), 0.0)),
                    (Value::UInt64(value), None, None) => Some((2, i128::from(*value), 0.0)),
                    (Value::Float64(value), None, None) => Some((4, 0, value.get())),
                    _ => None,
                };
                let Some((kind, integer, float)) = observed else {
                    supported = false;
                    continue;
                };
                if *family.get_or_insert(kind) != kind {
                    supported = false;
                    continue;
                }
                if kind == 4 {
                    float_sum = float_sum
                        .map(|sum| sum + float)
                        .filter(|sum| sum.is_finite());
                    float_extremes = Some(match float_extremes {
                        Some((min, max)) => (min.min(float), max.max(float)),
                        None => (float, float),
                    });
                } else {
                    int_sum = int_sum.and_then(|sum| sum.checked_add(integer));
                    int_extremes = Some(match int_extremes {
                        Some((min, max)) => (min.min(integer), max.max(integer)),
                        None => (integer, integer),
                    });
                }
            }
            let (sum, extremes) = if !supported || non_null == 0 {
                (None, None)
            } else {
                match family {
                    Some(1) => (
                        int_sum.map(SmaSum::Int),
                        int_extremes.map(|(min, max)| SmaExtremes::Int {
                            min: i64::try_from(min).expect("i64 addends"),
                            max: i64::try_from(max).expect("i64 addends"),
                        }),
                    ),
                    Some(2) => (
                        int_sum.map(SmaSum::Int),
                        int_extremes.map(|(min, max)| SmaExtremes::UInt {
                            min: u64::try_from(min).expect("u64 addends"),
                            max: u64::try_from(max).expect("u64 addends"),
                        }),
                    ),
                    Some(3) => {
                        let scale = decimal_scale.expect("decimal family implies scale");
                        (
                            int_sum.map(|units| SmaSum::DecimalUnits { units, scale }),
                            int_extremes.map(|(min, max)| SmaExtremes::DecimalUnits {
                                min,
                                max,
                                scale,
                            }),
                        )
                    }
                    Some(4) => (
                        float_sum.map(SmaSum::Float),
                        float_extremes.map(|(min, max)| SmaExtremes::Float { min, max }),
                    ),
                    Some(5) => (
                        None,
                        int_extremes.and_then(|(min, max)| {
                            let units = temporal_units?;
                            Some(SmaExtremes::Temporal {
                                min: i64::try_from(min).expect("native units fit i64"),
                                max: i64::try_from(max).expect("native units fit i64"),
                                units,
                            })
                        }),
                    ),
                    _ => (None, None),
                }
            };
            ColumnSma {
                column_id: column.id(),
                non_null,
                sum,
                extremes,
            }
        })
        .collect();
    SegmentSmas {
        live_rows,
        tombstones,
        columns,
    }
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

/// A fixed-width unit representation for a text-carried column, eligible
/// only when every stored value round-trips text -> units -> identical text
/// (PTSEG v2; docs/decisions.md "PTSEG v2 approved").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeUnits {
    /// Days since 1970-01-01 for `Date32` columns.
    Date,
    /// Microseconds since the epoch for `DateTime64` columns; the fractional
    /// precision needed to regenerate canonical text.
    DateTime { fsp: u8 },
    /// Scaled integers for `Decimal` columns with precision <= 18 (fits i64).
    Decimal { scale: u8 },
}

impl NativeUnits {
    /// The native representation a column's declared type could use, if any.
    #[must_use]
    pub fn for_data_type(data_type: DataType) -> Option<Self> {
        match data_type {
            DataType::Date32 => Some(Self::Date),
            DataType::DateTime64 { fsp } => Some(Self::DateTime { fsp }),
            DataType::Decimal { precision, scale } if precision <= 18 => {
                Some(Self::Decimal { scale })
            }
            _ => None,
        }
    }

    /// Parses one canonical text value into units, returning `None` unless
    /// the units regenerate the identical text (the round-trip guarantee the
    /// v2 writer requires before storing units instead of text).
    #[must_use]
    pub fn parse_exact(self, text: &str) -> Option<i64> {
        match self {
            Self::Date => {
                let days = pintail_types::parse_date_days(text)?;
                (pintail_types::format_date_days(days).as_deref() == Some(text)).then_some(days)
            }
            Self::DateTime { fsp } => {
                let micros = pintail_types::parse_datetime_micros(text)?;
                (pintail_types::format_datetime_micros(micros, fsp).as_deref() == Some(text))
                    .then_some(micros)
            }
            Self::Decimal { scale } => {
                let scaled = pintail_types::parse_decimal_scaled(text, scale)?;
                let scaled = i64::try_from(scaled).ok()?;
                (pintail_types::format_decimal_scaled(i128::from(scaled), scale) == text)
                    .then_some(scaled)
            }
        }
    }

    /// Regenerates the canonical text for stored units. `None` indicates
    /// corruption: the writer only stores units that round-trip.
    #[must_use]
    pub fn format(self, units: i64) -> Option<String> {
        match self {
            Self::Date => pintail_types::format_date_days(units),
            Self::DateTime { fsp } => pintail_types::format_datetime_micros(units, fsp),
            Self::Decimal { scale } => Some(pintail_types::format_decimal_scaled(
                i128::from(units),
                scale,
            )),
        }
    }
}

/// Whether a stored column's wire type is valid for its schema type: the
/// declared physical carrier always is; wire `Int64` additionally is for
/// columns with a native-unit representation (PTSEG v2 stores units).
fn wire_type_compatible(data_type: DataType, logical_type: LogicalType) -> bool {
    LogicalType::from_data_type(data_type) == logical_type
        || (logical_type == LogicalType::Int64 && NativeUnits::for_data_type(data_type).is_some())
}

/// Rewrites a native column's unit cells back into their canonical text
/// carrier, so downstream consumers keep seeing v1-shaped values (task #10
/// step A; step B will hand units through to the executor untouched).
fn format_native_cells(
    path: &Path,
    units: NativeUnits,
    cells: &mut [Cell],
) -> Result<(), StoreError> {
    for cell in cells.iter_mut() {
        match cell {
            Cell::Null => {}
            Cell::Int64(value) => {
                let text = units.format(*value).ok_or_else(|| {
                    corrupt(path, 0, "native units outside the canonical text range")
                })?;
                *cell = Cell::Utf8(text);
            }
            _ => return Err(corrupt(path, 0, "native column holds a non-integer cell")),
        }
    }
    Ok(())
}

/// Decides whether every value of one projected column can be stored as
/// fixed-width units: `Some(units)` (with `None` per null slot) only when
/// each non-null value passes the exact round-trip check.
#[allow(dead_code)] // consumed by the v2 writer (task #10 step A)
pub(crate) fn probe_native_column(
    units: NativeUnits,
    rows: &[StoredRow],
    value_index: usize,
) -> Option<Vec<Option<i64>>> {
    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows {
        match &row.values()[value_index] {
            Value::Null => parsed.push(None),
            Value::Utf8(text) => parsed.push(Some(units.parse_exact(text)?)),
            _ => return None,
        }
    }
    Some(parsed)
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
        match data_type.storage_type() {
            DataType::Boolean => Self::Boolean,
            DataType::Int64 => Self::Int64,
            DataType::UInt64 => Self::UInt64,
            DataType::Float64 => Self::Float64,
            DataType::Utf8 => Self::Utf8,
            DataType::Binary => Self::Binary,
            _ => unreachable!("storage_type returns a physical scalar type"),
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
    /// `Some` when this column's rows all passed the exact round-trip probe
    /// and will be stored as fixed-width units (wire type `Int64`).
    native: Option<NativeUnits>,
}

pub(crate) fn schema_fingerprint(schema: &TableSchema) -> u64 {
    let mut encoder = Encoder::new();
    encoder.u32(schema.version());
    encoder.u8(key_mode_tag(schema.key_mode()));
    encoder.u32(u32::try_from(schema.columns().len()).unwrap_or(u32::MAX));
    for column in schema.columns() {
        encoder.u32(column.id());
        encode_schema_data_type(&mut encoder, column.data_type());
        encoder.u8(u8::from(column.is_nullable()));
        encoder.raw(column.name().as_bytes());
        encoder.u8(0);
    }
    xxh3_64(&encoder.finish())
}

fn encode_schema_data_type(encoder: &mut Encoder, data_type: DataType) {
    match data_type {
        DataType::Boolean => encoder.u8(0),
        DataType::Int64 => encoder.u8(1),
        DataType::UInt64 => encoder.u8(2),
        DataType::Float64 => encoder.u8(3),
        DataType::Utf8 => encoder.u8(4),
        DataType::Binary => encoder.u8(5),
        DataType::Int8 => encoder.u8(6),
        DataType::Int16 => encoder.u8(7),
        DataType::Int32 => encoder.u8(8),
        DataType::UInt8 => encoder.u8(9),
        DataType::UInt16 => encoder.u8(10),
        DataType::UInt32 => encoder.u8(11),
        DataType::Float32 => encoder.u8(12),
        DataType::Decimal { precision, scale } => {
            encoder.u8(13);
            encoder.u8(precision);
            encoder.u8(scale);
        }
        DataType::Date32 => encoder.u8(14),
        DataType::DateTime64 { fsp } => {
            encoder.u8(15);
            encoder.u8(fsp);
        }
        DataType::Time64 { fsp } => {
            encoder.u8(16);
            encoder.u8(fsp);
        }
        DataType::Json => encoder.u8(17),
    }
}

fn key_mode_tag(key_mode: KeyMode) -> u8 {
    match key_mode {
        KeyMode::Primary => 0,
        KeyMode::Unique => 1,
        KeyMode::AppendRowId => 2,
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn write(
    directory: &Path,
    id: u64,
    schema: &TableSchema,
    rows: &[StoredRow],
    block_rows: usize,
    compression: Compression,
    unique_keys: bool,
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
    let mut specs = column_specs(schema);
    // PTSEG v2: a text-carried Decimal/Date32/DateTime64 column whose every
    // value passes the exact round-trip probe is stored as fixed-width
    // units under wire type Int64; one failing value keeps the column on
    // the v1 text path.
    for spec in &mut specs {
        let ColumnSource::Value(index) = spec.source else {
            continue;
        };
        let Some(units) = NativeUnits::for_data_type(schema.columns()[index].data_type()) else {
            continue;
        };
        if probe_native_column(units, rows, index).is_some() {
            spec.logical_type = LogicalType::Int64;
            spec.native = Some(units);
        }
    }
    let file_name = format!("segment-{id:020}.ptseg");
    let path = directory.join(&file_name);
    let temporary = directory.join(format!(".{file_name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| StoreError::io(format!("create {}", temporary.display()), error))?;
    let mut header = Encoder::new();
    header.raw(MAGIC);
    header.u8(FORMAT_VERSION);
    header.u32(schema.version());
    header.u64(fingerprint);
    header.u64(rows.len() as u64);
    header.length(specs.len(), "segment column count")?;
    header.length(block_rows, "segment block row target")?;
    let header = header.finish();
    file.write_all(&header)
        .map_err(|error| StoreError::io(format!("write {}", temporary.display()), error))?;
    let mut position = header.len();
    let mut column_offsets = Vec::with_capacity(specs.len());
    for spec in &specs {
        column_offsets.push(position as u64);
        let mut column = Encoder::new();
        write_column(&mut column, spec, rows, block_rows, compression)?;
        let column = column.finish();
        file.write_all(&column)
            .map_err(|error| StoreError::io(format!("write {}", temporary.display()), error))?;
        position = position.saturating_add(column.len());
    }

    let footer_offset = position as u64;
    let mut footer = Encoder::new();
    footer.raw(FOOTER_MAGIC);
    footer.u64(rows.len() as u64);
    footer.u64(min_version);
    footer.u64(max_version);
    footer.u64(fingerprint);
    footer.u64(rows.len() as u64);
    encode_key(&mut footer, &min_key)?;
    encode_key(&mut footer, &max_key)?;
    footer.length(column_offsets.len(), "footer column count")?;
    for offset in column_offsets {
        footer.u64(offset);
    }

    let sparse_count = rows.len().div_ceil(block_rows);
    footer.length(sparse_count, "sparse primary-key index")?;
    for row_index in (0..rows.len()).step_by(block_rows) {
        footer.u64(row_index as u64);
        encode_key(&mut footer, rows[row_index].key())?;
    }
    footer.bytes(&bloom, "primary-key bloom filter")?;
    let footer_checksum = xxh3_64(footer.as_slice());
    footer.u64(footer_checksum);
    footer.u64(footer_offset);
    file.write_all(&footer.finish())
        .and_then(|()| file.sync_all())
        .map_err(|error| StoreError::io(format!("write {}", temporary.display()), error))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        StoreError::io(
            format!(
                "publish segment {} as {}",
                temporary.display(),
                path.display()
            ),
            error,
        )
    })?;
    sync_directory(directory)?;

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
        unique_keys,
        smas: Some(compute_segment_smas(schema, rows)),
    })
}

pub(crate) fn read(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<Vec<StoredRow>, StoreError> {
    let path = directory.join(&meta.file_name);
    verify(directory, meta, schema)?;

    let mut decoder = FileDecoder::open(&path)?;
    let magic = decoder
        .raw(MAGIC.len())
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    if magic.as_slice() != MAGIC {
        return Err(corrupt(&path, 0, "invalid segment magic"));
    }
    if !format_version_supported(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
    ) {
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

    let columns = read_file_segment_columns(
        &path,
        &mut decoder,
        schema,
        schema_version,
        row_count,
        column_count,
    )?;
    assemble_rows(&path, &decoder, columns, row_count)
}

struct DecodedColumns {
    keys: Vec<Cell>,
    versions: Vec<Cell>,
    tombstones: Vec<Cell>,
    values: Vec<Vec<Cell>>,
}

enum StreamColumnTarget {
    Key,
    Version,
    Tombstone,
    Value(usize),
}

struct StreamColumn {
    decoder: FileDecoder,
    logical_type: LogicalType,
    remaining_blocks: usize,
    target: StreamColumnTarget,
}

/// Bounded row cursor over one immutable columnar segment.
pub(crate) struct SegmentRowStream {
    path: PathBuf,
    columns: Vec<StreamColumn>,
    nullable_absent: Vec<usize>,
    /// Per schema column: the native-unit mapping its type is eligible for,
    /// used to rewrite v2 unit cells back into canonical text.
    native_values: Vec<Option<NativeUnits>>,
    schema_column_count: usize,
    include_values: bool,
    remaining_rows: usize,
    next_physical_index: usize,
    buffered_rows: std::vec::IntoIter<StoredRow>,
}

pub(crate) struct SegmentRowHeader {
    pub(crate) key: PrimaryKey,
    pub(crate) version: u64,
    pub(crate) deleted: bool,
    pub(crate) physical_index: usize,
}

impl SegmentRowStream {
    pub(crate) fn open(
        directory: &Path,
        meta: &SegmentMeta,
        schema: &TableSchema,
    ) -> Result<Self, StoreError> {
        Self::open_internal(directory, meta, schema, true)
    }

    pub(crate) fn open_headers(
        directory: &Path,
        meta: &SegmentMeta,
        schema: &TableSchema,
    ) -> Result<Self, StoreError> {
        Self::open_internal(directory, meta, schema, false)
    }

    #[allow(clippy::too_many_lines)]
    fn open_internal(
        directory: &Path,
        meta: &SegmentMeta,
        schema: &TableSchema,
        include_values: bool,
    ) -> Result<Self, StoreError> {
        verify(directory, meta, schema)?;
        let path = directory.join(&meta.file_name);
        let mut layout = FileDecoder::open(&path)?;
        let magic = layout
            .raw(MAGIC.len())
            .map_err(|reason| corrupt_here(&path, &layout, reason))?;
        if magic.as_slice() != MAGIC {
            return Err(corrupt(&path, 0, "invalid segment magic"));
        }
        if !format_version_supported(
            layout
                .u8()
                .map_err(|reason| corrupt_here(&path, &layout, reason))?,
        ) {
            return Err(corrupt(&path, MAGIC.len(), "unsupported format version"));
        }
        let schema_version = layout
            .u32()
            .map_err(|reason| corrupt_here(&path, &layout, reason))?;
        if schema_version > schema.version() {
            return Err(StoreError::SchemaMismatch {
                expected_version: schema.version(),
                actual_version: schema_version,
            });
        }
        let fingerprint = layout
            .u64()
            .map_err(|reason| corrupt_here(&path, &layout, reason))?;
        if fingerprint != meta.schema_fingerprint {
            return Err(StoreError::SchemaFingerprintMismatch {
                expected: meta.schema_fingerprint,
                actual: fingerprint,
            });
        }
        let remaining_rows = usize::try_from(
            layout
                .u64()
                .map_err(|reason| corrupt_here(&path, &layout, reason))?,
        )
        .map_err(|_| corrupt_here(&path, &layout, "segment row count exceeds usize"))?;
        let column_count = layout
            .u32()
            .map_err(|reason| corrupt_here(&path, &layout, reason))?
            as usize;
        let block_rows = layout
            .u32()
            .map_err(|reason| corrupt_here(&path, &layout, reason))?;
        if block_rows == 0 {
            return Err(corrupt_here(
                &path,
                &layout,
                "segment block row target is zero",
            ));
        }

        let mut columns = Vec::with_capacity(column_count);
        let mut found_key = false;
        let mut found_version = false;
        let mut found_tombstone = false;
        let mut found_values = vec![false; schema.columns().len()];
        let mut expected_blocks = None;
        for _ in 0..column_count {
            let column_offset = layout.decode_position();
            let id = layout
                .u32()
                .map_err(|reason| corrupt_here(&path, &layout, reason))?;
            let logical_type = LogicalType::decode(
                layout
                    .u8()
                    .map_err(|reason| corrupt_here(&path, &layout, reason))?,
            )
            .map_err(|reason| corrupt_here(&path, &layout, reason))?;
            let block_count = layout
                .u32()
                .map_err(|reason| corrupt_here(&path, &layout, reason))?
                as usize;
            if expected_blocks
                .replace(block_count)
                .is_some_and(|count| count != block_count)
            {
                return Err(corrupt_here(&path, &layout, "column block counts differ"));
            }

            let target = match id {
                KEY_COLUMN_ID => {
                    if std::mem::replace(&mut found_key, true)
                        || logical_type != LogicalType::PrimaryKey
                    {
                        return Err(corrupt_here(
                            &path,
                            &layout,
                            "invalid or duplicate primary-key column",
                        ));
                    }
                    Some(StreamColumnTarget::Key)
                }
                VERSION_COLUMN_ID => {
                    if std::mem::replace(&mut found_version, true)
                        || logical_type != LogicalType::UInt64
                    {
                        return Err(corrupt_here(
                            &path,
                            &layout,
                            "invalid or duplicate version column",
                        ));
                    }
                    Some(StreamColumnTarget::Version)
                }
                TOMBSTONE_COLUMN_ID => {
                    if std::mem::replace(&mut found_tombstone, true)
                        || logical_type != LogicalType::Boolean
                    {
                        return Err(corrupt_here(
                            &path,
                            &layout,
                            "invalid or duplicate tombstone column",
                        ));
                    }
                    Some(StreamColumnTarget::Tombstone)
                }
                _ => {
                    let value_target = schema
                        .columns()
                        .iter()
                        .enumerate()
                        .find(|(_, column)| column.id() == id)
                        .map(|(index, column)| {
                            if found_values[index] {
                                return Err(corrupt_here(
                                    &path,
                                    &layout,
                                    format!("duplicate user column id {id}"),
                                ));
                            }
                            if !wire_type_compatible(column.data_type(), logical_type) {
                                return Err(StoreError::IncompatibleSchema(format!(
                                    "column {} ({id}) changed physical type",
                                    column.name()
                                )));
                            }
                            found_values[index] = true;
                            Ok(StreamColumnTarget::Value(index))
                        })
                        .transpose()?;
                    if include_values { value_target } else { None }
                }
            };

            if let Some(target) = target {
                let mut decoder = FileDecoder::open(&path)?;
                decoder
                    .seek_to(column_offset)
                    .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
                let decoded_id = decoder
                    .u32()
                    .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
                let decoded_type = LogicalType::decode(
                    decoder
                        .u8()
                        .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
                )
                .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
                let decoded_blocks = decoder
                    .u32()
                    .map_err(|reason| corrupt_here(&path, &decoder, reason))?
                    as usize;
                if (decoded_id, decoded_type, decoded_blocks) != (id, logical_type, block_count) {
                    return Err(corrupt_here(&path, &decoder, "column layout changed"));
                }
                columns.push(StreamColumn {
                    decoder,
                    logical_type,
                    remaining_blocks: block_count,
                    target,
                });
            }
            for _ in 0..block_count {
                let payload_length = layout
                    .u32()
                    .map_err(|reason| corrupt_here(&path, &layout, reason))?
                    as usize;
                layout
                    .skip(payload_length.saturating_add(8))
                    .map_err(|reason| corrupt_here(&path, &layout, reason))?;
            }
        }
        if !found_key || !found_version || !found_tombstone {
            return Err(corrupt_here(
                &path,
                &layout,
                "segment is missing a system column",
            ));
        }
        let mut nullable_absent = Vec::new();
        for (index, (column, found)) in schema.columns().iter().zip(found_values).enumerate() {
            if found {
                continue;
            }
            if !column.is_nullable() {
                return Err(StoreError::IncompatibleSchema(format!(
                    "required column {} ({}) is absent from schema version {schema_version}",
                    column.name(),
                    column.id()
                )));
            }
            if include_values {
                nullable_absent.push(index);
            }
        }
        Ok(Self {
            path,
            columns,
            nullable_absent,
            native_values: schema
                .columns()
                .iter()
                .map(|column| NativeUnits::for_data_type(column.data_type()))
                .collect(),
            schema_column_count: schema.columns().len(),
            include_values,
            remaining_rows,
            next_physical_index: 0,
            buffered_rows: Vec::new().into_iter(),
        })
    }

    pub(crate) fn next_row(&mut self) -> Result<Option<StoredRow>, StoreError> {
        loop {
            if let Some(row) = self.buffered_rows.next() {
                self.next_physical_index = self.next_physical_index.saturating_add(1);
                return Ok(Some(row));
            }
            if self.remaining_rows == 0 {
                if self
                    .columns
                    .iter()
                    .any(|column| column.remaining_blocks != 0)
                {
                    return Err(corrupt(&self.path, 0, "segment has extra column blocks"));
                }
                return Ok(None);
            }
            self.refill()?;
        }
    }

    pub(crate) fn next_header(&mut self) -> Result<Option<SegmentRowHeader>, StoreError> {
        let Some(row) = self.next_row()? else {
            return Ok(None);
        };
        Ok(Some(SegmentRowHeader {
            key: row.key().clone(),
            version: row.version(),
            deleted: row.is_deleted(),
            physical_index: self.next_physical_index.saturating_sub(1),
        }))
    }

    fn refill(&mut self) -> Result<(), StoreError> {
        let mut keys = None;
        let mut versions = None;
        let mut tombstones = None;
        let value_count = if self.include_values {
            self.schema_column_count
        } else {
            0
        };
        let mut values = vec![None; value_count];
        let mut block_rows = None;
        let mut decode_position = 0;
        for column in &mut self.columns {
            if column.remaining_blocks == 0 {
                return Err(corrupt(
                    &self.path,
                    column.decoder.decode_position(),
                    "column ended before the segment row count",
                ));
            }
            let cells = read_file_block(&self.path, &mut column.decoder, column.logical_type)?;
            column.remaining_blocks -= 1;
            decode_position = column.decoder.decode_position();
            if block_rows
                .replace(cells.len())
                .is_some_and(|count| count != cells.len())
            {
                return Err(corrupt(
                    &self.path,
                    decode_position,
                    "column block row count mismatch",
                ));
            }
            match column.target {
                StreamColumnTarget::Key => keys = Some(cells),
                StreamColumnTarget::Version => versions = Some(cells),
                StreamColumnTarget::Tombstone => tombstones = Some(cells),
                StreamColumnTarget::Value(index) => {
                    let mut cells = cells;
                    if column.logical_type == LogicalType::Int64
                        && let Some(units) = self.native_values[index]
                    {
                        format_native_cells(&self.path, units, &mut cells)?;
                    }
                    values[index] = Some(cells);
                }
            }
        }
        let block_rows =
            block_rows.ok_or_else(|| corrupt(&self.path, 0, "segment has no columns"))?;
        if block_rows == 0 || block_rows > self.remaining_rows {
            return Err(corrupt(
                &self.path,
                decode_position,
                "invalid streamed block row count",
            ));
        }
        for index in &self.nullable_absent {
            values[*index] = Some(vec![Cell::Null; block_rows]);
        }
        let decoded = DecodedColumns {
            keys: keys.ok_or_else(|| corrupt(&self.path, decode_position, "missing key block"))?,
            versions: versions
                .ok_or_else(|| corrupt(&self.path, decode_position, "missing version block"))?,
            tombstones: tombstones
                .ok_or_else(|| corrupt(&self.path, decode_position, "missing tombstone block"))?,
            values: values
                .into_iter()
                .map(|column| {
                    column
                        .ok_or_else(|| corrupt(&self.path, decode_position, "missing value block"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let position = DecodeOffset(decode_position);
        let rows = assemble_rows(&self.path, &position, decoded, block_rows)?;
        self.remaining_rows -= block_rows;
        self.buffered_rows = rows.into_iter();
        Ok(())
    }
}

struct DecodeOffset(usize);

impl DecodePosition for DecodeOffset {
    fn decode_position(&self) -> usize {
        self.0
    }
}

fn read_file_segment_columns(
    path: &Path,
    decoder: &mut FileDecoder,
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
        let (id, logical_type, column_cells) = read_file_column(path, decoder, row_count)?;
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
                if !wire_type_compatible(column.data_type(), logical_type) {
                    return Err(StoreError::IncompatibleSchema(format!(
                        "column {} ({id}) changed physical type",
                        column.name()
                    )));
                }
                let mut column_cells = column_cells;
                if logical_type == LogicalType::Int64
                    && let Some(units) = NativeUnits::for_data_type(column.data_type())
                {
                    format_native_cells(path, units, &mut column_cells)?;
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
    decoder: &impl DecodePosition,
    columns: DecodedColumns,
    row_count: usize,
) -> Result<Vec<StoredRow>, StoreError> {
    let mut rows = Vec::with_capacity(row_count);
    let mut value_columns = columns
        .values
        .into_iter()
        .map(Vec::into_iter)
        .collect::<Vec<_>>();
    let row_cells = columns
        .keys
        .into_iter()
        .zip(columns.versions)
        .zip(columns.tombstones);
    for ((key, version), deleted) in row_cells {
        let Cell::Key(key) = key else {
            return Err(corrupt_here(path, decoder, "invalid key cell"));
        };
        let Cell::UInt64(version) = version else {
            return Err(corrupt_here(path, decoder, "invalid version cell"));
        };
        let Cell::Boolean(deleted) = deleted else {
            return Err(corrupt_here(path, decoder, "invalid tombstone cell"));
        };
        let row_values = value_columns
            .iter_mut()
            .map(|column| {
                column
                    .next()
                    .map(Cell::into_value)
                    .ok_or_else(|| corrupt_here(path, decoder, "missing user value"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(StoredRow::new(key, row_values, version, deleted));
    }
    if rows.len() != row_count
        || value_columns
            .iter_mut()
            .any(|column| column.next().is_some())
    {
        return Err(corrupt_here(
            path,
            decoder,
            "decoded column row counts differ",
        ));
    }
    Ok(rows)
}

/// Loads and checksum-verifies a segment's footer bytes, returning them with
/// the footer offset and the 18-byte header.
fn read_verified_footer(path: &Path) -> Result<(Vec<u8>, usize, [u8; 18]), StoreError> {
    let mut file = File::open(path)
        .map_err(|error| StoreError::io(format!("open segment {}", path.display()), error))?;
    let length = usize::try_from(
        file.metadata()
            .map_err(|error| StoreError::io("stat segment footer", error))?
            .len(),
    )
    .map_err(|_| StoreError::FormatLimit("segment file length exceeds usize".into()))?;
    if length < 18 {
        return Err(corrupt(path, 0, "segment is shorter than its trailer"));
    }

    let mut header = [0_u8; 18];
    file.read_exact(&mut header)
        .map_err(|error| StoreError::io("read segment header", error))?;
    let trailer_offset = length - 16;
    file.seek(SeekFrom::Start(u64::try_from(trailer_offset).map_err(
        |_| StoreError::FormatLimit("segment trailer offset exceeds u64".into()),
    )?))
    .map_err(|error| StoreError::io("seek segment trailer", error))?;
    let mut trailer = [0_u8; 16];
    file.read_exact(&mut trailer)
        .map_err(|error| StoreError::io("read segment trailer", error))?;
    let expected = u64::from_le_bytes(
        trailer[..8]
            .try_into()
            .map_err(|_| corrupt(path, trailer_offset, "invalid footer checksum"))?,
    );
    let footer_offset =
        usize::try_from(u64::from_le_bytes(trailer[8..].try_into().map_err(
            |_| corrupt(path, trailer_offset + 8, "invalid footer offset"),
        )?))
        .map_err(|_| corrupt(path, trailer_offset + 8, "footer offset does not fit usize"))?;
    if footer_offset >= trailer_offset {
        return Err(corrupt(
            path,
            trailer_offset + 8,
            "footer offset is outside segment",
        ));
    }
    file.seek(SeekFrom::Start(u64::try_from(footer_offset).map_err(
        |_| StoreError::FormatLimit("segment footer offset exceeds u64".into()),
    )?))
    .map_err(|error| StoreError::io("seek segment footer", error))?;
    let mut footer = vec![0_u8; trailer_offset - footer_offset];
    file.read_exact(&mut footer)
        .map_err(|error| StoreError::io("read segment footer", error))?;
    if xxh3_64(&footer) != expected {
        return Err(corrupt(path, footer_offset, "footer checksum mismatch"));
    }
    if footer.get(..FOOTER_MAGIC.len()) != Some(FOOTER_MAGIC) {
        return Err(corrupt(path, footer_offset, "invalid footer magic"));
    }
    Ok((footer, footer_offset, header))
}

/// Reads the sparse primary-key index (one `(row ordinal, first key)` entry
/// per block) from a segment's checksum-verified footer.
pub(crate) fn read_sparse_index(
    directory: &Path,
    meta: &SegmentMeta,
) -> Result<Vec<(u64, PrimaryKey)>, StoreError> {
    let path = directory.join(&meta.file_name);
    let (footer, footer_offset, _) = read_verified_footer(&path)?;
    parse_footer_body(&path, &footer, footer_offset, meta)
}

/// Identity of one successful footer verification: the file as it existed
/// on disk (length + mtime) checked against one schema generation. Any
/// on-disk change or schema evolution changes the key and re-verifies.
type VerifiedKey = (PathBuf, u64, i128, u32, u64);

fn verified_segments() -> &'static std::sync::Mutex<HashSet<VerifiedKey>> {
    static VERIFIED: std::sync::OnceLock<std::sync::Mutex<HashSet<VerifiedKey>>> =
        std::sync::OnceLock::new();
    VERIFIED.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

fn verified_key(path: &Path, meta: &SegmentMeta, schema: &TableSchema) -> Option<VerifiedKey> {
    let stat = std::fs::metadata(path).ok()?;
    let modified = stat.modified().ok()?;
    let nanos = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).ok()?,
        Err(before_epoch) => -i128::try_from(before_epoch.duration().as_nanos()).ok()?,
    };
    Some((
        path.to_path_buf(),
        stat.len(),
        nanos,
        schema.version(),
        meta.schema_fingerprint,
    ))
}

pub(crate) fn verify(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<(), StoreError> {
    let path = directory.join(&meta.file_name);
    // Segments are immutable once published: one successful verification per
    // on-disk identity and schema generation covers every later read (block
    // payloads still checksum individually at decode).
    let key = verified_key(&path, meta, schema);
    if let Some(key) = &key
        && verified_segments()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(key)
    {
        return Ok(());
    }
    let (footer, footer_offset, header) = read_verified_footer(&path)?;
    parse_footer_body(&path, &footer, footer_offset, meta)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(corrupt(&path, 0, "invalid segment header"));
    }
    let segment_schema_version = u32::from_le_bytes(
        header[6..10]
            .try_into()
            .map_err(|_| corrupt(&path, 6, "invalid schema version"))?,
    );
    if segment_schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: segment_schema_version,
        });
    }
    let segment_fingerprint = u64::from_le_bytes(
        header[10..18]
            .try_into()
            .map_err(|_| corrupt(&path, 10, "invalid schema fingerprint"))?,
    );
    if meta.schema_fingerprint != segment_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: meta.schema_fingerprint,
            actual: segment_fingerprint,
        });
    }
    if let Some(key) = key {
        let mut verified = verified_segments()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Unbounded growth guard: re-verification is harmless, so a rare
        // full reset beats tracking recency.
        if verified.len() >= 8192 {
            verified.clear();
        }
        verified.insert(key);
    }
    Ok(())
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

pub(crate) struct ProjectedSegmentRow {
    pub(crate) key: PrimaryKey,
    pub(crate) version: u64,
    pub(crate) deleted: bool,
    pub(crate) physical_index: usize,
}

#[derive(Default)]
pub(crate) struct SegmentReadStats {
    pub(crate) read: usize,
    pub(crate) decoded: usize,
    pub(crate) pruned: usize,
}

pub(crate) struct ProjectedSegmentScan {
    pub(crate) rows: Vec<ProjectedSegmentRow>,
    pub(crate) stats: SegmentReadStats,
    pub(crate) reserved_bytes: usize,
}

pub(crate) struct ProjectedValueFetch {
    pub(crate) columns: Vec<Vec<Value>>,
    pub(crate) blocks_decoded: usize,
    pub(crate) reserved_bytes: usize,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn read_row_headers_range(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
    start: &PrimaryKey,
    end: &PrimaryKey,
    memory: &ScanMemoryBudget<'_>,
) -> Result<ProjectedSegmentScan, StoreError> {
    let path = directory.join(&meta.file_name);
    verify(directory, meta, schema)?;
    let mut decoder = FileDecoder::open(&path)?;
    let magic = decoder
        .raw(MAGIC.len())
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    if magic.as_slice() != MAGIC {
        return Err(corrupt(&path, 0, "invalid segment magic"));
    }
    if !format_version_supported(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
    ) {
        return Err(corrupt(&path, MAGIC.len(), "unsupported format version"));
    }
    let segment_schema_version = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    if segment_schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: segment_schema_version,
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
    .map_err(|_| corrupt_here(&path, &decoder, "segment row count exceeds usize"))?;
    let column_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
    let block_rows = decoder
        .u32()
        .map_err(|reason| corrupt_here(&path, &decoder, reason))? as usize;
    if block_rows == 0 {
        return Err(corrupt_here(
            &path,
            &decoder,
            "segment block row target is zero",
        ));
    }
    let block_count_upper = row_count.div_ceil(block_rows);
    let header_reserved = row_count
        .saturating_mul(
            std::mem::size_of::<usize>().saturating_add(std::mem::size_of::<ProjectedSegmentRow>()),
        )
        .saturating_add(
            row_count
                .saturating_mul(3)
                .saturating_mul(std::mem::size_of::<Cell>())
                .saturating_mul(2),
        )
        .saturating_add(block_count_upper.saturating_mul(
            std::mem::size_of::<bool>().saturating_add(std::mem::size_of::<usize>()),
        ));
    memory.reserve(header_reserved)?;
    let mut reserved_bytes = header_reserved;

    let mut selected_blocks = Vec::with_capacity(block_count_upper);
    let mut block_row_counts = Vec::with_capacity(block_count_upper);
    let mut selected_row_indices = Vec::with_capacity(row_count);
    let mut next_row_index = 0;
    let mut keys = None;
    let mut versions = None;
    let mut tombstones = None;
    let mut stats = SegmentReadStats::default();
    for _ in 0..column_count {
        let id = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let logical_type = LogicalType::decode(
            decoder
                .u8()
                .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
        )
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let block_count = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?
            as usize;

        if id == KEY_COLUMN_ID {
            if logical_type != LogicalType::PrimaryKey || !selected_blocks.is_empty() {
                return Err(corrupt_here(
                    &path,
                    &decoder,
                    "invalid or duplicate primary-key column",
                ));
            }
            let mut column_cells = Vec::new();
            for _ in 0..block_count {
                let block = read_file_block_if_bounded(
                    &path,
                    &mut decoder,
                    logical_type,
                    memory,
                    |minimum, maximum| {
                        let minimum = decode_stat_key(&path, minimum).map_err(|reason| {
                            StoreError::CorruptSegment {
                                path: path.clone(),
                                offset: 0,
                                reason,
                            }
                        })?;
                        let maximum = decode_stat_key(&path, maximum).map_err(|reason| {
                            StoreError::CorruptSegment {
                                path: path.clone(),
                                offset: 0,
                                reason,
                            }
                        })?;
                        Ok(minimum <= *end && maximum >= *start)
                    },
                )?;
                let selected = block.cells.is_some();
                reserved_bytes = reserved_bytes.saturating_add(block.reserved_bytes);
                stats.read += usize::from(selected);
                stats.decoded += usize::from(selected);
                stats.pruned += usize::from(!selected);
                selected_blocks.push(selected);
                block_row_counts.push(block.row_count);
                if let Some(cells) = block.cells {
                    selected_row_indices.extend(next_row_index..next_row_index + block.row_count);
                    column_cells.extend(cells);
                }
                next_row_index += block.row_count;
            }
            keys = Some(column_cells);
            continue;
        }
        if selected_blocks.len() != block_count {
            return Err(corrupt_here(
                &path,
                &decoder,
                "column block count differs from primary-key column",
            ));
        }

        let system_column = match id {
            VERSION_COLUMN_ID if logical_type == LogicalType::UInt64 => 1,
            TOMBSTONE_COLUMN_ID if logical_type == LogicalType::Boolean => 2,
            VERSION_COLUMN_ID | TOMBSTONE_COLUMN_ID => {
                return Err(corrupt_here(
                    &path,
                    &decoder,
                    "system column has the wrong logical type",
                ));
            }
            _ => 0,
        };
        let schema_index = schema.columns().iter().position(|column| column.id() == id);
        if let Some(schema_index) = schema_index
            && !wire_type_compatible(schema.columns()[schema_index].data_type(), logical_type)
        {
            return Err(StoreError::IncompatibleSchema(format!(
                "column {} ({id}) changed physical type",
                schema.columns()[schema_index].name()
            )));
        }
        let decode_column = system_column != 0;
        let mut column_cells = Vec::new();
        for (block_index, selected) in selected_blocks.iter().copied().enumerate() {
            let block =
                read_file_block_if_bounded(&path, &mut decoder, logical_type, memory, |_, _| {
                    Ok(selected && decode_column)
                })?;
            if block.row_count != block_row_counts[block_index] {
                return Err(corrupt_here(
                    &path,
                    &decoder,
                    "column block row count mismatch",
                ));
            }
            reserved_bytes = reserved_bytes.saturating_add(block.reserved_bytes);
            stats.decoded += usize::from(block.cells.is_some());
            if let Some(cells) = block.cells {
                column_cells.extend(cells);
            }
        }
        let duplicate = match system_column {
            1 => versions
                .replace(column_cells)
                .is_some()
                .then_some("duplicate version column"),
            2 => tombstones
                .replace(column_cells)
                .is_some()
                .then_some("duplicate tombstone column"),
            _ => None,
        };
        if let Some(message) = duplicate {
            return Err(corrupt_here(&path, &decoder, message));
        }
    }

    let keys = keys.ok_or_else(|| corrupt_here(&path, &decoder, "missing primary-key column"))?;
    let versions =
        versions.ok_or_else(|| corrupt_here(&path, &decoder, "missing version column"))?;
    let tombstones =
        tombstones.ok_or_else(|| corrupt_here(&path, &decoder, "missing tombstone column"))?;
    let cloned_key_bytes = keys
        .iter()
        .filter_map(|cell| match cell {
            Cell::Key(key) if key >= start && key <= end => Some(
                key.parts()
                    .len()
                    .saturating_mul(std::mem::size_of::<pintail_types::KeyPart>())
                    .saturating_add(key.heap_bytes()),
            ),
            _ => None,
        })
        .fold(0_usize, usize::saturating_add);
    memory.reserve(cloned_key_bytes)?;
    reserved_bytes = reserved_bytes.saturating_add(cloned_key_bytes);
    let mut rows = Vec::with_capacity(row_count);
    for row_index in 0..keys.len() {
        let Cell::Key(key) = &keys[row_index] else {
            return Err(corrupt_here(&path, &decoder, "invalid primary-key cell"));
        };
        if key < start || key > end {
            continue;
        }
        let Cell::UInt64(version) = versions[row_index] else {
            return Err(corrupt_here(&path, &decoder, "invalid version cell"));
        };
        let Cell::Boolean(deleted) = tombstones[row_index] else {
            return Err(corrupt_here(&path, &decoder, "invalid tombstone cell"));
        };
        rows.push(ProjectedSegmentRow {
            key: key.clone(),
            version,
            deleted,
            physical_index: selected_row_indices[row_index],
        });
    }
    Ok(ProjectedSegmentScan {
        rows,
        stats,
        reserved_bytes,
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) fn read_projected_rows(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
    projection: &[usize],
    row_indices: &[usize],
    memory: &ScanMemoryBudget<'_>,
) -> Result<ProjectedValueFetch, StoreError> {
    if row_indices.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::FormatLimit(
            "late-materialization row indices must be strictly increasing".into(),
        ));
    }
    let path = directory.join(&meta.file_name);
    verify(directory, meta, schema)?;
    let mut decoder = FileDecoder::open(&path)?;
    let header = read_segment_columns_header(&path, &mut decoder, meta, schema)?;
    let row_count = header.row_count;
    let block_rows = header.block_rows;
    let column_count = header.column_count;
    if row_indices.iter().any(|index| *index >= row_count) {
        return Err(StoreError::FormatLimit(
            "late-materialization row index exceeds segment row count".into(),
        ));
    }

    let column_matrix_reserved = projection
        .len()
        .saturating_mul(
            std::mem::size_of::<Vec<Value>>().saturating_add(
                row_indices
                    .len()
                    .saturating_mul(std::mem::size_of::<Value>()),
            ),
        )
        .saturating_add(projection.len().saturating_mul(std::mem::size_of::<bool>()));
    memory.reserve(column_matrix_reserved)?;
    let mut reserved_bytes = column_matrix_reserved;
    let mut columns = vec![vec![Value::Null; row_indices.len()]; projection.len()];
    let mut found = vec![false; projection.len()];
    let mut blocks_decoded = 0;
    for _ in 0..column_count {
        let id = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let logical_type = LogicalType::decode(
            decoder
                .u8()
                .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
        )
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let block_count = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?
            as usize;
        let schema_index = schema.columns().iter().position(|column| column.id() == id);
        if let Some(schema_index) = schema_index
            && !wire_type_compatible(schema.columns()[schema_index].data_type(), logical_type)
        {
            return Err(StoreError::IncompatibleSchema(format!(
                "column {} ({id}) changed physical type",
                schema.columns()[schema_index].name()
            )));
        }
        let projected_position = schema_index
            .and_then(|schema_index| projection.iter().position(|value| *value == schema_index));
        if let Some(position) = projected_position
            && std::mem::replace(&mut found[position], true)
        {
            return Err(corrupt_here(&path, &decoder, "duplicate user column"));
        }

        let mut block_start = 0_usize;
        for _ in 0..block_count {
            let block_limit = block_start.saturating_add(block_rows);
            let selected = projected_position.is_some()
                && row_indices
                    .iter()
                    .any(|index| *index >= block_start && *index < block_limit);
            let block =
                read_file_block_if_bounded(&path, &mut decoder, logical_type, memory, |_, _| {
                    Ok(selected)
                })?;
            reserved_bytes = reserved_bytes.saturating_add(block.reserved_bytes);
            let block_end = block_start
                .checked_add(block.row_count)
                .ok_or_else(|| corrupt_here(&path, &decoder, "column row count overflow"))?;
            if let (Some(position), Some(mut cells)) = (projected_position, block.cells) {
                blocks_decoded += 1;
                if logical_type == LogicalType::Int64
                    && let Some(units) = schema_index.and_then(|index| {
                        NativeUnits::for_data_type(schema.columns()[index].data_type())
                    })
                {
                    format_native_cells(&path, units, &mut cells)?;
                }
                let cloned_value_bytes = row_indices
                    .iter()
                    .copied()
                    .filter(|row_index| *row_index >= block_start && *row_index < block_end)
                    .map(|row_index| cells[row_index - block_start].heap_bytes())
                    .fold(0_usize, usize::saturating_add);
                memory.reserve(cloned_value_bytes)?;
                reserved_bytes = reserved_bytes.saturating_add(cloned_value_bytes);
                for (result_index, row_index) in row_indices.iter().copied().enumerate() {
                    if row_index < block_start || row_index >= block_end {
                        continue;
                    }
                    columns[position][result_index] = cells[row_index - block_start].to_value();
                }
            }
            block_start = block_end;
        }
        if block_start != row_count {
            return Err(corrupt_here(
                &path,
                &decoder,
                "column row count differs from segment header",
            ));
        }
    }

    for (position, schema_index) in projection.iter().copied().enumerate() {
        if found[position] {
            continue;
        }
        let column = &schema.columns()[schema_index];
        if !column.is_nullable() {
            return Err(StoreError::IncompatibleSchema(format!(
                "required projected column {} ({}) is absent",
                column.name(),
                column.id()
            )));
        }
    }
    Ok(ProjectedValueFetch {
        columns,
        blocks_decoded,
        reserved_bytes,
    })
}

/// The fixed segment header preceding the column directory.
struct SegmentColumnsHeader {
    row_count: usize,
    column_count: u32,
    block_rows: usize,
}

fn read_segment_columns_header(
    path: &Path,
    decoder: &mut FileDecoder,
    meta: &SegmentMeta,
    schema: &TableSchema,
) -> Result<SegmentColumnsHeader, StoreError> {
    if decoder
        .raw(MAGIC.len())
        .map_err(|reason| corrupt(path, 0, reason))?
        != MAGIC
    {
        return Err(corrupt(path, 0, "invalid segment magic"));
    }
    if !format_version_supported(
        decoder
            .u8()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    ) {
        return Err(corrupt(path, MAGIC.len(), "unsupported format version"));
    }
    let segment_schema_version = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if segment_schema_version > schema.version() {
        return Err(StoreError::SchemaMismatch {
            expected_version: schema.version(),
            actual_version: segment_schema_version,
        });
    }
    let fingerprint = decoder
        .u64()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if fingerprint != meta.schema_fingerprint {
        return Err(StoreError::SchemaFingerprintMismatch {
            expected: meta.schema_fingerprint,
            actual: fingerprint,
        });
    }
    let row_count = usize::try_from(
        decoder
            .u64()
            .map_err(|reason| corrupt_here(path, decoder, reason))?,
    )
    .map_err(|_| corrupt_here(path, decoder, "segment row count exceeds usize"))?;
    let column_count = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let block_rows = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    if block_rows == 0 {
        return Err(corrupt_here(
            path,
            decoder,
            "segment block row target is zero",
        ));
    }
    Ok(SegmentColumnsHeader {
        row_count,
        column_count,
        block_rows,
    })
}

/// Packed columns produced by [`read_projected_columns`].
pub(crate) struct ProjectedColumnFetch {
    pub(crate) columns: Vec<DecodedColumn>,
    pub(crate) blocks_decoded: usize,
    /// Blocks whose payload was fetched, and blocks skipped because no
    /// requested row range touched them. Counted the same way the merge
    /// path counts them, so the two paths report comparable statistics
    /// rather than the direct path silently reporting nothing.
    pub(crate) blocks_read: usize,
    pub(crate) blocks_pruned: usize,
    pub(crate) reserved_bytes: usize,
}

/// An in-progress [`DecodedColumn`] receiving one block of cells at a time.
enum ColumnBuilder {
    Int64 {
        values: Vec<i64>,
        validity: Vec<bool>,
    },
    UInt64 {
        values: Vec<u64>,
        validity: Vec<bool>,
    },
    Float64 {
        bits: Vec<u64>,
        validity: Vec<bool>,
    },
    Utf8 {
        heap: Vec<u8>,
        offsets: Vec<usize>,
        validity: Vec<bool>,
    },
    /// Dictionary-coded UTF-8: block dictionaries remap into one chunk
    /// dictionary and rows accumulate as u32 codes. Degrades to the arena
    /// shape if a non-dictionary block interrupts.
    DictUtf8 {
        dict_heap: Vec<u8>,
        dict_offsets: Vec<usize>,
        codes: Vec<u32>,
        validity: Vec<bool>,
    },
    /// A v2 native-unit column: unit cells accumulate packed and flow to
    /// the executor untouched (step B); text regenerates only where a
    /// consumer asks.
    NativeUnits {
        units: NativeUnits,
        values: Vec<i64>,
        validity: Vec<bool>,
    },
    Values(Vec<Value>),
}

impl ColumnBuilder {
    /// Chooses the builder for one projected column: native-unit columns
    /// build text by formatting units; everything else follows the wire
    /// type.
    fn new_for_column(
        logical_type: LogicalType,
        native: Option<NativeUnits>,
        capacity: usize,
    ) -> Self {
        if let Some(units) = native {
            return Self::NativeUnits {
                units,
                values: Vec::with_capacity(capacity),
                validity: Vec::with_capacity(capacity),
            };
        }
        Self::new(logical_type, capacity)
    }

    fn new(logical_type: LogicalType, capacity: usize) -> Self {
        match logical_type {
            LogicalType::Int64 => Self::Int64 {
                values: Vec::with_capacity(capacity),
                validity: Vec::with_capacity(capacity),
            },
            LogicalType::UInt64 => Self::UInt64 {
                values: Vec::with_capacity(capacity),
                validity: Vec::with_capacity(capacity),
            },
            LogicalType::Float64 => Self::Float64 {
                bits: Vec::with_capacity(capacity),
                validity: Vec::with_capacity(capacity),
            },
            LogicalType::Utf8 => {
                let mut offsets = Vec::with_capacity(capacity.saturating_add(1));
                offsets.push(0);
                Self::Utf8 {
                    heap: Vec::new(),
                    offsets,
                    validity: Vec::with_capacity(capacity),
                }
            }
            LogicalType::Boolean | LogicalType::Binary | LogicalType::PrimaryKey => {
                Self::Values(Vec::with_capacity(capacity))
            }
        }
    }

    fn push(&mut self, cell: Cell) -> Result<(), String> {
        match (&mut *self, cell) {
            (
                Self::Int64 { values, validity }
                | Self::NativeUnits {
                    values, validity, ..
                },
                Cell::Int64(value),
            ) => {
                values.push(value);
                validity.push(true);
            }
            (
                Self::Int64 { values, validity }
                | Self::NativeUnits {
                    values, validity, ..
                },
                Cell::Null,
            ) => {
                values.push(0);
                validity.push(false);
            }
            (Self::UInt64 { values, validity }, Cell::UInt64(value)) => {
                values.push(value);
                validity.push(true);
            }
            (Self::UInt64 { values, validity }, Cell::Null) => {
                values.push(0);
                validity.push(false);
            }
            (Self::Float64 { bits, validity }, Cell::Float64(value)) => {
                bits.push(value);
                validity.push(true);
            }
            (Self::Float64 { bits, validity }, Cell::Null) => {
                bits.push(0);
                validity.push(false);
            }
            (
                Self::Utf8 {
                    heap,
                    offsets,
                    validity,
                },
                Cell::Utf8(text),
            ) => {
                heap.extend_from_slice(text.as_bytes());
                offsets.push(heap.len());
                validity.push(true);
            }
            (
                Self::Utf8 {
                    offsets, validity, ..
                },
                Cell::Null,
            ) => {
                let end = *offsets.last().expect("offsets seeded with zero");
                offsets.push(end);
                validity.push(false);
            }
            (this @ Self::DictUtf8 { .. }, cell @ (Cell::Utf8(_) | Cell::Null)) => {
                this.degrade_dictionary();
                return this.push(cell);
            }
            (Self::Values(_), Cell::Key(_)) => {
                return Err("primary-key cell in a user column block".to_owned());
            }
            (Self::Values(values), cell) => values.push(cell.into_value()),
            _ => return Err("block cell type differs from its column type".to_owned()),
        }
        Ok(())
    }

    /// Registers one block's dictionary, upgrading an empty arena builder
    /// to dictionary mode. Returns per-block-entry translations into the
    /// chunk dictionary, or `None` when this builder cannot take codes
    /// (arena rows already accumulated, or a non-Utf8 shape).
    fn begin_dictionary_block(&mut self, entries: &[&[u8]]) -> Option<Vec<u32>> {
        if let Self::Utf8 {
            heap,
            offsets,
            validity,
        } = self
        {
            if !validity.is_empty() || !heap.is_empty() {
                return None;
            }
            let capacity = validity.capacity();
            let _ = (heap, offsets);
            *self = Self::DictUtf8 {
                dict_heap: Vec::new(),
                dict_offsets: vec![0],
                codes: Vec::with_capacity(capacity),
                validity: Vec::with_capacity(capacity),
            };
        }
        let Self::DictUtf8 {
            dict_heap,
            dict_offsets,
            ..
        } = self
        else {
            return None;
        };
        let translation = entries
            .iter()
            .map(|entry| {
                let existing = (0..dict_offsets.len() - 1).find(|index| {
                    &dict_heap[dict_offsets[*index]..dict_offsets[index + 1]] == *entry
                });
                if let Some(index) = existing {
                    u32::try_from(index).expect("chunk dictionary fits u32")
                } else {
                    dict_heap.extend_from_slice(entry);
                    dict_offsets.push(dict_heap.len());
                    u32::try_from(dict_offsets.len() - 2).expect("chunk dictionary fits u32")
                }
            })
            .collect();
        Some(translation)
    }

    /// Bulk code append for the no-null full-range decode fast path:
    /// block codes translate to chunk codes through `translation` (the
    /// container's snapshot-written segments rarely carry the identity
    /// mapping, so the translated form is the one that matters).
    fn push_codes_bulk(&mut self, raw: &[u8], translation: &[u32]) -> Result<(), String> {
        match self {
            Self::DictUtf8 {
                codes, validity, ..
            } => {
                codes.reserve(raw.len() / 4);
                validity.reserve(raw.len() / 4);
                for chunk in raw.chunks_exact(4) {
                    let block_code =
                        u32::from_le_bytes(chunk.try_into().expect("4-byte code")) as usize;
                    let chunk_code = *translation
                        .get(block_code)
                        .ok_or("dictionary index is out of bounds")?;
                    codes.push(chunk_code);
                    validity.push(true);
                }
                Ok(())
            }
            _ => Err("dictionary code in a non-dictionary column".to_owned()),
        }
    }

    /// Typed append for integer-wire decode: the same conversions as
    /// `integer_from_i128` without building a `Cell`.
    fn push_integer(&mut self, value: i128) -> Result<(), String> {
        match self {
            Self::Int64 { values, validity }
            | Self::NativeUnits {
                values, validity, ..
            } => {
                values
                    .push(i64::try_from(value).map_err(|_| "bit-packed signed integer overflow")?);
                validity.push(true);
                Ok(())
            }
            Self::UInt64 { values, validity } => {
                values.push(
                    u64::try_from(value).map_err(|_| "bit-packed unsigned integer overflow")?,
                );
                validity.push(true);
                Ok(())
            }
            _ => Err("integer decode into a non-integer column".to_owned()),
        }
    }

    fn push_code(&mut self, code: u32) -> Result<(), String> {
        match self {
            Self::DictUtf8 {
                codes, validity, ..
            } => {
                codes.push(code);
                validity.push(true);
                Ok(())
            }
            _ => Err("dictionary code in a non-dictionary column".to_owned()),
        }
    }

    fn push_null_code(&mut self) -> Result<(), String> {
        match self {
            Self::DictUtf8 {
                codes, validity, ..
            } => {
                codes.push(0);
                validity.push(false);
                Ok(())
            }
            _ => Err("dictionary code in a non-dictionary column".to_owned()),
        }
    }

    /// Converts an accumulated dictionary builder back to the arena shape,
    /// for chunks whose blocks mix encodings.
    fn degrade_dictionary(&mut self) {
        if std::env::var_os("PINTAIL_DECODE_DEBUG").is_some()
            && matches!(self, Self::DictUtf8 { .. })
        {
            eprintln!("[decode] dictionary degraded");
        }
        if let Self::DictUtf8 {
            dict_heap,
            dict_offsets,
            codes,
            validity,
        } = self
        {
            let mut heap = Vec::new();
            let mut offsets = Vec::with_capacity(codes.len() + 1);
            offsets.push(0);
            for (code, valid) in codes.iter().zip(validity.iter()) {
                if *valid {
                    let code = *code as usize;
                    heap.extend_from_slice(&dict_heap[dict_offsets[code]..dict_offsets[code + 1]]);
                }
                offsets.push(heap.len());
            }
            *self = Self::Utf8 {
                heap,
                offsets,
                validity: std::mem::take(validity),
            };
        }
    }

    /// Appends one pre-validated UTF-8 value without an intermediate `Cell`.
    fn push_utf8(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self {
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => {
                heap.extend_from_slice(bytes);
                offsets.push(heap.len());
                validity.push(true);
                Ok(())
            }
            _ => Err("string block value in a non-string column".to_owned()),
        }
    }

    /// Bytes retained by owned buffers, by length (used for reserve deltas).
    fn heap_len_bytes(&self) -> usize {
        match self {
            Self::Int64 { values, validity }
            | Self::NativeUnits {
                values, validity, ..
            } => values
                .len()
                .saturating_mul(std::mem::size_of::<i64>())
                .saturating_add(validity.len()),
            Self::DictUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => dict_heap
                .len()
                .saturating_add(
                    dict_offsets
                        .len()
                        .saturating_mul(std::mem::size_of::<usize>()),
                )
                .saturating_add(codes.len().saturating_mul(std::mem::size_of::<u32>()))
                .saturating_add(validity.len()),
            Self::UInt64 { values, validity } => values
                .len()
                .saturating_mul(std::mem::size_of::<u64>())
                .saturating_add(validity.len()),
            Self::Float64 { bits, validity } => bits
                .len()
                .saturating_mul(std::mem::size_of::<u64>())
                .saturating_add(validity.len()),
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => heap
                .len()
                .saturating_add(offsets.len().saturating_mul(std::mem::size_of::<usize>()))
                .saturating_add(validity.len()),
            Self::Values(values) => values
                .len()
                .saturating_mul(std::mem::size_of::<Value>())
                .saturating_add(values.iter().map(Value::heap_bytes).sum()),
        }
    }

    fn finish(self) -> DecodedColumn {
        match self {
            Self::Int64 { values, validity } => DecodedColumn::Int64 { values, validity },
            Self::UInt64 { values, validity } => DecodedColumn::UInt64 { values, validity },
            Self::Float64 { bits, validity } => DecodedColumn::Float64 { bits, validity },
            Self::Utf8 {
                heap,
                offsets,
                validity,
            } => DecodedColumn::Utf8 {
                heap,
                offsets,
                validity,
            },
            Self::NativeUnits {
                units,
                values,
                validity,
            } => DecodedColumn::NativeUnits {
                units,
                values,
                validity,
            },
            Self::DictUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            } => DecodedColumn::DictionaryUtf8 {
                dict_heap,
                dict_offsets,
                codes,
                validity,
            },
            Self::Values(values) => DecodedColumn::Values(values),
        }
    }
}

/// Decodes one contiguous row range of every projected column into packed
/// columnar storage, skipping blocks wholly outside the range.
#[allow(clippy::too_many_lines)]
pub(crate) fn read_projected_columns(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
    projection: &[usize],
    start_row: usize,
    end_row: usize,
    memory: &ScanMemoryBudget<'_>,
) -> Result<ProjectedColumnFetch, StoreError> {
    read_projected_column_ranges(
        directory,
        meta,
        schema,
        projection,
        std::slice::from_ref(&(start_row..end_row)),
        memory,
    )
}

/// Decodes several disjoint ascending row ranges of every projected column
/// into packed columnar storage, skipping blocks wholly outside every range
/// (the storage primitive behind filter-first late materialization).
#[allow(clippy::too_many_lines)]
pub(crate) fn read_projected_column_ranges(
    directory: &Path,
    meta: &SegmentMeta,
    schema: &TableSchema,
    projection: &[usize],
    ranges: &[std::ops::Range<usize>],
    memory: &ScanMemoryBudget<'_>,
) -> Result<ProjectedColumnFetch, StoreError> {
    let path = directory.join(&meta.file_name);
    verify(directory, meta, schema)?;
    let mut decoder = FileDecoder::open(&path)?;
    let header = read_segment_columns_header(&path, &mut decoder, meta, schema)?;
    let mut previous_end = 0_usize;
    for range in ranges {
        if range.start > range.end || range.end > header.row_count || range.start < previous_end {
            return Err(StoreError::FormatLimit(
                "projected row ranges must be ascending, disjoint, and in bounds".into(),
            ));
        }
        previous_end = range.end;
    }
    let selected_rows = ranges.iter().map(std::ops::Range::len).sum::<usize>();
    let mut builders: Vec<Option<ColumnBuilder>> = (0..projection.len()).map(|_| None).collect();
    let mut found = vec![false; projection.len()];
    let mut reserved_bytes = 0_usize;
    let mut blocks_decoded = 0_usize;
    // Counted alongside the decode tally so this path reports the same
    // statistics the merge path does; without them a caller cannot tell a
    // pruned block from an unreported one.
    let mut blocks_read = 0_usize;
    let mut blocks_pruned = 0_usize;
    for _ in 0..header.column_count {
        let id = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let logical_type = LogicalType::decode(
            decoder
                .u8()
                .map_err(|reason| corrupt_here(&path, &decoder, reason))?,
        )
        .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
        let block_count = decoder
            .u32()
            .map_err(|reason| corrupt_here(&path, &decoder, reason))?
            as usize;
        let schema_index = schema.columns().iter().position(|column| column.id() == id);
        if let Some(schema_index) = schema_index
            && !wire_type_compatible(schema.columns()[schema_index].data_type(), logical_type)
        {
            return Err(StoreError::IncompatibleSchema(format!(
                "column {} ({id}) changed physical type",
                schema.columns()[schema_index].name()
            )));
        }
        let projected_position = schema_index
            .and_then(|schema_index| projection.iter().position(|value| *value == schema_index));
        if let Some(position) = projected_position {
            if std::mem::replace(&mut found[position], true) {
                return Err(corrupt_here(&path, &decoder, "duplicate user column"));
            }
            let native = (logical_type == LogicalType::Int64)
                .then(|| {
                    schema_index.and_then(|index| {
                        NativeUnits::for_data_type(schema.columns()[index].data_type())
                    })
                })
                .flatten();
            let builder = ColumnBuilder::new_for_column(logical_type, native, selected_rows);
            let builder_bytes = builder.heap_len_bytes();
            memory.reserve(builder_bytes)?;
            reserved_bytes = reserved_bytes.saturating_add(builder_bytes);
            builders[position] = Some(builder);
        }
        let mut block_start = 0_usize;
        for _ in 0..block_count {
            let block_limit = block_start.saturating_add(header.block_rows);
            let selected = projected_position.is_some()
                && ranges
                    .iter()
                    .any(|range| block_start < range.end && block_limit > range.start);
            if projected_position.is_some() {
                if selected {
                    blocks_read += 1;
                } else {
                    blocks_pruned += 1;
                }
            }
            // Block-relative intersections of the requested ranges, clamped
            // to the target block span (the last block may be shorter; row
            // loops clamp naturally).
            let block_ranges = || {
                ranges
                    .iter()
                    .filter(|range| block_start < range.end && block_limit > range.start)
                    .map(|range| {
                        (
                            range.start.saturating_sub(block_start),
                            range.end.saturating_sub(block_start),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let int_eligible = matches!(logical_type, LogicalType::Int64 | LogicalType::UInt64)
                && projected_position.is_some_and(|position| {
                    matches!(
                        builders[position],
                        Some(
                            ColumnBuilder::Int64 { .. }
                                | ColumnBuilder::UInt64 { .. }
                                | ColumnBuilder::NativeUnits { .. }
                        )
                    )
                });
            let block = if let (Some(position), true, true) =
                (projected_position, selected, int_eligible)
            {
                // Integer-wire blocks decode straight into the typed
                // builder; non-bit-packed encodings fall back to cells
                // below (issue #6 WS2 — the ranged read built a Vec<Cell>
                // per selected block for every filtered scan).
                let builder = builders[position]
                    .as_mut()
                    .expect("builder exists for a projected column");
                let before = builder.heap_len_bytes();
                let block = read_file_block_int_into(
                    &path,
                    &mut decoder,
                    logical_type,
                    memory,
                    IntSink {
                        builder,
                        ranges: RangeCursor::new(block_ranges()),
                    },
                )?;
                if block.cells.is_none() {
                    blocks_decoded += 1;
                }
                let builder = builders[position]
                    .as_ref()
                    .expect("builder exists for a projected column");
                let appended = builder.heap_len_bytes().saturating_sub(before);
                memory.reserve(appended)?;
                reserved_bytes = reserved_bytes.saturating_add(appended);
                block
            } else if let (Some(position), true, LogicalType::Utf8) =
                (projected_position, selected, logical_type)
            {
                // String blocks decode straight into the column arena — no
                // per-cell String allocation on the columnar scan path.
                let builder = builders[position]
                    .as_mut()
                    .expect("builder exists for a projected column");
                let before = builder.heap_len_bytes();
                let block = read_file_block_utf8_into(
                    &path,
                    &mut decoder,
                    memory,
                    Utf8Sink {
                        builder,
                        ranges: RangeCursor::new(block_ranges()),
                    },
                )?;
                blocks_decoded += 1;
                let builder = builders[position]
                    .as_ref()
                    .expect("builder exists for a projected column");
                let appended = builder.heap_len_bytes().saturating_sub(before);
                memory.reserve(appended)?;
                reserved_bytes = reserved_bytes.saturating_add(appended);
                block
            } else {
                read_file_block_if_bounded(&path, &mut decoder, logical_type, memory, |_, _| {
                    Ok(selected)
                })?
            };
            reserved_bytes = reserved_bytes.saturating_add(block.reserved_bytes);
            let block_end = block_start
                .checked_add(block.row_count)
                .ok_or_else(|| corrupt_here(&path, &decoder, "column row count overflow"))?;
            if let (Some(position), Some(cells)) = (projected_position, block.cells) {
                blocks_decoded += 1;
                let builder = builders[position]
                    .as_mut()
                    .expect("builder exists for a projected column");
                let before = builder.heap_len_bytes();
                let block_rows = block_end - block_start;
                let mut cells = cells.into_iter();
                let mut consumed = 0_usize;
                for (lo, hi) in block_ranges() {
                    let lo = lo.min(block_rows);
                    let hi = hi.min(block_rows);
                    if lo >= hi {
                        continue;
                    }
                    for _ in consumed..lo {
                        cells.next();
                    }
                    for cell in cells.by_ref().take(hi - lo) {
                        builder
                            .push(cell)
                            .map_err(|reason| corrupt_here(&path, &decoder, reason))?;
                    }
                    consumed = hi;
                }
                let appended = builder.heap_len_bytes().saturating_sub(before);
                memory.reserve(appended)?;
                reserved_bytes = reserved_bytes.saturating_add(appended);
            }
            block_start = block_end;
        }
        if block_start != header.row_count {
            return Err(corrupt_here(
                &path,
                &decoder,
                "column row count differs from segment header",
            ));
        }
    }

    let mut columns = Vec::with_capacity(projection.len());
    for (position, schema_index) in projection.iter().copied().enumerate() {
        if found[position] {
            let column = builders[position]
                .take()
                .expect("found column has a builder")
                .finish();
            if column.len() != selected_rows {
                return Err(corrupt_here(
                    &path,
                    &decoder,
                    "projected column row count differs from the requested range",
                ));
            }
            columns.push(column);
            continue;
        }
        let column = &schema.columns()[schema_index];
        if !column.is_nullable() {
            return Err(StoreError::IncompatibleSchema(format!(
                "required projected column {} ({}) is absent",
                column.name(),
                column.id()
            )));
        }
        let null_bytes = selected_rows.saturating_mul(std::mem::size_of::<Value>());
        memory.reserve(null_bytes)?;
        reserved_bytes = reserved_bytes.saturating_add(null_bytes);
        columns.push(DecodedColumn::Values(vec![Value::Null; selected_rows]));
    }
    Ok(ProjectedColumnFetch {
        columns,
        blocks_decoded,
        blocks_read,
        blocks_pruned,
        reserved_bytes,
    })
}

fn decode_stat_key(path: &Path, bytes: &[u8]) -> Result<PrimaryKey, String> {
    let mut decoder = Decoder::new(bytes);
    let key = decode_key(&mut decoder)
        .map_err(|reason| format!("invalid key statistic in {}: {reason}", path.display()))?;
    decoder.finish()?;
    Ok(key)
}

fn column_specs(schema: &TableSchema) -> Vec<ColumnSpec> {
    let mut specs = vec![
        ColumnSpec {
            id: KEY_COLUMN_ID,
            logical_type: LogicalType::PrimaryKey,
            source: ColumnSource::Key,
            native: None,
        },
        ColumnSpec {
            id: VERSION_COLUMN_ID,
            logical_type: LogicalType::UInt64,
            source: ColumnSource::Version,
            native: None,
        },
        ColumnSpec {
            id: TOMBSTONE_COLUMN_ID,
            logical_type: LogicalType::Boolean,
            source: ColumnSource::Tombstone,
            native: None,
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
                native: None,
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
    let mut block = Encoder::new();
    block.length(cells.len(), "block row count")?;
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
    block.bytes(&null_bitmap, "null bitmap")?;
    let encoding = select_encoding(logical_type, &non_null);
    block.u8(encoding as u8);
    block.u8(compression as u8);
    let uncompressed = encode_payload(logical_type, encoding, &non_null)?;
    block.length(uncompressed.len(), "uncompressed block")?;
    let compressed = compress_block(compression, &uncompressed)?;
    block.bytes(&compressed, "compressed block")?;
    block.u32(null_count);

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
    block.bytes(&min, "block minimum")?;
    block.bytes(&max, "block maximum")?;
    block.bytes(&hll_registers(&encoded_values), "block HLL sketch")?;
    let payload = block.finish();
    encoder.bytes(&payload, "column block")?;
    encoder.u64(xxh3_64(&payload));
    Ok(())
}

fn read_file_column(
    path: &Path,
    decoder: &mut FileDecoder,
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
        cells.extend(read_file_block(path, decoder, logical_type)?);
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
    decoder: &impl DecodePosition,
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
    read_block_if(path, decoder, logical_type, |_, _| Ok(true))?
        .cells
        .ok_or_else(|| corrupt_here(path, decoder, "selected block was not decoded"))
}

fn read_file_block(
    path: &Path,
    decoder: &mut FileDecoder,
    logical_type: LogicalType,
) -> Result<Vec<Cell>, StoreError> {
    let block_offset = decoder.decode_position();
    let payload_length = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let encoded_length = payload_length.saturating_add(12);
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.extend_from_slice(
        &u32::try_from(payload_length)
            .map_err(|_| StoreError::FormatLimit("block payload exceeds u32::MAX".into()))?
            .to_le_bytes(),
    );
    encoded.resize(payload_length.saturating_add(4), 0);
    decoder
        .read_exact(&mut encoded[4..])
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    encoded.extend_from_slice(
        &decoder
            .u64()
            .map_err(|reason| corrupt_here(path, decoder, reason))?
            .to_le_bytes(),
    );
    let mut block_decoder = Decoder::with_base_offset(&encoded, block_offset);
    read_block(path, &mut block_decoder, logical_type)
}

struct BlockRead {
    row_count: usize,
    cells: Option<Vec<Cell>>,
    reserved_bytes: usize,
}

/// A columnar destination for one string block: rows in `lo..hi`
/// (block-relative) append straight into the builder's arena, so decode
/// allocates no per-cell `String`.
struct Utf8Sink<'a> {
    builder: &'a mut ColumnBuilder,
    ranges: RangeCursor,
}

/// A columnar destination for one integer-wire block: selected rows append
/// straight into the builder's typed vector — no `Vec<Cell>` is built (the
/// prewhere ranged read materialized one per block, issue #6 WS2).
struct IntSink<'a> {
    builder: &'a mut ColumnBuilder,
    ranges: RangeCursor,
}

/// Bit-packed integer payload straight into an int-typed builder. Returns
/// `false` (sink untouched) for encodings this fast path does not cover —
/// the caller falls back to the generic cell decode.
fn decode_int_payload_into(
    bytes: &[u8],
    logical_type: LogicalType,
    encoding: Encoding,
    row_count: usize,
    non_null_count: usize,
    null_bitmap: &[u8],
    sink: IntSink<'_>,
) -> Result<bool, String> {
    if !matches!(encoding, Encoding::BitPacked) {
        return Ok(false);
    }
    let IntSink {
        builder,
        mut ranges,
    } = sink;
    let mut decoder = Decoder::new(bytes);
    let base = decode_integer_base(&mut decoder, logical_type)?;
    let normalized = unpack(&mut decoder, non_null_count)?;
    decoder.finish()?;
    let is_null = |row: usize| null_bitmap[row / 8] & (1 << (row % 8)) != 0;
    let mut next = 0_usize;
    for row in 0..row_count {
        if is_null(row) {
            if ranges.contains(row) {
                builder.push(Cell::Null)?;
            }
            continue;
        }
        let normalized = *normalized
            .get(next)
            .ok_or("encoding produced too few values")?;
        next += 1;
        if ranges.contains(row) {
            let value = base
                .checked_add(i128::from(normalized))
                .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
            builder.push_integer(value)?;
        }
    }
    if next != non_null_count {
        return Err("encoding produced too few values".to_owned());
    }
    Ok(true)
}

/// Ascending membership test over sorted, disjoint block-relative row
/// ranges; rows must be queried in ascending order.
struct RangeCursor {
    ranges: Vec<(usize, usize)>,
    index: usize,
}

impl RangeCursor {
    fn new(ranges: Vec<(usize, usize)>) -> Self {
        Self { ranges, index: 0 }
    }

    fn contains(&mut self, row: usize) -> bool {
        while self.index < self.ranges.len() && self.ranges[self.index].1 <= row {
            self.index += 1;
        }
        self.index < self.ranges.len() && self.ranges[self.index].0 <= row
    }

    /// Whether the cursor selects every row in `0..row_count` (the common
    /// full-block shape that unlocks bulk decode paths).
    fn covers_all(&self, row_count: usize) -> bool {
        self.ranges.first().is_some_and(|range| range.0 == 0)
            && self.ranges.last().is_some_and(|range| range.1 >= row_count)
            && self.ranges.windows(2).all(|pair| pair[0].1 >= pair[1].0)
    }
}

#[allow(clippy::too_many_lines)]
fn read_block_if<F>(
    path: &Path,
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    should_decode: F,
) -> Result<BlockRead, StoreError>
where
    F: FnOnce(&[u8], &[u8]) -> Result<bool, StoreError>,
{
    read_block_if_with_budget(path, decoder, logical_type, None, should_decode, None, None)
}

fn read_block_if_bounded<F>(
    path: &Path,
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    memory: &ScanMemoryBudget<'_>,
    should_decode: F,
) -> Result<BlockRead, StoreError>
where
    F: FnOnce(&[u8], &[u8]) -> Result<bool, StoreError>,
{
    read_block_if_with_budget(
        path,
        decoder,
        logical_type,
        Some(memory),
        should_decode,
        None,
        None,
    )
}

fn read_file_block_if_bounded<F>(
    path: &Path,
    decoder: &mut FileDecoder,
    logical_type: LogicalType,
    memory: &ScanMemoryBudget<'_>,
    should_decode: F,
) -> Result<BlockRead, StoreError>
where
    F: FnOnce(&[u8], &[u8]) -> Result<bool, StoreError>,
{
    let block_offset = decoder.decode_position();
    let payload_length = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let encoded_length = payload_length.saturating_add(12);
    let _encoded_memory = memory.reserve_temporary(encoded_length)?;
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.extend_from_slice(
        &u32::try_from(payload_length)
            .map_err(|_| StoreError::FormatLimit("block payload exceeds u32::MAX".into()))?
            .to_le_bytes(),
    );
    encoded.resize(payload_length.saturating_add(4), 0);
    decoder
        .read_exact(&mut encoded[4..])
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    encoded.extend_from_slice(
        &decoder
            .u64()
            .map_err(|reason| corrupt_here(path, decoder, reason))?
            .to_le_bytes(),
    );
    let mut block_decoder = Decoder::with_base_offset(&encoded, block_offset);
    read_block_if_bounded(
        path,
        &mut block_decoder,
        logical_type,
        memory,
        should_decode,
    )
}

/// Reads one string block from the segment file straight into a column
/// builder's arena (see [`Utf8Sink`]), bypassing per-cell decode.
fn read_file_block_utf8_into(
    path: &Path,
    decoder: &mut FileDecoder,
    memory: &ScanMemoryBudget<'_>,
    sink: Utf8Sink<'_>,
) -> Result<BlockRead, StoreError> {
    let block_offset = decoder.decode_position();
    let payload_length = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let encoded_length = payload_length.saturating_add(12);
    let _encoded_memory = memory.reserve_temporary(encoded_length)?;
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.extend_from_slice(
        &u32::try_from(payload_length)
            .map_err(|_| StoreError::FormatLimit("block payload exceeds u32::MAX".into()))?
            .to_le_bytes(),
    );
    encoded.resize(payload_length.saturating_add(4), 0);
    decoder
        .read_exact(&mut encoded[4..])
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    encoded.extend_from_slice(
        &decoder
            .u64()
            .map_err(|reason| corrupt_here(path, decoder, reason))?
            .to_le_bytes(),
    );
    let mut block_decoder = Decoder::with_base_offset(&encoded, block_offset);
    read_block_if_with_budget(
        path,
        &mut block_decoder,
        LogicalType::Utf8,
        Some(memory),
        |_, _| Ok(true),
        Some(sink),
        None,
    )
}

/// Reads one integer-wire block straight into a typed column builder when
/// the encoding allows (bit-packed); other encodings return cells for the
/// caller's generic loop.
fn read_file_block_int_into(
    path: &Path,
    decoder: &mut FileDecoder,
    logical_type: LogicalType,
    memory: &ScanMemoryBudget<'_>,
    sink: IntSink<'_>,
) -> Result<BlockRead, StoreError> {
    let block_offset = decoder.decode_position();
    let payload_length = decoder
        .u32()
        .map_err(|reason| corrupt_here(path, decoder, reason))? as usize;
    let encoded_length = payload_length.saturating_add(12);
    let _encoded_memory = memory.reserve_temporary(encoded_length)?;
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.extend_from_slice(
        &u32::try_from(payload_length)
            .map_err(|_| StoreError::FormatLimit("block payload exceeds u32::MAX".into()))?
            .to_le_bytes(),
    );
    encoded.resize(payload_length.saturating_add(4), 0);
    decoder
        .read_exact(&mut encoded[4..])
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    encoded.extend_from_slice(
        &decoder
            .u64()
            .map_err(|reason| corrupt_here(path, decoder, reason))?
            .to_le_bytes(),
    );
    let mut block_decoder = Decoder::with_base_offset(&encoded, block_offset);
    read_block_if_with_budget(
        path,
        &mut block_decoder,
        logical_type,
        Some(memory),
        |_, _| Ok(true),
        None,
        Some(sink),
    )
}

#[allow(clippy::too_many_lines)]
fn read_block_if_with_budget<F>(
    path: &Path,
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    memory: Option<&ScanMemoryBudget<'_>>,
    should_decode: F,
    utf8_sink: Option<Utf8Sink<'_>>,
    int_sink: Option<IntSink<'_>>,
) -> Result<BlockRead, StoreError>
where
    F: FnOnce(&[u8], &[u8]) -> Result<bool, StoreError>,
{
    let block_offset = decoder.position();
    let payload = decoder
        .bytes()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    let expected_checksum = decoder
        .u64()
        .map_err(|reason| corrupt_here(path, decoder, reason))?;
    if xxh3_64(payload) != expected_checksum {
        return Err(corrupt(path, block_offset, "block checksum mismatch"));
    }
    let mut block = Decoder::new(payload);
    let row_count = block
        .u32()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?
        as usize;
    let null_bitmap = block
        .bytes()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    if null_bitmap.len() != row_count.div_ceil(8) {
        return Err(corrupt(
            path,
            block_offset + block.position(),
            "invalid null bitmap length",
        ));
    }
    let encoding = Encoding::decode(
        block
            .u8()
            .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?,
    )
    .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    let compression = Compression::decode(
        block
            .u8()
            .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?,
    )
    .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    let uncompressed_length = block
        .u32()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?
        as usize;
    let compressed_offset = block_offset + block.position();
    let compressed = block
        .bytes()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;

    let declared_nulls = block
        .u32()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?
        as usize;
    let minimum = block
        .bytes()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    let maximum = block
        .bytes()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    let hll = block
        .bytes()
        .map_err(|reason| corrupt(path, block_offset + block.position(), reason))?;
    if hll.len() != 64 {
        return Err(corrupt(
            path,
            block_offset + block.position(),
            "invalid HLL register count",
        ));
    }
    block
        .finish()
        .map_err(|reason| corrupt(path, block_offset, reason))?;

    // Popcount per byte rather than a test per row. This runs for every
    // block of every column of every scan, including blocks the predicate
    // is about to skip, so a bit-at-a-time walk here costs more than the
    // decoding it guards. The trailing byte is masked to the bits the row
    // count actually covers: without that, a corrupt file could hide a set
    // bit past the end that the per-row walk would have refused to count.
    let actual_nulls: usize = null_bitmap
        .iter()
        .enumerate()
        .map(|(byte, bits)| {
            let covered = row_count.saturating_sub(byte * 8).min(8);
            let mask = if covered >= 8 {
                u8::MAX
            } else {
                (1_u8 << covered) - 1
            };
            (bits & mask).count_ones() as usize
        })
        .sum();
    if actual_nulls != declared_nulls {
        return Err(corrupt(path, block_offset, "null count mismatch"));
    }
    if !should_decode(minimum, maximum)? {
        return Ok(BlockRead {
            row_count,
            cells: None,
            reserved_bytes: 0,
        });
    }
    let non_null_count = row_count - actual_nulls;
    let _uncompressed_memory = memory
        .map(|memory| memory.reserve_temporary(uncompressed_length))
        .transpose()?;
    let uncompressed = decompress_block(compression, compressed, uncompressed_length)
        .map_err(|reason| corrupt(path, compressed_offset, reason))?;
    if let Some(sink) = utf8_sink {
        decode_utf8_payload_into(
            &uncompressed,
            encoding,
            row_count,
            non_null_count,
            null_bitmap,
            sink,
        )
        .map_err(|reason| corrupt(path, compressed_offset, reason))?;
        return Ok(BlockRead {
            row_count,
            cells: None,
            reserved_bytes: 0,
        });
    }
    if let Some(sink) = int_sink
        && decode_int_payload_into(
            &uncompressed,
            logical_type,
            encoding,
            row_count,
            non_null_count,
            null_bitmap,
            sink,
        )
        .map_err(|reason| corrupt(path, compressed_offset, reason))?
    {
        return Ok(BlockRead {
            row_count,
            cells: None,
            reserved_bytes: 0,
        });
    }
    let decoded_heap =
        decoded_heap_upper_bound(&uncompressed, logical_type, encoding, non_null_count)
            .map_err(|reason| corrupt(path, compressed_offset, reason))?;
    let _decode_memory = memory
        .map(|memory| {
            memory.reserve_temporary(
                uncompressed_length
                    .saturating_add(non_null_count.saturating_mul(
                        std::mem::size_of::<Cell>().saturating_add(std::mem::size_of::<u64>()),
                    ))
                    .saturating_add(row_count.saturating_mul(std::mem::size_of::<Cell>()))
                    .saturating_add(decoded_heap),
            )
        })
        .transpose()?;
    let reserved_bytes =
        decoded_heap.saturating_add(row_count.saturating_mul(std::mem::size_of::<Cell>()));
    if let Some(memory) = memory {
        memory.reserve(reserved_bytes)?;
    }
    let decoded_values = decode_payload(&uncompressed, logical_type, encoding, non_null_count)
        .map_err(|reason| corrupt(path, compressed_offset, reason))?;
    // A block with no nulls decodes straight into its final shape. The
    // splice below exists to interleave `Cell::Null`, and doing it anyway
    // costs a second `Vec<Cell>` allocation plus a full move of every value
    // — per block, per column, on every scan. Non-nullable columns are the
    // common case, so this is the difference between one allocation and two
    // for most of the data an engine reads.
    if actual_nulls == 0 {
        if decoded_values.len() != row_count {
            return Err(corrupt(
                path,
                compressed_offset,
                "encoding produced the wrong value count",
            ));
        }
        return Ok(BlockRead {
            row_count,
            cells: Some(decoded_values),
            reserved_bytes: memory.map_or(0, |_| reserved_bytes),
        });
    }
    let mut decoded_values = decoded_values.into_iter();
    let mut cells = Vec::with_capacity(row_count);
    for index in 0..row_count {
        if null_bitmap[index / 8] & (1 << (index % 8)) != 0 {
            cells.push(Cell::Null);
        } else {
            cells.push(decoded_values.next().ok_or_else(|| {
                corrupt(path, compressed_offset, "encoding produced too few values")
            })?);
        }
    }
    if decoded_values.next().is_some() {
        return Err(corrupt(
            path,
            compressed_offset,
            "encoding produced too many values",
        ));
    }
    Ok(BlockRead {
        row_count,
        cells: Some(cells),
        reserved_bytes: memory.map_or(0, |_| reserved_bytes),
    })
}

/// Decodes one string block payload straight into a column builder's arena.
/// Values are UTF-8-validated once per distinct payload (per cell for Plain,
/// per entry for `Dictionary`, per run for `RunLength`) — never cloned per row.
#[allow(clippy::too_many_lines)]
fn decode_utf8_payload_into(
    bytes: &[u8],
    encoding: Encoding,
    row_count: usize,
    non_null_count: usize,
    null_bitmap: &[u8],
    sink: Utf8Sink<'_>,
) -> Result<(), String> {
    let Utf8Sink {
        builder,
        mut ranges,
    } = sink;
    let mut decoder = Decoder::new(bytes);
    let is_null = |row: usize| null_bitmap[row / 8] & (1 << (row % 8)) != 0;
    let validate = |value: &[u8]| {
        std::str::from_utf8(value)
            .map(|_| ())
            .map_err(|_| "invalid UTF-8 block value".to_owned())
    };
    let mut produced = 0_usize;
    match encoding {
        Encoding::Plain => {
            builder.degrade_dictionary();
            for row in 0..row_count {
                let in_range = ranges.contains(row);
                if is_null(row) {
                    if in_range {
                        builder.push(Cell::Null)?;
                    }
                    continue;
                }
                let value = decoder.bytes()?;
                produced += 1;
                if in_range {
                    validate(value)?;
                    builder.push_utf8(value)?;
                }
            }
        }
        Encoding::Dictionary => {
            let dictionary_count = decoder.u32()? as usize;
            let mut entries = Vec::with_capacity(dictionary_count);
            for _ in 0..dictionary_count {
                let entry = decoder.bytes()?;
                validate(entry)?;
                entries.push(entry);
            }
            // Code fast path: rows land as u32 codes into the chunk
            // dictionary; a 5-value column never materializes its strings.
            let translation = builder.begin_dictionary_block(&entries);
            // Bulk fast path (the Q2 profile's per-row Decoder::u32 +
            // push_code overhead): no nulls and a full-range cursor —
            // block codes translate to chunk codes during the copy
            // (bounds-checked by the translation lookup itself).
            if non_null_count == row_count
                && ranges.covers_all(row_count)
                && let Some(translation) = translation.as_deref()
            {
                let raw = decoder.take(row_count * 4)?;
                builder.push_codes_bulk(raw, translation)?;
                produced = row_count;
            } else {
                for row in 0..row_count {
                    let in_range = ranges.contains(row);
                    if is_null(row) {
                        if in_range {
                            match &translation {
                                Some(_) => builder.push_null_code()?,
                                None => builder.push(Cell::Null)?,
                            }
                        }
                        continue;
                    }
                    let index = decoder.u32()? as usize;
                    if index >= entries.len() {
                        return Err(format!("dictionary index {index} is out of bounds"));
                    }
                    produced += 1;
                    if in_range {
                        match &translation {
                            Some(translation) => builder.push_code(translation[index])?,
                            None => builder.push_utf8(entries[index])?,
                        }
                    }
                }
            }
        }
        Encoding::RunLength => {
            builder.degrade_dictionary();
            let run_count = decoder.u32()? as usize;
            let mut runs_read = 0_usize;
            let mut run_remaining = 0_usize;
            let mut run_value: &[u8] = &[];
            for row in 0..row_count {
                let in_range = ranges.contains(row);
                if is_null(row) {
                    if in_range {
                        builder.push(Cell::Null)?;
                    }
                    continue;
                }
                if run_remaining == 0 {
                    if runs_read == run_count {
                        return Err("run lengths produce too few values".to_owned());
                    }
                    run_remaining = decoder.u32()? as usize;
                    if run_remaining == 0 {
                        return Err("run length must be non-zero".to_owned());
                    }
                    run_value = decoder.bytes()?;
                    validate(run_value)?;
                    runs_read += 1;
                }
                run_remaining -= 1;
                produced += 1;
                if in_range {
                    builder.push_utf8(run_value)?;
                }
            }
            if run_remaining != 0 || runs_read != run_count {
                return Err("run lengths exceed block value count".to_owned());
            }
        }
        Encoding::BitPacked | Encoding::DeltaBitPacked => {
            return Err("string block uses an integer encoding".to_owned());
        }
    }
    if produced != non_null_count {
        return Err(format!(
            "encoding produced {produced} values, expected {non_null_count}"
        ));
    }
    decoder.finish()?;
    Ok(())
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

fn decoded_heap_upper_bound(
    bytes: &[u8],
    logical_type: LogicalType,
    encoding: Encoding,
    value_count: usize,
) -> Result<usize, String> {
    if !matches!(logical_type, LogicalType::Utf8 | LogicalType::Binary) {
        return Ok(if logical_type == LogicalType::PrimaryKey {
            let payload_bytes = if matches!(encoding, Encoding::Plain) {
                bytes.len().saturating_mul(4)
            } else {
                bytes.len().saturating_mul(value_count)
            };
            payload_bytes.saturating_add(value_count.saturating_mul(64))
        } else {
            0
        });
    }
    let mut decoder = Decoder::new(bytes);
    let heap_bytes = match encoding {
        Encoding::Plain => {
            let mut heap_bytes = 0_usize;
            for _ in 0..value_count {
                heap_bytes = heap_bytes.saturating_add(decoder.bytes()?.len());
            }
            heap_bytes
        }
        Encoding::Dictionary => {
            let dictionary_count = decoder.u32()? as usize;
            let mut maximum = 0_usize;
            for _ in 0..dictionary_count {
                maximum = maximum.max(decoder.bytes()?.len());
            }
            for _ in 0..value_count {
                let index = decoder.u32()? as usize;
                if index >= dictionary_count {
                    return Err(format!("dictionary index {index} is out of bounds"));
                }
            }
            maximum.saturating_mul(value_count)
        }
        Encoding::RunLength => {
            let run_count = decoder.u32()? as usize;
            let mut produced = 0_usize;
            let mut heap_bytes = 0_usize;
            for _ in 0..run_count {
                let length = decoder.u32()? as usize;
                if length == 0 {
                    return Err("run length must be non-zero".to_owned());
                }
                produced = produced.saturating_add(length);
                if produced > value_count {
                    return Err("run lengths exceed block value count".to_owned());
                }
                heap_bytes =
                    heap_bytes.saturating_add(decoder.bytes()?.len().saturating_mul(length));
            }
            if produced != value_count {
                return Err(format!(
                    "run lengths produce {produced} values, expected {value_count}"
                ));
            }
            heap_bytes
        }
        Encoding::BitPacked | Encoding::DeltaBitPacked => {
            return Err("string block uses an integer encoding".to_owned());
        }
    };
    decoder.finish()?;
    Ok(heap_bytes)
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
    if width == 0 {
        return Ok(values);
    }
    // LSB-first bitstream: value v's bits live at positions v*width.. in
    // little-endian byte order. A 16-byte window covers the worst case of
    // 64 bits starting at bit offset 7 within a byte.
    let width = usize::from(width);
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    };
    for (value_index, value) in values.iter_mut().enumerate() {
        let bit = value_index * width;
        let byte = bit / 8;
        let mut window = [0_u8; 16];
        let available = (bytes.len() - byte).min(16);
        window[..available].copy_from_slice(&bytes[byte..byte + available]);
        #[allow(clippy::cast_possible_truncation)]
        {
            *value = (u128::from_le_bytes(window) >> (bit % 8)) as u64 & mask;
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
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(value) => value.len(),
            Self::Binary(value) => value.len(),
            Self::Key(value) => value
                .parts()
                .len()
                .saturating_mul(std::mem::size_of::<pintail_types::KeyPart>())
                .saturating_add(value.heap_bytes()),
            Self::Null | Self::Boolean(_) | Self::Int64(_) | Self::UInt64(_) | Self::Float64(_) => {
                0
            }
        }
    }

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

    fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Boolean(value),
            Self::Int64(value) => Value::Int64(value),
            Self::UInt64(value) => Value::UInt64(value),
            Self::Float64(bits) => {
                Value::Float64(pintail_types::Float64::new(f64::from_bits(bits)))
            }
            Self::Utf8(value) => Value::Utf8(value),
            Self::Binary(value) => Value::Binary(value),
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
            Value::Utf8(value) => {
                if let Some(units) = spec.native {
                    // The probe already verified every value round-trips.
                    let parsed = units
                        .parse_exact(value)
                        .expect("probed native column value round-trips");
                    Cell::Int64(parsed)
                } else {
                    Cell::Utf8(value.clone())
                }
            }
            Value::Boolean(value) => Cell::Boolean(*value),
            Value::Int64(value) => Cell::Int64(*value),
            Value::UInt64(value) => Cell::UInt64(*value),
            Value::Float64(value) => Cell::Float64(value.to_bits()),
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

fn parse_footer_body(
    path: &Path,
    bytes: &[u8],
    footer_offset: usize,
    meta: &SegmentMeta,
) -> Result<Vec<(u64, PrimaryKey)>, StoreError> {
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
    let mut sparse = Vec::with_capacity(sparse_count as usize);
    for _ in 0..sparse_count {
        let ordinal = decoder
            .u64()
            .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
        let key = decode_key(&mut decoder)
            .map_err(|reason| corrupt(path, footer_offset + decoder.position(), reason))?;
        sparse.push((ordinal, key));
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
    Ok(sparse)
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

fn corrupt_here(
    path: &Path,
    decoder: &impl DecodePosition,
    reason: impl Into<String>,
) -> StoreError {
    corrupt(path, decoder.decode_position(), reason)
}

#[cfg(test)]
mod range_read_tests {
    use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};

    use super::{Compression, ScanMemoryBudget, read_projected_column_ranges, write};

    #[test]
    fn multi_range_reads_match_concatenated_single_ranges() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let schema = TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "label", DataType::Utf8, true),
                Column::new(
                    3,
                    "amount",
                    DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    true,
                ),
            ],
        )
        .expect("schema");
        let rows = (0..100_u64)
            .map(|id| {
                StoredRow::new(
                    PrimaryKey::new(vec![KeyPart::UInt64(id)]).expect("key"),
                    vec![
                        Value::UInt64(id),
                        if id % 7 == 0 {
                            Value::Null
                        } else {
                            Value::Utf8(format!("label-{id}"))
                        },
                        Value::Utf8(format!("{id}.25")),
                    ],
                    1,
                    false,
                )
            })
            .collect::<Vec<_>>();
        // Small blocks so ranges straddle block boundaries.
        let meta = write(
            directory.path(),
            1,
            &schema,
            &rows,
            16,
            Compression::Lz4,
            true,
        )
        .expect("write segment");

        let budget_cell = std::sync::atomic::AtomicUsize::new(0);
        let budget = ScanMemoryBudget::new(&budget_cell, usize::MAX);
        let projection = [0_usize, 1, 2];
        let ranges = [3..9_usize, 15..17, 16 + 16..80, 99..100];
        let multi = read_projected_column_ranges(
            directory.path(),
            &meta,
            &schema,
            &projection,
            &ranges,
            &budget,
        )
        .expect("multi-range read");
        let mut expected: Vec<Vec<Value>> = vec![Vec::new(); projection.len()];
        for range in &ranges {
            let single = read_projected_column_ranges(
                directory.path(),
                &meta,
                &schema,
                &projection,
                std::slice::from_ref(range),
                &budget,
            )
            .expect("single-range read");
            for (column, values) in expected.iter_mut().zip(single.columns) {
                column.extend(values.into_values());
            }
        }
        let actual = multi
            .columns
            .into_iter()
            .map(super::super::store::DecodedColumn::into_values)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "multi-range equals concatenated singles");
        assert_eq!(
            actual[0].len(),
            ranges
                .iter()
                .map(std::iter::ExactSizeIterator::len)
                .sum::<usize>()
        );

        let backwards = [10..20_usize, 5..8];
        assert!(
            read_projected_column_ranges(
                directory.path(),
                &meta,
                &schema,
                &projection,
                &backwards,
                &budget,
            )
            .is_err(),
            "unsorted ranges are rejected"
        );
    }
}

#[cfg(test)]
mod native_units_tests {
    use pintail_types::{DataType, Value};

    use super::{NativeUnits, probe_native_column};

    fn rows_of(values: Vec<Value>) -> Vec<pintail_types::StoredRow> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let key = pintail_types::PrimaryKey::new(vec![pintail_types::KeyPart::Int64(
                    i64::try_from(index).expect("small index"),
                )])
                .expect("test key");
                pintail_types::StoredRow::new(key, vec![value], 1, false)
            })
            .collect()
    }

    #[test]
    fn eligible_types_map_to_native_units() {
        assert_eq!(
            NativeUnits::for_data_type(DataType::Date32),
            Some(NativeUnits::Date)
        );
        assert_eq!(
            NativeUnits::for_data_type(DataType::DateTime64 { fsp: 3 }),
            Some(NativeUnits::DateTime { fsp: 3 })
        );
        assert_eq!(
            NativeUnits::for_data_type(DataType::Decimal {
                precision: 18,
                scale: 2
            }),
            Some(NativeUnits::Decimal { scale: 2 })
        );
        // i64 cannot carry every precision-19 value: stays on text.
        assert_eq!(
            NativeUnits::for_data_type(DataType::Decimal {
                precision: 19,
                scale: 2
            }),
            None
        );
        assert_eq!(NativeUnits::for_data_type(DataType::Utf8), None);
    }

    #[test]
    fn parse_exact_requires_identical_round_trips() {
        let date = NativeUnits::Date;
        assert_eq!(date.parse_exact("2024-02-29"), Some(19_782));
        assert_eq!(date.format(19_782).as_deref(), Some("2024-02-29"));
        assert_eq!(date.parse_exact("2023-02-29"), None);

        let datetime = NativeUnits::DateTime { fsp: 3 };
        let micros = datetime
            .parse_exact("2023-06-15 12:34:56.123")
            .expect("canonical datetime");
        assert_eq!(
            datetime.format(micros).as_deref(),
            Some("2023-06-15 12:34:56.123")
        );
        // fsp-0 column cannot regenerate a fractional payload: rejected.
        assert_eq!(
            NativeUnits::DateTime { fsp: 0 }.parse_exact("2023-06-15 12:34:56.123"),
            None
        );

        let decimal = NativeUnits::Decimal { scale: 2 };
        assert_eq!(decimal.parse_exact("123.45"), Some(12_345));
        assert_eq!(decimal.parse_exact("-0.05"), Some(-5));
        // "123.4" parses but formats back as "123.40": not canonical input.
        assert_eq!(decimal.parse_exact("123.4"), None);
    }

    #[test]
    fn probe_accepts_nulls_and_rejects_mixed_columns() {
        let decimal = NativeUnits::Decimal { scale: 2 };
        let rows = rows_of(vec![
            Value::Utf8("1.50".to_owned()),
            Value::Null,
            Value::Utf8("-2.25".to_owned()),
        ]);
        assert_eq!(
            probe_native_column(decimal, &rows, 0),
            Some(vec![Some(150), None, Some(-225)])
        );
        let rows = rows_of(vec![
            Value::Utf8("1.50".to_owned()),
            Value::Utf8("not-a-decimal".to_owned()),
        ]);
        assert_eq!(probe_native_column(decimal, &rows, 0), None);
    }
}
