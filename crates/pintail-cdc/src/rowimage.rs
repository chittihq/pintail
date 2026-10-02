//! Row images read straight from a rows event's bytes.
//!
//! A rows event is a run of row images, each a NULL bitmap followed by the
//! values of the columns that are present and not NULL. What a value's bytes
//! mean is fixed by the table map and the probed schema, and neither changes
//! between the rows of an event or between the events of one table map. A
//! [`RowPlan`] resolves all of it once - the width or length prefix of every
//! column, where it lands in the schema, how its bytes become a stored value
//! - and the rows are then read with nothing decided per row.
//!
//! The plan reads the common column types itself. Every other type, and
//! every value a reader declines, goes through the general value decoder on
//! the same bytes, so the two can only differ in cost.

use chrono::{Datelike as _, Timelike as _, Utc};
use mysql_async::{
    binlog::{
        events::{OptionalMetaExtractor, TableMapEvent},
        value::BinlogValue,
    },
    consts::ColumnType,
};
use mysql_common::{io::ParseBuf, proto::MyDeserialize as _};
use pintail_probe::{SourceColumn, SourceTable};
use pintail_types::{DataType, KeyMode, PrimaryKey, Value};

use crate::{
    CdcError,
    decoder::{RowAlignment, decode_value, is_textual, key_part},
};

/// Decimal digits one four-byte group of a packed decimal holds.
const GROUP_DIGITS: usize = 9;
/// Bytes a partial group of that many digits is packed into.
const PARTIAL_GROUP_BYTES: [usize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
const POWERS_OF_TEN: [u32; 10] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

/// Where the value at one table-map ordinal goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// The schema column at this index.
    Column(usize),
    /// Nowhere: the schema no longer has the column.
    Dropped,
    /// Nowhere, and the row is refused: the image is wider than the schema
    /// it is placed against position by position.
    Beyond,
}

/// The integer type a stored integer is checked against.
#[derive(Clone, Copy, Debug)]
enum IntegerTarget {
    Boolean,
    Signed(DataType),
    Unsigned(DataType),
}

/// How one column's bytes become a stored value.
#[derive(Debug)]
enum Reader {
    /// A little-endian integer of `width` bytes. `declared_width` is set for
    /// a column declared unsigned: a table map without signedness has the
    /// value read as signed, and its bits are then taken at that width.
    Integer {
        width: usize,
        declared_width: Option<usize>,
        target: IntegerTarget,
    },
    Float,
    Double,
    /// Length-prefixed UTF-8 text.
    Text {
        prefix: usize,
    },
    /// Length-prefixed bytes kept as they are.
    Binary {
        prefix: usize,
    },
    /// A packed decimal.
    Decimal {
        precision: usize,
        scale: usize,
        size: usize,
    },
    /// A packed date and time with `fraction` trailing bytes.
    DateTime {
        fraction: usize,
        fsp: u8,
    },
    /// Seconds since the epoch with `fraction` trailing bytes.
    Timestamp {
        fraction: usize,
        fsp: u8,
    },
    Date,
    /// A one- or two-byte index into the declared labels.
    Enum {
        width: usize,
        labels: Vec<String>,
    },
    /// Any other type: the general decoder finds the value's end.
    General,
}

struct PlanColumn {
    kind: ColumnType,
    metadata: Vec<u8>,
    unsigned: bool,
    placement: Placement,
    reader: Reader,
}

/// Everything about a table map's row images that does not vary by row.
pub(crate) struct RowPlan {
    /// The table map the plan was compiled from.
    table_map: Option<TableMapEvent<'static>>,
    source: SourceTable,
    columns: Vec<PlanColumn>,
    /// Schema indexes of the key columns, in key order. Empty for a table
    /// keyed by row version.
    key: Vec<usize>,
}

/// One value of a row image, or the reason the row holding it is refused.
type Decoded = Result<Value, CdcError>;

/// A value's bytes in a row image: all of them, and the part after any
/// length prefix.
type Framed<'a> = (&'a [u8], &'a [u8]);

fn truncated(table: &str) -> CdcError {
    CdcError::Decode(format!("{table} row image ends inside a value"))
}

fn take<'a>(data: &mut &'a [u8], length: usize, table: &str) -> Result<&'a [u8], CdcError> {
    if data.len() < length {
        return Err(truncated(table));
    }
    let (taken, rest) = data.split_at(length);
    *data = rest;
    Ok(taken)
}

/// A big- or little-endian unsigned integer of up to eight bytes.
fn unsigned(bytes: &[u8], big_endian: bool) -> u64 {
    let fold = |value: u64, byte: &u8| (value << 8) | u64::from(*byte);
    if big_endian {
        bytes.iter().fold(0, fold)
    } else {
        bytes.iter().rev().fold(0, fold)
    }
}

/// A big-endian two's-complement integer of up to eight bytes.
fn signed_big_endian(bytes: &[u8]) -> i64 {
    let negative = bytes.first().is_some_and(|byte| byte & 0x80 != 0);
    let mut wide = [if negative { 0xff } else { 0 }; 8];
    wide[8 - bytes.len()..].copy_from_slice(bytes);
    i64::from_be_bytes(wide)
}

fn push_digits(text: &mut String, value: u32, width: usize) {
    for position in (0..width).rev() {
        let digit = (value / POWERS_OF_TEN[position]) % 10;
        text.push(char::from(b'0' + u8::try_from(digit).unwrap_or(0)));
    }
}

fn push_number(text: &mut String, value: u32) {
    let mut width = 1;
    while width < 10 && value >= POWERS_OF_TEN[width] {
        width += 1;
    }
    if width == 10 {
        // Above every power in the table: ten digits.
        text.push(char::from(
            b'0' + u8::try_from(value / 1_000_000_000).unwrap_or(0),
        ));
        width = 9;
    }
    push_digits(text, value, width);
}

