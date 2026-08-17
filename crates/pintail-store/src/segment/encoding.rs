//! Block payload codec: compression selection and the column
//! encodings (dictionary, run-length, bit-packed, delta) with their
//! integer normalization and bit packing.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use lz4_flex::block::{compress as lz4_compress, decompress as lz4_decompress};
use xxhash_rust::xxh3::xxh3_64;

use super::{Cell, Compression, Encoding, LogicalType, decode_cell, encode_cell};
use crate::{
    StoreError,
    codec::{Decoder, Encoder},
};

pub(super) fn materially_smaller(uncompressed: usize, compressed: usize) -> bool {
    uncompressed > 0 && compressed.saturating_mul(100) <= uncompressed.saturating_mul(95)
}

pub(super) fn compress_block_for_storage(
    compression: Compression,
    bytes: &[u8],
) -> Result<(Compression, Vec<u8>), StoreError> {
    match compression {
        Compression::None => Ok((Compression::None, bytes.to_vec())),
        Compression::Lz4 => Ok((Compression::Lz4, lz4_compress(bytes))),
        Compression::Zstd => zstd::bulk::compress(bytes, 3)
            .map(|compressed| (Compression::Zstd, compressed))
            .map_err(|error| StoreError::io("compress zstd segment block", error)),
        Compression::AdaptiveLz4 => {
            let compressed = lz4_compress(bytes);
            if materially_smaller(bytes.len(), compressed.len()) {
                Ok((Compression::Lz4, compressed))
            } else {
                Ok((Compression::None, bytes.to_vec()))
            }
        }
    }
}

pub(super) fn decompress_block(
    compression: Compression,
    bytes: &[u8],
    uncompressed_length: usize,
) -> Result<Vec<u8>, String> {
    match compression {
        Compression::None => {
            if bytes.len() != uncompressed_length {
                return Err(format!(
                    "raw block length is {}, expected {uncompressed_length}",
                    bytes.len()
                ));
            }
            Ok(bytes.to_vec())
        }
        Compression::Lz4 => lz4_decompress(bytes, uncompressed_length)
            .map_err(|error| format!("invalid LZ4 block: {error}")),
        Compression::Zstd => zstd::bulk::decompress(bytes, uncompressed_length)
            .map_err(|error| format!("invalid zstd block: {error}")),
        Compression::AdaptiveLz4 => {
            Err("adaptive LZ4 is a writer policy, not a stored compression".to_owned())
        }
    }
}