/// The text of a packed decimal, or `None` for a group outside the range
/// its digits allow, which the general decoder then reports.
fn decimal_text(raw: &[u8], precision: usize, scale: usize) -> Option<String> {
    let negative = raw.first()? & 0x80 == 0;
    let flip = if negative { 0xff } else { 0 };
    let mut position = 0_usize;
    let mut group = |length: usize, digits: usize| -> Option<u32> {
        let mut value = 0_u32;
        for index in position..position + length {
            let mut byte = *raw.get(index)? ^ flip;
            if index == 0 {
                byte ^= 0x80;
            }
            if index == position && byte & 0x80 != 0 {
                return None;
            }
            value = (value << 8) | u32::from(byte);
        }
        position += length;
        (value < POWERS_OF_TEN[digits]).then_some(value)
    };
    let integer_digits = precision - scale;
    let leading = integer_digits % GROUP_DIGITS;
    let trailing = scale % GROUP_DIGITS;
    let mut text = String::with_capacity(precision + 2);
    if negative {
        text.push('-');
    }
    let mut started = false;
    let mut write_integer = |text: &mut String, value: u32| {
        if started {
            push_digits(text, value, GROUP_DIGITS);
        } else if value != 0 {
            started = true;
            push_number(text, value);
        }
    };
    if leading > 0 {
        let value = group(PARTIAL_GROUP_BYTES[leading], leading)?;
        write_integer(&mut text, value);
    }
    for _ in 0..integer_digits / GROUP_DIGITS {
        let value = group(4, GROUP_DIGITS)?;
        write_integer(&mut text, value);
    }
    if !started {
        if scale == 0 {
            // Zero with no fraction carries no sign.
            return Some("0".to_owned());
        }
        text.push('0');
    }
    if scale > 0 {
        text.push('.');
    }
    for _ in 0..scale / GROUP_DIGITS {
        let value = group(4, GROUP_DIGITS)?;
        push_digits(&mut text, value, GROUP_DIGITS);
    }
    if trailing > 0 {
        let value = group(PARTIAL_GROUP_BYTES[trailing], trailing)?;
        push_digits(&mut text, value, trailing);
    }
    Some(text)
}

fn decimal_size(precision: usize, scale: usize) -> usize {
    let integer_digits = precision - scale;
    (integer_digits / GROUP_DIGITS) * 4
        + PARTIAL_GROUP_BYTES[integer_digits % GROUP_DIGITS]
        + (scale / GROUP_DIGITS) * 4
        + PARTIAL_GROUP_BYTES[scale % GROUP_DIGITS]
}

fn push_fraction(text: &mut String, micros: u32, fsp: u8) {
    if fsp == 0 {
        return;
    }
    text.push('.');
    let digits = usize::from(fsp.min(6));
    push_digits(text, micros / POWERS_OF_TEN[6 - digits], digits);
}

/// A date and time as the store holds it, `None` when the parts are not
/// ones a source can hold. The all-zero date is a value and keeps the
/// column's width.
fn datetime_value(parts: [u32; 6], micros: u32, fsp: u8) -> Value {
    let [year, month, day, hour, minute, second] = parts;
    let mut text = String::with_capacity(26);
    if year == 0 && month == 0 && day == 0 {
        text.push_str("0000-00-00 00:00:00");
        if fsp > 0 {
            text.push('.');
            for _ in 0..fsp {
                text.push('0');
            }
        }
        return Value::Utf8(text);
    }
    if year > 9999
        || month > 12
        || day > 31
        || hour >= 24
        || minute >= 60
        || second >= 60
        || micros >= 1_000_000
    {
        return Value::Null;
    }
    push_digits(&mut text, year, 4);
    text.push('-');
    push_digits(&mut text, month, 2);
    text.push('-');
    push_digits(&mut text, day, 2);
    text.push(' ');
    push_digits(&mut text, hour, 2);
    text.push(':');
    push_digits(&mut text, minute, 2);
    text.push(':');
    push_digits(&mut text, second, 2);
    push_fraction(&mut text, micros, fsp);
    Value::Utf8(text)
}

/// Bytes of fractional seconds a temporal column of this precision carries.
const fn fraction_bytes(precision: u8) -> usize {
    match precision {
        1 | 2 => 1,
        3 | 4 => 2,
        5 | 6 => 3,
        _ => 0,
    }
}

/// The fraction's microseconds as written, which is signed.
fn fraction_micros(bytes: &[u8]) -> i64 {
    match bytes.len() {
        1 => signed_big_endian(bytes) * 10_000,
        2 => signed_big_endian(bytes) * 100,
        3 => signed_big_endian(bytes),
        _ => 0,
    }
}

fn packed_datetime(raw: &[u8], fsp: u8) -> Option<Value> {
    let (whole, fraction) = raw.split_at_checked(5)?;
    let whole = i64::try_from(unsigned(whole, true)).ok()? - 0x80_0000_0000;
    let packed = (whole * (1 << 24) + fraction_micros(fraction)).abs();
    let micros = packed % (1 << 24);
    let clock_and_date = packed >> 24;
    let date = clock_and_date >> 17;
    let year_month = date >> 5;
    let clock = clock_and_date % (1 << 17);
    let part = |value: i64| u32::try_from(value).ok();
    Some(datetime_value(
        [
            part(year_month / 13)?,
            part(year_month % 13)?,
            part(date % (1 << 5))?,
            part(clock >> 12)?,
            part((clock >> 6) % (1 << 6))?,
            part(clock % (1 << 6))?,
        ],
        part(micros)?,
        fsp,
    ))
}

fn epoch_timestamp(raw: &[u8], fsp: u8) -> Option<Value> {
    let (whole, fraction) = raw.split_at_checked(4)?;
    let seconds = i32::from_be_bytes(whole.try_into().ok()?);
    let micros = u32::try_from(fraction_micros(fraction)).ok()?;
    if micros >= 1_000_000 {
        return None;
    }
    if seconds == 0 {
        return Some(datetime_value([0; 6], 0, fsp));
    }
    let moment = chrono::DateTime::<Utc>::from_timestamp(i64::from(seconds), 0)?;
    Some(datetime_value(
        [
            u32::from(u16::try_from(moment.year()).ok()?),
            moment.month(),
            moment.day(),
            moment.hour(),
            moment.minute(),
            moment.second(),
        ],
        micros,
        fsp,
    ))
}

fn packed_date(raw: &[u8]) -> Option<Value> {
    let packed = u32::try_from(unsigned(raw, false)).ok()?;
    let (year, month, day) = (packed >> 9, (packed >> 5) & 15, packed & 31);
    if year == 0 && month == 0 && day == 0 {
        return Some(Value::Utf8("0000-00-00".to_owned()));
    }
    if year > 9999 || month > 12 || day > 31 {
        return Some(Value::Null);
    }
    let mut text = String::with_capacity(10);
    push_digits(&mut text, year, 4);
    text.push('-');
    push_digits(&mut text, month, 2);
    text.push('-');
    push_digits(&mut text, day, 2);
    Some(Value::Utf8(text))
}

/// The value an integer column stores, or `None` when it does not fit the
/// declared type, which the general decoder then reports.
fn integer_value(
    raw: &[u8],
    unsigned_in_map: bool,
    declared_width: Option<usize>,
    target: IntegerTarget,
) -> Option<Value> {
    // The value as read: signed unless the table map says otherwise, and an
    // unsigned one above the signed range kept unsigned.
    let mut wide = [0_u8; 8];
    wide[..raw.len()].copy_from_slice(raw);
    let negative = !unsigned_in_map && raw.last().is_some_and(|byte| byte & 0x80 != 0);
    if negative {
        wide[raw.len()..].fill(0xff);
    }
    let bits = u64::from_le_bytes(wide);
    let read: Result<i64, u64> = if negative {
        Ok(i64::from_le_bytes(wide))
    } else {
        i64::try_from(bits).map_err(|_| bits)
    };
    // A column declared unsigned takes the low bytes of a signed reading.
    let read = match (read, declared_width) {
        (Ok(_), Some(width)) => {
            let mut low = [0_u8; 8];
            low[..width].copy_from_slice(&wide[..width]);
            Err(u64::from_le_bytes(low))
        }
        (read, _) => read,
    };
    Some(match target {
        IntegerTarget::Boolean => Value::Boolean(read != Ok(0) && read != Err(0)),
        IntegerTarget::Signed(data_type) => {
            let value = match read {
                Ok(value) => value,
                Err(value) => i64::try_from(value).ok()?,
            };
            let fits = match data_type {
                DataType::Int8 => i8::try_from(value).is_ok(),
                DataType::Int16 => i16::try_from(value).is_ok(),
                DataType::Int32 => i32::try_from(value).is_ok(),
                _ => true,
            };
            fits.then_some(Value::Int64(value))?
        }
        IntegerTarget::Unsigned(data_type) => {
            let value = match read {
                Ok(value) => u64::try_from(value).ok()?,
                Err(value) => value,
            };
            let fits = match data_type {
                DataType::UInt8 => u8::try_from(value).is_ok(),
                DataType::UInt16 => u16::try_from(value).is_ok(),
                DataType::UInt32 => u32::try_from(value).is_ok(),
                DataType::Year => value == 0 || (1901..=2155).contains(&value),
                _ => true,
            };
            fits.then_some(Value::UInt64(value))?
        }
    })
}

/// The reader for a length-prefixed column, by what the schema stores.
fn bytes_reader(prefix: usize, column: &SourceColumn, special: bool) -> Reader {
    if special {
        return Reader::General;
    }
    let utf8 = column.character_set.as_deref().is_none_or(|character_set| {
        ["utf8mb4", "utf8", "utf8mb3", "ascii"]
            .iter()
            .any(|known| character_set.eq_ignore_ascii_case(known))
    });
    match (is_textual(column), column.pintail_type) {
        (true, DataType::Utf8) if utf8 => Reader::Text { prefix },
        (false, DataType::Binary) => Reader::Binary { prefix },
        _ => Reader::General,
    }
}