pub(super) fn select_encoding(logical_type: LogicalType, cells: &[Cell]) -> Encoding {
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

pub(super) fn compare_cells(left: &Cell, right: &Cell) -> Ordering {
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

pub(super) fn hll_registers(encoded_values: &[Vec<u8>]) -> [u8; 64] {
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

pub(super) fn encode_payload(
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

pub(super) fn decode_payload(
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

pub(super) fn decoded_heap_upper_bound(
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

pub(super) fn decode_integer_base(
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

/// LSB-first bitstream cursor over a validated payload.
///
/// The existing [`unpack`] builds a zeroed 16-byte window per value, copies
/// up to 16 payload bytes into it, and converts through `u128` - per value.
/// This reader keeps a rolling accumulator instead, refilling eight bytes at
/// a time, so decoding a value is a shift and a mask. Same wire format, same
/// LSB-first order.
struct BitReader<'bytes> {
    bytes: &'bytes [u8],
    cursor: usize,
    accumulator: u128,
    live_bits: u32,
}

impl<'bytes> BitReader<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            accumulator: 0,
            live_bits: 0,
        }
    }

    #[inline]
    fn read(&mut self, width: u32, mask: u64) -> u64 {
        while self.live_bits < width {
            if self.cursor + 8 <= self.bytes.len() {
                let word = u64::from_le_bytes(
                    self.bytes[self.cursor..self.cursor + 8]
                        .try_into()
                        .expect("eight bytes"),
                );
                self.accumulator |= u128::from(word) << self.live_bits;
                self.live_bits += 64;
                self.cursor += 8;
            } else if self.cursor < self.bytes.len() {
                self.accumulator |= u128::from(self.bytes[self.cursor]) << self.live_bits;
                self.live_bits += 8;
                self.cursor += 1;
            } else {
                // Validated payloads always hold enough bits; padding in the
                // final byte reads as zeros through the mask.
                break;
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let value = (self.accumulator as u64) & mask;
        self.accumulator >>= width;
        self.live_bits = self.live_bits.saturating_sub(width);
        value
    }
}

/// Reads the bit-packed payload header exactly as [`unpack`] does, returning
/// the width and the validated byte slice.
fn unpack_header<'payload>(
    decoder: &mut Decoder<'payload>,
    value_count: usize,
) -> Result<(u32, &'payload [u8]), String> {
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
    Ok((u32::from(width), bytes))
}

const fn width_mask(width: u32) -> u64 {
    if width == 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    }
}

/// Decodes a bit-packed payload, adds the block base, and appends signed
/// values straight into the destination - one pass, no temporary vector.
///
/// Matches the two-pass path's semantics exactly, including its overflow
/// error strings. When the base and width prove every possible value fits,
/// the per-value overflow checks hoist out of the loop entirely.
pub(super) fn unpack_signed_into(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    base: i128,
    out: &mut Vec<i64>,
) -> Result<(), String> {
    let (width, bytes) = unpack_header(decoder, value_count)?;
    let mask = width_mask(width);
    out.reserve(value_count);
    let mut reader = BitReader::new(bytes);
    let in_range = base >= i128::from(i64::MIN)
        && base
            .checked_add(i128::from(mask))
            .is_some_and(|top| top <= i128::from(i64::MAX));
    if in_range && width < 64 {
        #[allow(clippy::cast_possible_truncation)]
        let base = base as i64;
        for _ in 0..value_count {
            #[allow(clippy::cast_possible_wrap)]
            let value = reader.read(width, mask) as i64;
            out.push(base.wrapping_add(value));
        }
        return Ok(());
    }
    for _ in 0..value_count {
        let normalized = reader.read(width, mask);
        let value = base
            .checked_add(i128::from(normalized))
            .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
        out.push(i64::try_from(value).map_err(|_| "bit-packed signed integer overflow")?);
    }
    Ok(())
}

/// The unsigned twin of [`unpack_signed_into`].
pub(super) fn unpack_unsigned_into(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    base: i128,
    out: &mut Vec<u64>,
) -> Result<(), String> {
    let (width, bytes) = unpack_header(decoder, value_count)?;
    let mask = width_mask(width);
    out.reserve(value_count);
    let mut reader = BitReader::new(bytes);
    let in_range = base >= 0
        && base
            .checked_add(i128::from(mask))
            .is_some_and(|top| top <= i128::from(u64::MAX));
    if in_range {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let base = base as u64;
        for _ in 0..value_count {
            out.push(base.wrapping_add(reader.read(width, mask)));
        }
        return Ok(());
    }
    for _ in 0..value_count {
        let normalized = reader.read(width, mask);
        let value = base
            .checked_add(i128::from(normalized))
            .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
        out.push(u64::try_from(value).map_err(|_| "bit-packed unsigned integer overflow")?);
    }
    Ok(())
}

pub(super) fn unpack(decoder: &mut Decoder<'_>, value_count: usize) -> Result<Vec<u64>, String> {
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

#[cfg(test)]
mod bit_reader_tests {
    use super::*;

    /// Builds a raw bit-packed payload the way the writer lays it out:
    /// [width u8][length-prefixed bytes], via the same Encoder the format
    /// uses elsewhere. To stay independent of the writer, bytes are random
    /// and both readers parse the identical buffer.
    fn payload(width: u8, value_count: usize, seed: u64) -> Vec<u8> {
        let bits = value_count * usize::from(width);
        let mut bytes = vec![0_u8; bits.div_ceil(8)];
        let mut state = seed | 1;
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state & 0xFF) as u8;
        }
        let mut framed = Vec::new();
        framed.push(width);
        framed.extend_from_slice(
            &u32::try_from(bytes.len())
                .expect("test payload fits u32")
                .to_le_bytes(),
        );
        framed.extend_from_slice(&bytes);
        framed
    }

    #[test]
    fn streaming_reader_matches_the_windowed_unpack() {
        for &width in &[0_u8, 1, 3, 7, 8, 13, 24, 31, 32, 33, 63, 64] {
            for &count in &[0_usize, 1, 2, 63, 64, 65, 2_290] {
                let framed = payload(width, count, u64::from(width) * 31 + count as u64);
                let expected = unpack(&mut Decoder::new(&framed), count).expect("windowed unpack");
                let mut streamed = Vec::new();
                unpack_unsigned_into(&mut Decoder::new(&framed), count, 0, &mut streamed)
                    .expect("streaming unpack");
                assert_eq!(expected, streamed, "width {width} count {count}");
            }
        }
    }

    #[test]
    fn signed_bases_round_trip_against_the_two_pass_arithmetic() {
        for &base in &[
            0_i128,
            -1,
            42,
            i128::from(i64::MIN),
            i128::from(i64::MAX) - 200,
        ] {
            let width = 8_u8;
            let count = 200_usize;
            let framed = payload(width, count, 7);
            let normalized = unpack(&mut Decoder::new(&framed), count).expect("unpack");
            let expected: Result<Vec<i64>, String> = normalized
                .iter()
                .map(|value| {
                    let sum = base
                        .checked_add(i128::from(*value))
                        .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
                    i64::try_from(sum).map_err(|_| "bit-packed signed integer overflow".to_owned())
                })
                .collect();
            let mut streamed = Vec::new();
            let outcome =
                unpack_signed_into(&mut Decoder::new(&framed), count, base, &mut streamed);
            match expected {
                Ok(values) => {
                    outcome.expect("in-range base decodes");
                    assert_eq!(values, streamed, "base {base}");
                }
                Err(message) => {
                    assert_eq!(outcome.expect_err("overflow must error"), message);
                }
            }
        }
    }

    #[test]
    fn unsigned_negative_base_errors_exactly_like_the_two_pass_path() {
        // A negative base with a value too small to lift it back above zero
        // must produce the same error string the old path produced.
        let framed = payload(4, 16, 3);
        let mut streamed = Vec::new();
        let outcome = unpack_unsigned_into(&mut Decoder::new(&framed), 16, -1_000, &mut streamed);
        assert_eq!(
            outcome.expect_err("negative base under unsigned must error"),
            "bit-packed unsigned integer overflow"
        );
    }
}