impl Reader {
    /// Picks the reader for a column. Anything not recognised with
    /// certainty is left to the general decoder.
    #[allow(clippy::too_many_lines)]
    fn resolve(kind: ColumnType, metadata: &[u8], unsigned: bool, column: &SourceColumn) -> Self {
        let mysql_type = column.mysql_data_type.to_ascii_lowercase();
        // Types whose stored value is derived from the declaration rather
        // than read off the bytes.
        let special = matches!(mysql_type.as_str(), "enum" | "set" | "timestamp");
        match kind {
            ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_LONGLONG => {
                let target = match column.pintail_type {
                    DataType::Boolean => IntegerTarget::Boolean,
                    DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                        IntegerTarget::Signed(column.pintail_type)
                    }
                    DataType::UInt8
                    | DataType::UInt16
                    | DataType::UInt32
                    | DataType::UInt64
                    | DataType::Year => IntegerTarget::Unsigned(column.pintail_type),
                    _ => return Self::General,
                };
                if special {
                    return Self::General;
                }
                let declared_unsigned = column
                    .mysql_column_type
                    .to_ascii_lowercase()
                    .contains("unsigned");
                let declared_width = match mysql_type.as_str() {
                    _ if !declared_unsigned => None,
                    "tinyint" => Some(1),
                    "smallint" => Some(2),
                    "mediumint" => Some(3),
                    "int" | "integer" => Some(4),
                    "bigint" => Some(8),
                    _ => None,
                };
                // Three bytes carry their sign only by the declaration.
                // One that is not a signed-or-unsigned MEDIUMINT agreeing
                // with the table map is not read here.
                if kind == ColumnType::MYSQL_TYPE_INT24
                    && (mysql_type != "mediumint" || (unsigned && declared_width.is_none()))
                {
                    return Self::General;
                }
                Self::Integer {
                    width: match kind {
                        ColumnType::MYSQL_TYPE_TINY => 1,
                        ColumnType::MYSQL_TYPE_SHORT => 2,
                        ColumnType::MYSQL_TYPE_INT24 => 3,
                        ColumnType::MYSQL_TYPE_LONG => 4,
                        _ => 8,
                    },
                    declared_width,
                    target,
                }
            }
            ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE
                if !special
                    && matches!(column.pintail_type, DataType::Float32 | DataType::Float64) =>
            {
                if kind == ColumnType::MYSQL_TYPE_FLOAT {
                    Self::Float
                } else {
                    Self::Double
                }
            }
            ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
                let &[low, high, ..] = metadata else {
                    return Self::General;
                };
                let declared = usize::from(low) | (usize::from(high) << 8);
                bytes_reader(if declared < 256 { 1 } else { 2 }, column, special)
            }
            ColumnType::MYSQL_TYPE_STRING => {
                let &[first, second, ..] = metadata else {
                    return Self::General;
                };
                let (first, second) = (usize::from(first), usize::from(second));
                let declared = if first == 0 {
                    second << 8
                } else if first & 0x30 == 0x30 {
                    second
                } else {
                    // The two bits above a byte of length are folded into
                    // the type byte, inverted.
                    second | (((first & 0x30) ^ 0x30) << 4)
                };
                bytes_reader(if declared < 256 { 1 } else { 2 }, column, special)
            }
            ColumnType::MYSQL_TYPE_TINY_BLOB
            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
            | ColumnType::MYSQL_TYPE_LONG_BLOB
            | ColumnType::MYSQL_TYPE_BLOB
            | ColumnType::MYSQL_TYPE_GEOMETRY => match metadata.first().copied() {
                Some(prefix @ 1..=4) => bytes_reader(usize::from(prefix), column, special),
                _ => Self::General,
            },
            ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                let &[precision, scale, ..] = metadata else {
                    return Self::General;
                };
                let (precision, scale) = (usize::from(precision), usize::from(scale));
                if special
                    || is_textual(column)
                    || !matches!(column.pintail_type, DataType::Decimal { .. })
                    || !(1..=65).contains(&precision)
                    || scale > precision
                {
                    return Self::General;
                }
                Self::Decimal {
                    precision,
                    scale,
                    size: decimal_size(precision, scale),
                }
            }
            ColumnType::MYSQL_TYPE_DATETIME2 => match (metadata.first(), column.pintail_type) {
                (Some(precision), DataType::DateTime64 { fsp }) if !special => Self::DateTime {
                    fraction: fraction_bytes(*precision),
                    fsp,
                },
                _ => Self::General,
            },
            ColumnType::MYSQL_TYPE_TIMESTAMP2 => match (metadata.first(), column.pintail_type) {
                (Some(precision), DataType::DateTime64 { fsp }) if mysql_type == "timestamp" => {
                    Self::Timestamp {
                        fraction: fraction_bytes(*precision),
                        fsp,
                    }
                }
                _ => Self::General,
            },
            ColumnType::MYSQL_TYPE_NEWDATE
                if !special && column.pintail_type == DataType::Date32 =>
            {
                Self::Date
            }
            ColumnType::MYSQL_TYPE_ENUM => {
                let width = match metadata {
                    [_, width @ (1 | 2), ..] => usize::from(*width),
                    _ => return Self::General,
                };
                if mysql_type != "enum" || column.pintail_type != DataType::Utf8 {
                    return Self::General;
                }
                match pintail_types::declaration_labels(&column.mysql_column_type, "enum") {
                    Some(labels) => Self::Enum { width, labels },
                    None => Self::General,
                }
            }
            _ => Self::General,
        }
    }

    /// The bytes of the next value: all of them, and the part after any
    /// length prefix. `None` for a reader that does not frame its own.
    fn frame<'a>(&self, data: &mut &'a [u8], table: &str) -> Result<Option<Framed<'a>>, CdcError> {
        let start = *data;
        let length = match self {
            Self::Integer { width, .. } | Self::Enum { width, .. } => *width,
            Self::Float => 4,
            Self::Double => 8,
            Self::Decimal { size, .. } => *size,
            Self::DateTime { fraction, .. } => 5 + fraction,
            Self::Timestamp { fraction, .. } => 4 + fraction,
            Self::Date => 3,
            Self::Text { prefix } | Self::Binary { prefix } => {
                let length = take(data, *prefix, table)?;
                let length = usize::try_from(unsigned(length, false)).unwrap_or(usize::MAX);
                let body = take(data, length, table)?;
                return Ok(Some((&start[..prefix + length], body)));
            }
            Self::General => return Ok(None),
        };
        let body = take(data, length, table)?;
        Ok(Some((body, body)))
    }

    /// The stored value of a framed body, or `None` to leave the value to
    /// the general decoder.
    fn value(&self, body: &[u8], unsigned_in_map: bool) -> Option<Value> {
        match self {
            Self::Integer {
                declared_width,
                target,
                ..
            } => integer_value(body, unsigned_in_map, *declared_width, *target),
            Self::Float => Some(Value::float64(f64::from(f32::from_le_bytes(
                body.try_into().ok()?,
            )))),
            Self::Double => Some(Value::float64(f64::from_le_bytes(body.try_into().ok()?))),
            Self::Text { .. } => Some(Value::Utf8(std::str::from_utf8(body).ok()?.to_owned())),
            Self::Binary { .. } => Some(Value::Binary(body.to_vec())),
            Self::Decimal {
                precision, scale, ..
            } => decimal_text(body, *precision, *scale).map(Value::Utf8),
            Self::DateTime { fsp, .. } => packed_datetime(body, *fsp),
            Self::Timestamp { fsp, .. } => epoch_timestamp(body, *fsp),
            Self::Date => packed_date(body),
            Self::Enum { labels, .. } => {
                let index = usize::try_from(unsigned(body, false)).ok()?;
                if index == 0 {
                    return Some(Value::Utf8(String::new()));
                }
                labels.get(index - 1).cloned().map(Value::Utf8)
            }
            Self::General => None,
        }
    }
}

impl PlanColumn {
    fn new(
        kind: ColumnType,
        metadata: &[u8],
        unsigned: bool,
        placement: Placement,
        column: Option<&SourceColumn>,
    ) -> Self {
        Self {
            kind,
            metadata: metadata.to_vec(),
            unsigned,
            placement,
            reader: column.map_or(Reader::General, |column| {
                Reader::resolve(kind, metadata, unsigned, column)
            }),
        }
    }

    /// The general decoder over `data`, which it advances past the value.
    fn general(
        &self,
        table: &str,
        column: Option<&SourceColumn>,
        data: &mut &[u8],
    ) -> Result<Decoded, CdcError> {
        let whole = *data;
        let mut buffer = ParseBuf(whole);
        let value = BinlogValue::deserialize(
            (self.kind, self.metadata.as_slice(), self.unsigned, false),
            &mut buffer,
        )
        .map_err(|error| CdcError::Decode(format!("{table} row image: {error}")))?;
        let consumed = whole.len() - buffer.0.len();
        let decoded = column.map_or(Ok(Value::Null), |column| decode_value(table, column, value));
        *data = &whole[consumed..];
        Ok(decoded)
    }

    /// Reads the next value off `data`. The outer error means the image
    /// cannot be followed any further; the inner one refuses this row only.
    fn read(
        &self,
        table: &str,
        column: Option<&SourceColumn>,
        data: &mut &[u8],
    ) -> Result<Decoded, CdcError> {
        let Some((mut whole, body)) = self.reader.frame(data, table)? else {
            return self.general(table, column, data);
        };
        if column.is_none() {
            return Ok(Ok(Value::Null));
        }
        match self.reader.value(body, self.unsigned) {
            Some(value) => Ok(Ok(value)),
            // The same bytes again, for the value or the reason it has none.
            None => Ok(self.general(table, column, &mut whole).unwrap_or_else(Err)),
        }
    }
}

impl RowPlan {
    /// Compiles the plan for `table_map`'s images against `table`, or
    /// `None` when the table map cannot be read column by column; the
    /// caller then decodes the event the general way.
    pub(crate) fn build(
        table: &SourceTable,
        table_map: &TableMapEvent<'_>,
        alignment: &RowAlignment,
    ) -> Option<Self> {
        let count = usize::try_from(table_map.columns_count()).ok()?;
        let optional = OptionalMetaExtractor::new(table_map.iter_optional_meta()).ok()?;
        let mut signedness = optional.iter_signedness();
        // Optional metadata that does not parse refuses every row of the
        // event in the general decoder; leave that verdict to it. These
        // are read a column at a time: some never end by themselves.
        let mut charsets = optional.iter_charset();
        let mut label_charsets = optional.iter_enum_and_set_charset();
        let mut names = optional.iter_column_name();
        let mut described = Vec::with_capacity(count);
        for ordinal in 0..count {
            let kind = table_map.get_column_type(ordinal).ok().flatten()?;
            let metadata = table_map.get_column_metadata(ordinal).unwrap_or(&[]);
            let unsigned = kind
                .is_numeric_type()
                .then(|| signedness.next())
                .flatten()
                .unwrap_or_default();
            if kind.is_character_type() {
                charsets.next().transpose().ok()?;
            } else if kind.is_enum_or_set_type() {
                label_charsets.next().transpose().ok()?;
            }
            names.next().transpose().ok()?;
            described.push((kind, metadata, unsigned));
        }
        let mut plan = Self::for_columns(table, &described, alignment)?;
        plan.table_map = Some(table_map.clone().into_owned());
        Some(plan)
    }

    /// The plan for images whose columns are of these types, metadata and
    /// signedness, in table-map order.
    fn for_columns(
        table: &SourceTable,
        described: &[(ColumnType, &[u8], bool)],
        alignment: &RowAlignment,
    ) -> Option<Self> {
        let mut columns = Vec::with_capacity(described.len());
        for (ordinal, (kind, metadata, unsigned)) in described.iter().copied().enumerate() {
            let placement = match alignment {
                RowAlignment::Positional if ordinal < table.columns.len() => {
                    Placement::Column(ordinal)
                }
                RowAlignment::Positional => Placement::Beyond,
                RowAlignment::ByName { image_to_schema } => image_to_schema
                    .get(ordinal)
                    .copied()
                    .flatten()
                    .map_or(Placement::Dropped, Placement::Column),
            };
            let column = match placement {
                Placement::Column(index) => Some(table.columns.get(index)?),
                Placement::Dropped | Placement::Beyond => None,
            };
            columns.push(PlanColumn::new(kind, metadata, unsigned, placement, column));
        }
        let key = if table.key.mode == KeyMode::AppendRowId {
            Vec::new()
        } else {
            table
                .key
                .columns
                .iter()
                .map(|key| {
                    table
                        .columns
                        .iter()
                        .position(|column| column.name.eq_ignore_ascii_case(key))
                })
                .collect::<Option<Vec<_>>>()?
        };
        Some(Self {
            table_map: None,
            source: table.clone(),
            columns,
            key,
        })
    }

    /// Whether the plan was compiled for exactly this table map and schema.
    pub(crate) fn matches(&self, table: &SourceTable, table_map: &TableMapEvent<'_>) -> bool {
        self.table_map
            .as_ref()
            .is_some_and(|compiled| compiled == table_map)
            && self.source == *table
    }

    /// Reads one row image off `data` into the schema's column order.
    ///
    /// `present` names the table-map ordinal of each value the image
    /// carries; columns it leaves out read NULL. With `only_key`, just the
    /// key columns are produced and every other value is stepped over.
    ///
    /// # Errors
    ///
    /// The outer error means the event cannot be followed past this image.
    /// The inner one refuses the row and leaves `data` at the next image.
    pub(crate) fn read_image(
        &self,
        present: &[usize],
        data: &mut &[u8],
        only_key: bool,
        values: &mut Vec<Value>,
    ) -> Result<Result<(), CdcError>, CdcError> {
        let table = &self.source;
        values.clear();
        values.resize(table.columns.len(), Value::Null);
        let nulls = take(data, present.len().div_ceil(8), &table.name)?;
        let mut refused = None;
        for (position, ordinal) in present.iter().enumerate() {
            let column = self.columns.get(*ordinal).ok_or_else(|| {
                CdcError::Decode(format!(
                    "{} row image names column {ordinal} of a {}-column table map",
                    table.name,
                    self.columns.len()
                ))
            })?;
            let target = match column.placement {
                Placement::Column(index) if !only_key || self.key.contains(&index) => Some(index),
                Placement::Column(_) | Placement::Dropped => None,
                Placement::Beyond => {
                    refused.get_or_insert_with(|| {
                        CdcError::Decode(format!(
                            "{} row image column {ordinal} is beyond its {}-column schema",
                            table.name,
                            table.columns.len()
                        ))
                    });
                    None
                }
            };
            if nulls[position / 8] & (1 << (position % 8)) != 0 {
                continue;
            }
            let decoded =
                column.read(&table.name, target.map(|index| &table.columns[index]), data)?;
            match (decoded, target) {
                (Ok(value), Some(index)) => values[index] = value,
                (Err(error), _) => {
                    refused.get_or_insert(error);
                }
                (Ok(_), None) => {}
            }
        }
        Ok(refused.map_or(Ok(()), Err))
    }

    /// The physical key of a decoded row.
    pub(crate) fn key(&self, values: &[Value]) -> Result<PrimaryKey, CdcError> {
        let table = &self.source;
        if self.key.is_empty() {
            return Err(CdcError::Decode(format!(
                "{} has no stable source key",
                table.name
            )));
        }
        let parts = self
            .key
            .iter()
            .map(|index| {
                key_part(&values[*index]).ok_or_else(|| {
                    CdcError::Decode(format!(
                        "{}.{} key value is NULL",
                        table.name, table.columns[*index].name
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        PrimaryKey::new(parts).map_err(CdcError::Schema)
    }
}

#[cfg(test)]
mod tests {
    use super::{Placement, PlanColumn, Reader};
    use mysql_async::consts::ColumnType;
    use pintail_probe::SourceColumn;
    use pintail_types::{DataType, Value};

    fn column(data_type: &str, column_type: &str, pintail_type: DataType) -> SourceColumn {
        SourceColumn {
            id: 1,
            name: "value".to_owned(),
            mysql_data_type: data_type.to_owned(),
            mysql_column_type: column_type.to_owned(),
            pintail_type,
            nullable: true,
            character_set: Some("utf8mb4".to_owned()),
            collation: Some("utf8mb4_0900_ai_ci".to_owned()),
            generated_stored: false,
            generation_expression: String::new(),
            generation_captured: true,
            extra: String::new(),
            auto_increment: false,
            default_value: None,
            default_generated: false,
            ordinal: 0,
        }
    }

    /// A deterministic byte source: the tests below compare two decoders on
    /// the same bytes, so the bytes only have to be varied, not random.
    struct Bytes(u64);

    impl Bytes {
        fn next(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0.to_le_bytes()[3]
        }

        fn fill(&mut self, length: usize) -> Vec<u8> {
            (0..length).map(|_| self.next()).collect()
        }
    }

    /// Reads `raw` with the plan's own reader and with the general decoder,
    /// and requires the same value from both. Returns whether the plan read
    /// the value itself.
    fn agree(plan: &PlanColumn, source: &SourceColumn, raw: &[u8]) -> bool {
        let mut general_data = raw;
        let general = plan
            .general("t", Some(source), &mut general_data)
            .expect("the general decoder follows the value");
        let mut data = raw;
        let read = plan
            .read("t", Some(source), &mut data)
            .expect("the plan follows the value");
        assert_eq!(
            data.len(),
            general_data.len(),
            "{:?} {raw:?}: the two decoders end the value at different bytes",
            plan.reader
        );
        assert_eq!(
            format!("{read:?}"),
            format!("{general:?}"),
            "{:?} {raw:?}",
            plan.reader
        );
        let (_, body) = plan
            .reader
            .frame(&mut &*raw, "t")
            .expect("framed")
            .expect("the plan frames this type");
        plan.reader.value(body, plan.unsigned).is_some()
    }

    fn plan(
        kind: ColumnType,
        metadata: &[u8],
        unsigned: bool,
        source: &SourceColumn,
    ) -> PlanColumn {
        let plan = PlanColumn::new(kind, metadata, unsigned, Placement::Column(0), Some(source));
        assert!(
            !matches!(plan.reader, Reader::General),
            "{kind:?} {source:?} is left to the general decoder"
        );
        plan
    }

    #[test]
    fn integers_read_as_the_general_decoder_reads_them() {
        let kinds = [
            (ColumnType::MYSQL_TYPE_TINY, "tinyint", 1),
            (ColumnType::MYSQL_TYPE_SHORT, "smallint", 2),
            (ColumnType::MYSQL_TYPE_INT24, "mediumint", 3),
            (ColumnType::MYSQL_TYPE_LONG, "int", 4),
            (ColumnType::MYSQL_TYPE_LONGLONG, "bigint", 8),
        ];
        let targets = [
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Year,
        ];
        let mut bytes = Bytes(0x9e37_79b9_7f4a_7c15);
        let mut read = 0;
        for (kind, name, width) in kinds {
            for target in targets {
                for declared in [name.to_owned(), format!("{name} unsigned")] {
                    for unsigned in [false, true] {
                        // A table map cannot call a signed column unsigned.
                        if unsigned && width == 3 && declared == name {
                            continue;
                        }
                        let source = column(name, &declared, target);
                        let plan = plan(kind, &[], unsigned, &source);
                        let edges = [vec![0; width], vec![0xff; width], {
                            let mut edge = vec![0xff; width];
                            edge[width - 1] = 0x7f;
                            edge
                        }];
                        for raw in edges.into_iter().chain((0..40).map(|_| bytes.fill(width))) {
                            read += usize::from(agree(&plan, &source, &raw));
                        }
                    }
                }
            }
        }
        assert!(read > 2_000, "only {read} integers were read by the plan");
    }

    #[test]
    fn decimals_read_as_the_general_decoder_reads_them() {
        let mut bytes = Bytes(0x2545_f491_4f6c_dd1d);
        let mut read = 0_usize;
        for precision in 1..=65_u8 {
            for scale in (0..=precision.min(30)).step_by(3) {
                let source = column(
                    "decimal",
                    &format!("decimal({precision},{scale})"),
                    DataType::Decimal { precision, scale },
                );
                let plan = plan(
                    ColumnType::MYSQL_TYPE_NEWDECIMAL,
                    &[precision, scale],
                    false,
                    &source,
                );
                let (precision, scale) = (usize::from(precision), usize::from(scale));
                let size = super::decimal_size(precision, scale);
                // The digit groups in the order they are packed.
                let integer = precision - scale;
                let groups = [integer % 9]
                    .into_iter()
                    .chain(std::iter::repeat_n(9, integer / 9))
                    .chain(std::iter::repeat_n(9, scale / 9))
                    .chain([scale % 9])
                    .filter(|digits| *digits > 0)
                    .collect::<Vec<_>>();
                for round in 0..40 {
                    // Zero, then every group at its largest, then values
                    // with leading zero groups, then arbitrary digits; each
                    // in both signs.
                    let mut raw = Vec::with_capacity(size);
                    for (index, digits) in groups.iter().enumerate() {
                        let limit = super::POWERS_OF_TEN[*digits];
                        let value = match round {
                            0 | 1 => 0,
                            2 | 3 => limit - 1,
                            _ if round % 3 == 0 && index < groups.len() / 2 => 0,
                            _ => {
                                u32::from_le_bytes(bytes.fill(4).try_into().expect("four")) % limit
                            }
                        };
                        let length = super::PARTIAL_GROUP_BYTES[*digits];
                        raw.extend_from_slice(&value.to_be_bytes()[4 - length..]);
                    }
                    assert_eq!(raw.len(), size);
                    raw[0] |= 0x80;
                    if round % 2 == 1 {
                        for byte in &mut raw {
                            *byte = !*byte;
                        }
                    }
                    assert!(
                        agree(&plan, &source, &raw),
                        "decimal({precision},{scale}) {raw:?} was left to the general decoder"
                    );
                    read += 1;
                }
            }
        }
        assert!(read > 5_000, "only {read} decimals were read by the plan");
    }

    #[test]
    fn dates_and_times_read_as_the_general_decoder_reads_them() {
        let mut bytes = Bytes(0xda94_2042_e4dd_58b5);
        let mut read = 0;
        for precision in 0..=6_u8 {
            let fraction = super::fraction_bytes(precision);
            let datetime = column(
                "datetime",
                "datetime",
                DataType::DateTime64 { fsp: precision },
            );
            let plan_datetime = plan(
                ColumnType::MYSQL_TYPE_DATETIME2,
                &[precision],
                false,
                &datetime,
            );
            let timestamp = column(
                "timestamp",
                "timestamp",
                DataType::DateTime64 { fsp: precision },
            );
            let plan_timestamp = plan(
                ColumnType::MYSQL_TYPE_TIMESTAMP2,
                &[precision],
                false,
                &timestamp,
            );
            for round in 0..400 {
                // A real date and time, packed as the server packs it, in
                // most rounds; arbitrary bytes in the rest.
                let mut raw = bytes.fill(5 + fraction);
                if round % 4 != 0 {
                    let year_month =
                        (1000 + u64::from(bytes.next()) * 30) * 13 + u64::from(bytes.next() % 13);
                    let date = (year_month << 5) | u64::from(bytes.next() % 32);
                    let clock = (u64::from(bytes.next() % 24) << 12)
                        | (u64::from(bytes.next() % 60) << 6)
                        | u64::from(bytes.next() % 60);
                    let packed = ((date << 17) | clock) + 0x80_0000_0000;
                    raw[..5].copy_from_slice(&packed.to_be_bytes()[3..]);
                    if fraction > 0 {
                        raw[5] &= 0x07;
                    }
                }
                if round == 1 {
                    raw = 0x80_0000_0000_u64.to_be_bytes()[3..].to_vec();
                    raw.resize(5 + fraction, 0);
                }
                read += usize::from(agree(&plan_datetime, &datetime, &raw));
                let mut raw = bytes.fill(4 + fraction);
                if round % 4 != 0 && fraction > 0 {
                    raw[4] &= 0x07;
                }
                if round == 1 {
                    raw[..4].fill(0);
                }
                read += usize::from(agree(&plan_timestamp, &timestamp, &raw));
            }
        }
        let date = column("date", "date", DataType::Date32);
        let plan_date = plan(ColumnType::MYSQL_TYPE_NEWDATE, &[], false, &date);
        assert!(agree(&plan_date, &date, &[0, 0, 0]));
        for _ in 0..2_000 {
            read += usize::from(agree(&plan_date, &date, &bytes.fill(3)));
        }
        assert!(read > 5_000, "only {read} dates were read by the plan");
    }

    #[test]
    fn text_bytes_floats_and_labels_read_as_the_general_decoder_reads_them() {
        let mut bytes = Bytes(0x1234_5678_9abc_def1);
        let text = column("varchar", "varchar(64)", DataType::Utf8);
        let short = plan(ColumnType::MYSQL_TYPE_VARCHAR, &[64, 0], false, &text);
        let long = plan(ColumnType::MYSQL_TYPE_VARCHAR, &[0, 4], false, &text);
        let char_column = column("char", "char(8)", DataType::Utf8);
        let fixed = plan(
            ColumnType::MYSQL_TYPE_STRING,
            &[0xfe, 32],
            false,
            &char_column,
        );
        let long_text = column("text", "text", DataType::Utf8);
        let blob_text = plan(ColumnType::MYSQL_TYPE_BLOB, &[2], false, &long_text);
        let mut binary = column("varbinary", "varbinary(64)", DataType::Binary);
        binary.character_set = None;
        let raw_bytes = plan(ColumnType::MYSQL_TYPE_VARCHAR, &[64, 0], false, &binary);
        for round in 0..300_usize {
            let length = round % 40;
            // Valid UTF-8 mostly; every seventh round is arbitrary bytes,
            // which the text readers must refuse exactly as the general
            // decoder does.
            let body = if round % 7 == 0 {
                bytes.fill(length)
            } else {
                "aé€😀z"
                    .chars()
                    .cycle()
                    .take(length / 2)
                    .collect::<String>()
                    .into_bytes()
            };
            let length = u8::try_from(body.len()).expect("short");
            let one = [&[length][..], &body].concat();
            let two = [&[length, 0][..], &body].concat();
            assert_eq!(
                agree(&short, &text, &one),
                std::str::from_utf8(&body).is_ok()
            );
            agree(&long, &text, &two);
            agree(&fixed, &char_column, &one);
            agree(&blob_text, &long_text, &two);
            assert!(agree(&raw_bytes, &binary, &one));
        }
        let float = column("float", "float", DataType::Float32);
        let double = column("double", "double", DataType::Float64);
        let plan_float = plan(ColumnType::MYSQL_TYPE_FLOAT, &[4], false, &float);
        let plan_double = plan(ColumnType::MYSQL_TYPE_DOUBLE, &[8], false, &double);
        for _ in 0..500 {
            assert!(agree(&plan_float, &float, &bytes.fill(4)));
            assert!(agree(&plan_double, &double, &bytes.fill(8)));
        }
        let label = column("enum", "enum('alpha','βeta','it\\'s')", DataType::Utf8);
        let plan_label = plan(ColumnType::MYSQL_TYPE_ENUM, &[0xf7, 1], false, &label);
        for index in 0..=5_u8 {
            assert_eq!(agree(&plan_label, &label, &[index]), index <= 3);
        }
        let mut data = &[2_u8][..];
        assert_eq!(
            format!("{:?}", plan_label.read("t", Some(&label), &mut data)),
            format!(
                "{:?}",
                Ok::<_, ()>(Ok::<_, ()>(Value::Utf8("βeta".to_owned())))
            )
        );
    }

    fn parcels() -> pintail_probe::SourceTable {
        let named = |name: &str, mut source: SourceColumn| {
            source.name = name.to_owned();
            source
        };
        let mut id = named("id", column("bigint", "bigint", DataType::Int64));
        id.nullable = false;
        pintail_probe::SourceTable {
            name: "parcels".to_owned(),
            engine: None,
            estimated_rows: None,
            rows_are_exact: false,
            columns: vec![
                id,
                named("label", column("varchar", "varchar(64)", DataType::Utf8)),
                named("weight", column("mediumint", "mediumint", DataType::Int32)),
            ],
            key: pintail_probe::SourceKey {
                mode: pintail_types::KeyMode::Primary,
                index_name: None,
                columns: vec!["ID".to_owned()],
            },
            unique_keys: Vec::new(),
            requires_reconciliation: false,
            foreign_keys: Vec::new(),
            secondary_indexes: Vec::new(),
            warnings: Vec::new(),
            source_column_count: 0,
        }
    }

    const PARCEL_COLUMNS: [(ColumnType, &[u8], bool); 3] = [
        (ColumnType::MYSQL_TYPE_LONGLONG, &[], false),
        (ColumnType::MYSQL_TYPE_VARCHAR, &[64, 0], false),
        (ColumnType::MYSQL_TYPE_INT24, &[], false),
    ];

    #[test]
    fn a_row_image_is_read_into_the_schema_order_with_its_nulls() {
        use super::{RowAlignment, RowPlan};
        let table = parcels();
        let plan = RowPlan::for_columns(&table, &PARCEL_COLUMNS, &RowAlignment::Positional)
            .expect("a plan");
        assert_eq!(plan.key, vec![0]);
        // Two rows back to back: (7, 'ab', -2), then (8, NULL, NULL).
        let image = [
            &[0b000_u8][..],
            &7_i64.to_le_bytes(),
            &[2, b'a', b'b'],
            &[0xfe, 0xff, 0xff],
            &[0b110],
            &8_i64.to_le_bytes(),
        ]
        .concat();
        let mut data = image.as_slice();
        let mut values = Vec::new();
        plan.read_image(&[0, 1, 2], &mut data, false, &mut values)
            .expect("followed")
            .expect("read");
        assert_eq!(
            values,
            vec![
                Value::Int64(7),
                Value::Utf8("ab".to_owned()),
                Value::Int64(-2)
            ]
        );
        assert_eq!(
            plan.key(&values).expect("key").parts(),
            [pintail_types::KeyPart::Int64(7)]
        );
        // Only the key of the second, with the image still stepped over.
        plan.read_image(&[0, 1, 2], &mut data, true, &mut values)
            .expect("followed")
            .expect("read");
        assert_eq!(values, vec![Value::Int64(8), Value::Null, Value::Null]);
        assert!(data.is_empty());
        // An image that carries the key alone leaves the rest NULL.
        let key_only = [&[0_u8][..], &9_i64.to_le_bytes()].concat();
        plan.read_image(&[0], &mut key_only.as_slice(), false, &mut values)
            .expect("followed")
            .expect("read");
        assert_eq!(values, vec![Value::Int64(9), Value::Null, Value::Null]);
        // A NULL key refuses the row; the image is still followed.
        plan.read_image(&[0], &mut &[1_u8][..], false, &mut values)
            .expect("followed")
            .expect("read");
        assert!(plan.key(&values).is_err());
        // An image cut short cannot be followed at all.
        assert!(
            plan.read_image(&[0, 1, 2], &mut &image[..6], false, &mut values)
                .is_err()
        );
    }

    #[test]
    fn a_row_image_wider_or_older_than_the_schema_is_placed_or_refused() {
        use super::{RowAlignment, RowPlan};
        let table = parcels();
        let wide = [
            PARCEL_COLUMNS[0],
            PARCEL_COLUMNS[1],
            PARCEL_COLUMNS[2],
            (ColumnType::MYSQL_TYPE_LONG, &[][..], false),
        ];
        let image = [
            &[0_u8][..],
            &7_i64.to_le_bytes(),
            &[1, b'x'],
            &[1, 0, 0],
            &5_i32.to_le_bytes(),
            &[0xaa],
        ]
        .concat();
        // Position by position, the fourth value has no column: the row is
        // refused, and the bytes behind it are still where the next row starts.
        let positional =
            RowPlan::for_columns(&table, &wide, &RowAlignment::Positional).expect("a plan");
        let mut data = image.as_slice();
        let mut values = Vec::new();
        assert!(
            positional
                .read_image(&[0, 1, 2, 3], &mut data, false, &mut values)
                .expect("followed")
                .is_err()
        );
        assert_eq!(data, [0xaa]);
        // Placed by name, a column the schema dropped is stepped over and
        // the others land where the placement says.
        let by_name = RowPlan::for_columns(
            &table,
            &wide,
            &RowAlignment::ByName {
                image_to_schema: vec![Some(0), Some(1), None, Some(2)],
            },
        )
        .expect("a plan");
        // The fourth value is framed by the table map's type, four bytes,
        // and stored as the schema column it is placed on.
        let mut data = image.as_slice();
        by_name
            .read_image(&[0, 1, 2, 3], &mut data, false, &mut values)
            .expect("followed")
            .expect("read");
        assert_eq!(
            values,
            vec![
                Value::Int64(7),
                Value::Utf8("x".to_owned()),
                Value::Int64(5)
            ]
        );
        assert_eq!(data, [0xaa]);
    }

    #[test]
    fn types_the_plan_does_not_read_are_left_to_the_general_decoder() {
        let json = column("json", "json", DataType::Json);
        let plan = PlanColumn::new(
            ColumnType::MYSQL_TYPE_JSON,
            &[4],
            false,
            Placement::Column(0),
            Some(&json),
        );
        assert!(matches!(plan.reader, Reader::General));
        // A latin1 column is transcoded, which the plan leaves alone.
        let mut latin = column("varchar", "varchar(8)", DataType::Utf8);
        latin.character_set = Some("latin1".to_owned());
        let plan = PlanColumn::new(
            ColumnType::MYSQL_TYPE_VARCHAR,
            &[8, 0],
            false,
            Placement::Column(0),
            Some(&latin),
        );
        assert!(matches!(plan.reader, Reader::General));
        let mut data = &[1_u8, 0xe9][..];
        let read = plan.read("t", Some(&latin), &mut data).expect("followed");
        assert_eq!(format!("{read:?}"), "Ok(Utf8(\"é\"))");
        assert!(data.is_empty());
    }
}
