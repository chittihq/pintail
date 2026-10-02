//! Block payload codec: compression selection and the column
//! encodings (dictionary, run-length, bit-packed, delta) with their
//! integer normalization and bit packing.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use lz4_flex::block::compress as lz4_compress;
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
        Compression::Framed => Err(StoreError::FormatLimit(
            "framed is a stored block layout, not a writer policy".into(),
        )),
    }
}

/// Uncompressed bytes a frame of a framed block aims for. A frame closes at
/// the first value boundary at or past this size, so a value is never split
/// and one wider than the target sits in a frame of its own.
pub(super) const FRAME_TARGET_BYTES: usize = 64 * 1024;

/// The smallest plain text payload stored as frames. Below four frames the
/// saving on a sparse read is too small to pay for the frame directory.
pub(super) const FRAMED_MINIMUM_BYTES: usize = 4 * FRAME_TARGET_BYTES;

/// Bytes one frame occupies in a framed block's directory.
pub(super) const FRAME_ENTRY_BYTES: usize = 4 + 4 + 1 + 4 + 8;

/// One independently compressed run of whole values inside a framed block,
/// as its directory entry describes it.
pub(super) struct FrameEntry {
    /// Non-null ordinal, within the block, of the frame's first value.
    pub(super) first_value: usize,
    pub(super) value_count: usize,
    pub(super) uncompressed_length: usize,
    pub(super) codec: Compression,
    /// Offset of the stored bytes from the end of the directory.
    pub(super) stored_offset: usize,
    pub(super) stored_length: usize,
    pub(super) checksum: u64,
}

impl FrameEntry {
    pub(super) fn end_value(&self) -> usize {
        self.first_value + self.value_count
    }
}

/// A frame and its stored bytes.
pub(super) struct Frame<'a> {
    pub(super) entry: FrameEntry,
    data: &'a [u8],
}

/// Compresses a plain length-prefixed value payload as a directory of
/// frames, each holding whole values and compressed on its own, so a read of
/// a few rows decompresses the frames holding them instead of the block.
/// Decompressing every frame in order yields `bytes` exactly, so a full
/// decode sees the same payload an unframed block stores.
///
/// Layout: `u32 frame_count`; per frame `u32 value_count`,
/// `u32 uncompressed_length`, `u8 codec`, `u32 stored_length` and
/// `u64 xxh3(stored bytes)`; then `u64 xxh3(block_head ++ directory)`; then
/// every frame's stored bytes in order. `block_head` is the block payload
/// before its compressed bytes (row count, null bitmap, encoding, the framed
/// tag and the uncompressed length), so a reader that loads only the head,
/// the directory and the frames it needs can check each of them without
/// reading the rest of the block. Under the adaptive policy a frame LZ4
/// cannot shrink by 5% is stored raw, and a block whose frames together do
/// not save 5% is stored raw and unframed.
pub(super) fn compress_framed_for_storage(
    policy: Compression,
    block_head: &[u8],
    bytes: &[u8],
) -> Result<(Compression, Vec<u8>), StoreError> {
    if matches!(policy, Compression::None | Compression::Framed) {
        return compress_block_for_storage(policy, bytes);
    }
    let mut frames = Vec::new();
    let mut decoder = Decoder::new(bytes);
    let mut start = 0_usize;
    let mut values = 0_usize;
    while decoder.position() < bytes.len() {
        decoder
            .bytes()
            .map_err(|reason| StoreError::FormatLimit(format!("plain payload: {reason}")))?;
        values += 1;
        if decoder.position() - start >= FRAME_TARGET_BYTES {
            frames.push((values, start..decoder.position()));
            start = decoder.position();
            values = 0;
        }
    }
    if values > 0 {
        frames.push((values, start..bytes.len()));
    }
    let mut directory = Encoder::new();
    directory.length(frames.len(), "frame count")?;
    let mut data = Vec::with_capacity(bytes.len() / 2);
    for (value_count, range) in frames {
        let raw = &bytes[range];
        let (codec, stored) = match policy {
            Compression::Zstd => compress_block_for_storage(Compression::Zstd, raw)?,
            Compression::Lz4 => (Compression::Lz4, lz4_compress(raw)),
            _ => compress_block_for_storage(Compression::AdaptiveLz4, raw)?,
        };
        directory.length(value_count, "frame value count")?;
        directory.length(raw.len(), "frame length")?;
        directory.u8(codec as u8);
        directory.length(stored.len(), "frame stored length")?;
        directory.u64(xxh3_64(&stored));
        data.extend_from_slice(&stored);
    }
    let digest = framed_head_digest(block_head, directory.as_slice());
    directory.u64(digest);
    let mut stored_block = directory.finish();
    stored_block.extend_from_slice(&data);
    if policy == Compression::AdaptiveLz4 && !materially_smaller(bytes.len(), stored_block.len()) {
        return Ok((Compression::None, bytes.to_vec()));
    }
    Ok((Compression::Framed, stored_block))
}

pub(super) fn framed_head_digest(block_head: &[u8], directory: &[u8]) -> u64 {
    let mut bytes = Vec::with_capacity(block_head.len() + directory.len());
    bytes.extend_from_slice(block_head);
    bytes.extend_from_slice(directory);
    xxh3_64(&bytes)
}

/// Parses the frame entries of a directory (`u32 frame_count` and the
/// entries, without the trailing digest), checking that the frames account
/// for exactly `uncompressed_length` bytes.
pub(super) fn parse_frame_directory(
    directory: &[u8],
    uncompressed_length: usize,
) -> Result<Vec<FrameEntry>, String> {
    let mut decoder = Decoder::new(directory);
    let frame_count = decoder.count(FRAME_ENTRY_BYTES)?;
    let mut entries = Vec::with_capacity(frame_count);
    let mut first_value = 0_usize;
    let mut stored_offset = 0_usize;
    let mut total = 0_usize;
    for _ in 0..frame_count {
        let value_count = decoder.u32()? as usize;
        let length = decoder.u32()? as usize;
        let codec = match decoder.u8()? {
            0 => Compression::None,
            1 => Compression::Lz4,
            2 => Compression::Zstd,
            tag => return Err(format!("unknown frame compression {tag}")),
        };
        let stored_length = decoder.u32()? as usize;
        let checksum = decoder.u64()?;
        if value_count == 0 {
            return Err("empty frame".to_owned());
        }
        entries.push(FrameEntry {
            first_value,
            value_count,
            uncompressed_length: length,
            codec,
            stored_offset,
            stored_length,
            checksum,
        });
        first_value = first_value
            .checked_add(value_count)
            .ok_or("frame value count overflow")?;
        stored_offset = stored_offset
            .checked_add(stored_length)
            .ok_or("frame stored length overflow")?;
        total = total.checked_add(length).ok_or("frame length overflow")?;
    }
    decoder.finish()?;
    if total != uncompressed_length {
        return Err(format!(
            "frames hold {total} bytes, block declares {uncompressed_length}"
        ));
    }
    Ok(entries)
}

/// Splits an in-memory framed payload into its frames. The block checksum
/// already covers these bytes, so the per-frame checksums are not re-read.
pub(super) fn parse_frames(
    bytes: &[u8],
    uncompressed_length: usize,
) -> Result<Vec<Frame<'_>>, String> {
    let mut decoder = Decoder::new(bytes);
    let frame_count = decoder.count(FRAME_ENTRY_BYTES)?;
    let directory_length = 4 + frame_count * FRAME_ENTRY_BYTES;
    let directory = bytes
        .get(..directory_length)
        .ok_or("frame directory is truncated")?;
    let entries = parse_frame_directory(directory, uncompressed_length)?;
    let data = bytes
        .get(directory_length + 8..)
        .ok_or("frame directory digest is truncated")?;
    let stored = entries
        .last()
        .map_or(0, |entry| entry.stored_offset + entry.stored_length);
    if stored != data.len() {
        return Err(format!(
            "frames store {stored} bytes, block holds {}",
            data.len()
        ));
    }
    Ok(entries
        .into_iter()
        .map(|entry| {
            let data = &data[entry.stored_offset..entry.stored_offset + entry.stored_length];
            Frame { entry, data }
        })
        .collect())
}

/// Decompresses one frame's stored bytes.
pub(super) fn decompress_frame(entry: &FrameEntry, stored: &[u8]) -> Result<Vec<u8>, String> {
    decompress_block(entry.codec, stored, entry.uncompressed_length)
}

pub(super) fn decompress_block(
    compression: Compression,
    bytes: &[u8],
    uncompressed_length: usize,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    decompress_block_into(compression, bytes, uncompressed_length, &mut output)?;
    Ok(output)
}

/// [`decompress_block`] into a caller's buffer, replacing its contents, so a
/// scan can reuse one buffer across blocks.
pub(super) fn decompress_block_into(
    compression: Compression,
    bytes: &[u8],
    uncompressed_length: usize,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    output.clear();
    match compression {
        Compression::None => {
            if bytes.len() != uncompressed_length {
                return Err(format!(
                    "raw block length is {}, expected {uncompressed_length}",
                    bytes.len()
                ));
            }
            output.extend_from_slice(bytes);
        }
        Compression::Lz4 => {
            output.resize(uncompressed_length, 0);
            let written = lz4_flex::block::decompress_into(bytes, output)
                .map_err(|error| format!("invalid LZ4 block: {error}"))?;
            if written != uncompressed_length {
                return Err(format!(
                    "invalid LZ4 block: {written} bytes, expected {uncompressed_length}"
                ));
            }
        }
        Compression::Zstd => {
            *output = zstd::bulk::decompress(bytes, uncompressed_length)
                .map_err(|error| format!("invalid zstd block: {error}"))?;
        }
        Compression::AdaptiveLz4 => {
            return Err("adaptive LZ4 is a writer policy, not a stored compression".to_owned());
        }
        Compression::Framed => {
            // Frames decompress in place into one buffer, and zstd frames
            // share one context: a context per 64 KiB frame, or a copy per
            // frame, cost a full-column scan about a tenth of its time.
            let frames = parse_frames(bytes, uncompressed_length)?;
            output.resize(uncompressed_length, 0);
            let mut zstd_context = None;
            let mut offset = 0_usize;
            for Frame { entry, data } in &frames {
                let end = offset + entry.uncompressed_length;
                let destination = &mut output[offset..end];
                let written = match entry.codec {
                    Compression::None => {
                        if data.len() != destination.len() {
                            return Err("raw frame length differs from its directory".to_owned());
                        }
                        destination.copy_from_slice(data);
                        destination.len()
                    }
                    Compression::Lz4 => lz4_flex::block::decompress_into(data, destination)
                        .map_err(|error| format!("invalid LZ4 frame: {error}"))?,
                    _ => {
                        if zstd_context.is_none() {
                            zstd_context = Some(
                                zstd::bulk::Decompressor::new()
                                    .map_err(|error| format!("zstd context: {error}"))?,
                            );
                        }
                        zstd_context
                            .as_mut()
                            .expect("context was just created")
                            .decompress_to_buffer(data, destination)
                            .map_err(|error| format!("invalid zstd frame: {error}"))?
                    }
                };
                if written != entry.uncompressed_length {
                    return Err(
                        "frame decompressed to a length other than its directory's".to_owned()
                    );
                }
                offset = end;
            }
        }
    }
    Ok(())
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
        // The narrow form is made from the wide one by the block writer.
        Encoding::Dictionary | Encoding::NarrowDictionary => {
            encode_dictionary(&mut encoder, cells)?;
        }
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
        Encoding::Dictionary => decode_dictionary(&mut decoder, logical_type, value_count, false)?,
        Encoding::NarrowDictionary => {
            decode_dictionary(&mut decoder, logical_type, value_count, true)?
        }
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
        Encoding::Dictionary | Encoding::NarrowDictionary => {
            let dictionary_count = decoder.u32()? as usize;
            let mut maximum = 0_usize;
            for _ in 0..dictionary_count {
                maximum = maximum.max(decoder.bytes()?.len());
            }
            let width = if encoding == Encoding::NarrowDictionary {
                dictionary_index_width(&mut decoder)?
            } else {
                4
            };
            for _ in 0..value_count {
                let index = dictionary_index(&mut decoder, width)?;
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

/// The most entries a column's segment-wide dictionary holds before its
/// blocks go back to dictionaries of their own.
const COLUMN_DICTIONARY_ENTRIES: usize = 1024;

/// The most bytes its entries hold together. A block lists the dictionary's
/// leading entries whether it uses them or not, so long values repeated in
/// runs - each block its own - would be written again by every later block.
const COLUMN_DICTIONARY_BYTES: usize = 32 * 1024;

/// One text column's dictionary across the blocks of a segment being
/// written: every value takes the code of its first appearance in the
/// segment and keeps it in every later block.
///
/// A block still carries its own dictionary and is read alone, but that
/// dictionary is the leading entries of this one, so the codes of two
/// blocks mean the same values. A reader assembling several blocks then
/// copies codes instead of translating each one, where blocks that each
/// numbered their values in their own order of appearance made it look
/// every code up. A column with more values than the limit, or longer ones
/// than the byte limit together, stops: its later blocks number their own
/// values, as every block did before.
#[derive(Default)]
pub(super) struct ColumnDictionary {
    positions: HashMap<Cell, u32>,
    entries: Vec<Cell>,
    /// Bytes of text the entries hold.
    bytes: usize,
    closed: bool,
}

impl ColumnDictionary {
    /// Whether every one of `cells` already has a code here.
    pub(super) fn holds_all(&self, cells: &[Cell]) -> bool {
        !self.closed
            && !cells.is_empty()
            && cells.iter().all(|cell| self.positions.contains_key(cell))
    }

    pub(super) const fn is_open(&self) -> bool {
        !self.closed
    }

    /// The dictionary payload of one block coded against the column, or
    /// `None` once the column holds too many values.
    pub(super) fn encode(&mut self, cells: &[Cell]) -> Result<Option<Vec<u8>>, StoreError> {
        if self.closed {
            return Ok(None);
        }
        let mut indices = Vec::with_capacity(cells.len());
        let mut listed = 0_usize;
        for cell in cells {
            let index = if let Some(index) = self.positions.get(cell) {
                *index
            } else {
                if let Cell::Utf8(text) = cell {
                    self.bytes = self.bytes.saturating_add(text.len());
                }
                if self.entries.len() >= COLUMN_DICTIONARY_ENTRIES
                    || self.bytes > COLUMN_DICTIONARY_BYTES
                {
                    self.closed = true;
                    return Ok(None);
                }
                let index = u32::try_from(self.entries.len())
                    .map_err(|_| StoreError::FormatLimit("dictionary exceeds u32::MAX".into()))?;
                self.positions.insert(cell.clone(), index);
                self.entries.push(cell.clone());
                index
            };
            listed = listed.max(index as usize + 1);
            indices.push(index);
        }
        let mut encoder = Encoder::new();
        encoder.length(listed, "block dictionary")?;
        for value in &self.entries[..listed] {
            encode_cell(&mut encoder, value)?;
        }
        for index in indices {
            encoder.u32(index);
        }
        Ok(Some(encoder.finish()))
    }
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

/// The dictionary payload `wide` (entry count, entries, a `u32` index per
/// value) with its indexes in one byte each when there are at most 256
/// entries, or two when there are at most 65,536: the same count and
/// entries, then the width, then the indexes. `None` when the entries are
/// not length-prefixed bytes laid out as expected, or need all four bytes.
pub(super) fn narrow_dictionary_payload(wide: &[u8], value_count: usize) -> Option<Vec<u8>> {
    let mut decoder = Decoder::new(wide);
    let entries = decoder.u32().ok()? as usize;
    let width = match entries {
        0..=256 => 1_usize,
        257..=65_536 => 2,
        _ => return None,
    };
    for _ in 0..entries {
        decoder.bytes().ok()?;
    }
    let head = decoder.position();
    let indexes = decoder.take(value_count.checked_mul(4)?).ok()?;
    decoder.finish().ok()?;
    let mut narrow = Vec::with_capacity(head + 1 + value_count * width);
    narrow.extend_from_slice(&wide[..head]);
    narrow.push(u8::try_from(width).ok()?);
    for index in indexes.chunks_exact(4) {
        // An index names an entry, so its high bytes are zero.
        if index[width..].iter().any(|byte| *byte != 0) {
            return None;
        }
        narrow.extend_from_slice(&index[..width]);
    }
    Some(narrow)
}

/// The byte width of a narrow dictionary's indexes, read from its payload
/// after the entries.
pub(super) fn dictionary_index_width(decoder: &mut Decoder<'_>) -> Result<usize, String> {
    match decoder.u8()? {
        1 => Ok(1),
        2 => Ok(2),
        width => Err(format!("dictionary index width {width} is not one or two")),
    }
}

/// One dictionary index of `width` bytes.
pub(super) fn dictionary_index(decoder: &mut Decoder<'_>, width: usize) -> Result<usize, String> {
    Ok(match width {
        1 => usize::from(decoder.u8()?),
        2 => {
            let bytes = decoder.take(2)?;
            usize::from(u16::from_le_bytes([bytes[0], bytes[1]]))
        }
        _ => decoder.u32()? as usize,
    })
}

fn decode_dictionary(
    decoder: &mut Decoder<'_>,
    logical_type: LogicalType,
    value_count: usize,
    narrow: bool,
) -> Result<Vec<Cell>, String> {
    let dictionary_count = decoder.u32()? as usize;
    let dictionary = (0..dictionary_count)
        .map(|_| decode_cell(decoder, logical_type))
        .collect::<Result<Vec<_>, _>>()?;
    let width = if narrow {
        dictionary_index_width(decoder)?
    } else {
        4
    };
    (0..value_count)
        .map(|_| {
            let index = dictionary_index(decoder, width)?;
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

/// Values one unpack group decodes. Sixty-four values of `width` bits fill
/// exactly `width` little-endian words, so every group starts on a word
/// boundary and the bit position of each of its values is a constant of the
/// width alone.
const GROUP: usize = 64;

/// Decodes `value_count` LSB-first packed values of `width` bits, handing
/// them to `sink` in order, sixty-four at a time (the last call may be
/// shorter).
///
/// Whole groups decode through a kernel specialised per width: the group's
/// words load once, and each value is two shifts, an OR and a mask at
/// offsets the compiler knows, with no branch and no bounds check in the
/// loop. The value-at-a-time reader it replaces refilled a 128-bit
/// accumulator behind a data-dependent loop and branch per value, which
/// held a full-column decode to a few values per nanosecond. The tail after
/// the last whole group goes through that reader. Same wire format, same
/// order.
fn for_each_unpacked_group<E>(
    bytes: &[u8],
    width: u32,
    value_count: usize,
    mut sink: impl FnMut(&[u64]) -> Result<(), E>,
) -> Result<(), E> {
    let whole_groups = value_count / GROUP;
    let mut buffer = [0_u64; GROUP];
    let mut wide = WideBuffer::new();
    unpack_whole_groups(
        width,
        bytes,
        whole_groups,
        &mut buffer,
        &mut wide,
        &mut sink,
    )?;
    let rest = value_count - whole_groups * GROUP;
    if rest > 0 {
        let consumed = whole_groups * GROUP * width as usize / 8;
        let mut reader = BitReader::new(&bytes[consumed..]);
        let mask = width_mask(width);
        for value in &mut buffer[..rest] {
            *value = reader.read(width, mask);
        }
        sink(&buffer[..rest])?;
    }
    Ok(())
}

/// Routes a width to its specialised kernel.
fn unpack_whole_groups<E>(
    width: u32,
    bytes: &[u8],
    groups: usize,
    buffer: &mut [u64; GROUP],
    wide: &mut WideBuffer,
    sink: &mut impl FnMut(&[u64]) -> Result<(), E>,
) -> Result<(), E> {
    if groups > 0 && pintail_simd::unpack_level(width) != pintail_simd::Level::Baseline {
        return unpack_groups_vectored(width, bytes, groups, buffer, wide, sink);
    }
    macro_rules! dispatch {
        ($($width:literal)*) => {
            match width {
                0 => {
                    *buffer = [0; GROUP];
                    for _ in 0..groups {
                        sink(buffer)?;
                    }
                    Ok(())
                }
                $($width => unpack_groups::<$width, E>(bytes, groups, buffer, sink),)*
                _ => unreachable!("bit widths are validated to at most 64"),
            }
        };
    }
    dispatch!(
        1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32
        33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61
        62 63 64
    )
}

/// Groups one call of the vector kernel decodes: enough that the call and
/// its setup vanish against the decode, few enough that the values are
/// still in the first-level cache when the sink reads them.
const VECTORED_GROUPS: usize = 8;

/// Where the vector kernel lands several groups at once. It is cleared
/// when first asked for, once a payload, and not before: a decode that
/// takes its groups one at a time - a short payload, a selection that
/// keeps a group here and a group there - never pays for clearing it.
struct WideBuffer(Option<[u64; GROUP * VECTORED_GROUPS]>);

impl WideBuffer {
    const fn new() -> Self {
        Self(None)
    }

    fn values(&mut self) -> &mut [u64; GROUP * VECTORED_GROUPS] {
        match &mut self.0 {
            Some(values) => values,
            empty => empty.insert([0; GROUP * VECTORED_GROUPS]),
        }
    }
}

/// The whole groups of a payload through the CPU's vector unpack kernel,
/// handed to `sink` a group at a time as the per-width kernels hand them.
/// `bytes` runs to the payload's end, so the kernel's loads stay inside it
/// until the last few values. A lone group decodes straight into the
/// caller's buffer.
fn unpack_groups_vectored<E>(
    width: u32,
    bytes: &[u8],
    groups: usize,
    buffer: &mut [u64; GROUP],
    wide: &mut WideBuffer,
    sink: &mut impl FnMut(&[u64]) -> Result<(), E>,
) -> Result<(), E> {
    if groups == 1 {
        pintail_simd::unpack_u64(width, bytes, 0, buffer);
        return sink(buffer);
    }
    let group_bytes = GROUP * width as usize / 8;
    let decoded = wide.values();
    let mut group = 0;
    while group < groups {
        let batch = (groups - group).min(VECTORED_GROUPS);
        let values = &mut decoded[..batch * GROUP];
        pintail_simd::unpack_u64(width, &bytes[group * group_bytes..], 0, values);
        for chunk in values.chunks_exact(GROUP) {
            sink(chunk)?;
        }
        group += batch;
    }
    Ok(())
}

/// The whole groups of a `WIDTH`-bit payload. `words` keeps one zero word
/// past the group so the high half of the last value reads a real index
/// rather than needing a branch.
#[inline]
fn unpack_groups<const WIDTH: usize, E>(
    bytes: &[u8],
    groups: usize,
    buffer: &mut [u64; GROUP],
    sink: &mut impl FnMut(&[u64]) -> Result<(), E>,
) -> Result<(), E> {
    #[allow(clippy::cast_possible_truncation)]
    let mask = width_mask(WIDTH as u32);
    if WIDTH <= 32 {
        // A value of at most 32 bits (56 would fit), starting anywhere inside
        // a byte, lies within the eight bytes from that byte: one unaligned load, a
        // shift and a mask. Groups read straight from the payload while
        // eight bytes of slack follow them; the last group or so copies
        // into a zero-padded window instead.
        let group_bytes = WIDTH * 8;
        let mut padded = [0_u8; GROUP * 8 + 8];
        for group in 0..groups {
            let start = group * group_bytes;
            let window = if let Some(window) = bytes.get(start..start + group_bytes + 8) {
                window
            } else {
                padded[..group_bytes].copy_from_slice(&bytes[start..start + group_bytes]);
                &padded[..group_bytes + 8]
            };
            // Eight values of WIDTH bits span exactly WIDTH bytes, so each
            // run of eight starts on a byte and every shift below is a
            // constant of the width.
            for (eight, values) in buffer.chunks_exact_mut(8).enumerate() {
                let first = eight * WIDTH;
                for (index, value) in values.iter_mut().enumerate() {
                    let bit = index * WIDTH;
                    let at = first + bit / 8;
                    let word = u64::from_le_bytes([
                        window[at],
                        window[at + 1],
                        window[at + 2],
                        window[at + 3],
                        window[at + 4],
                        window[at + 5],
                        window[at + 6],
                        window[at + 7],
                    ]);
                    *value = (word >> (bit % 8)) & mask;
                }
            }
            sink(buffer)?;
        }
        return Ok(());
    }
    let mut words = [0_u64; GROUP + 1];
    for chunk in bytes.chunks_exact(WIDTH * 8).take(groups) {
        for (word, eight) in words[..WIDTH].iter_mut().zip(chunk.chunks_exact(8)) {
            *word = u64::from_le_bytes([
                eight[0], eight[1], eight[2], eight[3], eight[4], eight[5], eight[6], eight[7],
            ]);
        }
        for (index, value) in buffer.iter_mut().enumerate() {
            let bit = index * WIDTH;
            let shift = bit % 64;
            let low = words[bit / 64] >> shift;
            // `<< 1 << (63 - shift)` is `<< (64 - shift)` without the
            // undefined full-width shift when `shift` is zero.
            let high = (words[bit / 64 + 1] << 1) << (63 - shift);
            *value = (low | high) & mask;
        }
        sink(buffer)?;
    }
    Ok(())
}

/// Whether a payload of `value_count` values of `width` bits decodes
/// through the CPU's vector unpack kernel as a whole: the kernel adds the
/// block base itself and hands back chunks ready to append, where the
/// group-at-a-time path pays a sink call for every sixty-four values.
fn decodes_whole(width: u32, value_count: usize) -> bool {
    value_count >= GROUP && pintail_simd::unpack_level(width) != pintail_simd::Level::Baseline
}

/// Values one whole-payload kernel call decodes: a multiple of a group, so
/// each call starts on a byte, and small enough to stay in the first-level
/// cache until it is appended.
const WHOLE_CHUNK: usize = GROUP * VECTORED_GROUPS;

/// Appends every value of a payload through the vector kernel, a chunk at
/// a time. `CHUNK` is the values a kernel call lands before they are
/// appended.
fn unpack_whole_chunked<T: Copy + Default, const CHUNK: usize>(
    width: u32,
    bytes: &[u8],
    value_count: usize,
    out: &mut Vec<T>,
    kernel: impl Fn(&[u8], &mut [T]),
) {
    let mut chunk = [T::default(); CHUNK];
    let mut done = 0;
    while done < value_count {
        let values = &mut chunk[..(value_count - done).min(CHUNK)];
        kernel(&bytes[done * width as usize / 8..], values);
        out.extend_from_slice(values);
        done += values.len();
    }
}

/// [`unpack_whole_chunked`] with the chunk a payload of `value_count`
/// values wants: a short payload takes a short chunk, so that clearing the
/// chunk never costs more than decoding into it.
fn unpack_whole<T: Copy + Default>(
    width: u32,
    bytes: &[u8],
    value_count: usize,
    out: &mut Vec<T>,
    kernel: impl Fn(&[u8], &mut [T]),
) {
    if value_count >= WHOLE_CHUNK {
        unpack_whole_chunked::<T, WHOLE_CHUNK>(width, bytes, value_count, out, kernel);
    } else {
        unpack_whole_chunked::<T, GROUP>(width, bytes, value_count, out, kernel);
    }
}

/// Appends every value of a payload, each plus `base` (wrapping). The
/// caller has proved no sum leaves the type.
fn unpack_whole_unsigned(
    width: u32,
    bytes: &[u8],
    value_count: usize,
    base: u64,
    out: &mut Vec<u64>,
) {
    unpack_whole(width, bytes, value_count, out, |bytes, values| {
        pintail_simd::unpack_u64(width, bytes, base, values);
    });
}

/// The signed twin of [`unpack_whole_unsigned`].
fn unpack_whole_signed(
    width: u32,
    bytes: &[u8],
    value_count: usize,
    base: i64,
    out: &mut Vec<i64>,
) {
    unpack_whole(width, bytes, value_count, out, |bytes, values| {
        pintail_simd::unpack_i64(width, bytes, base, values);
    });
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
    let in_range = base >= i128::from(i64::MIN)
        && base
            .checked_add(i128::from(mask))
            .is_some_and(|top| top <= i128::from(i64::MAX));
    if in_range && width < 64 {
        #[allow(clippy::cast_possible_truncation)]
        let base = base as i64;
        if decodes_whole(width, value_count) {
            unpack_whole_signed(width, bytes, value_count, base, out);
            return Ok(());
        }
        return for_each_unpacked_group(bytes, width, value_count, |group| {
            #[allow(clippy::cast_possible_wrap)]
            out.extend(group.iter().map(|value| base.wrapping_add(*value as i64)));
            Ok(())
        });
    }
    for_each_unpacked_group(bytes, width, value_count, |group| {
        for normalized in group {
            let value = base
                .checked_add(i128::from(*normalized))
                .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
            out.push(i64::try_from(value).map_err(|_| "bit-packed signed integer overflow")?);
        }
        Ok(())
    })
}

/// Reconstructs monotone integer values straight from packed deltas.
/// The callback sees every non-NULL value, including rows a range excludes,
/// so corrupt overflows cannot hide in an unselected prefix or suffix.
pub(super) fn unpack_delta_each(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    first: i128,
    logical_type: LogicalType,
    mut emit: impl FnMut(i128) -> Result<(), String>,
) -> Result<(), String> {
    if value_count == 0 {
        return Err("delta block cannot be empty".to_owned());
    }
    let (width, bytes) = unpack_header(decoder, value_count - 1)?;
    let mask = width_mask(width);
    let maximum = match logical_type {
        LogicalType::Int64 => i128::from(i64::MAX),
        LogicalType::UInt64 => i128::from(u64::MAX),
        _ => return Err("delta destination must be integer".to_owned()),
    };
    let mut current = first;
    emit(current)?;
    // Every delta is nonnegative and at most mask. If even that worst
    // cumulative endpoint fits, all intermediate additions fit as well.
    let bounded = i128::try_from(value_count - 1)
        .ok()
        .and_then(|count| i128::from(mask).checked_mul(count))
        .and_then(|span| first.checked_add(span))
        .is_some_and(|last| last <= maximum);
    if bounded {
        return for_each_unpacked_group(bytes, width, value_count - 1, |group| {
            for delta in group {
                current += i128::from(*delta);
                emit(current)?;
            }
            Ok(())
        });
    }
    for_each_unpacked_group(bytes, width, value_count - 1, |group| {
        for delta in group {
            current = current
                .checked_add(i128::from(*delta))
                .filter(|value| *value <= maximum)
                .ok_or_else(|| "integer delta overflow".to_owned())?;
            emit(current)?;
        }
        Ok(())
    })
}

/// [`unpack_delta_each`] straight into a typed vector. When the block's
/// width proves no running value can leave the destination type, the
/// prefix sum runs in that type, one add a value; the per-value callback,
/// widened to `i128` and range-checked, cost a key-range filter over a
/// monotone column about a fifth of its time. Otherwise every value is
/// checked exactly as the callback path checks it, with the same errors.
pub(super) fn unpack_delta_into<T: Copy>(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    first: i128,
    logical_type: LogicalType,
    out: &mut Vec<T>,
    lift: impl Fn(i128) -> Result<T, String>,
    step: impl Fn(T, u64) -> T,
) -> Result<(), String> {
    if value_count == 0 {
        return Err("delta block cannot be empty".to_owned());
    }
    let (width, bytes) = unpack_header(decoder, value_count - 1)?;
    let mask = width_mask(width);
    let maximum = match logical_type {
        LogicalType::Int64 => i128::from(i64::MAX),
        LogicalType::UInt64 => i128::from(u64::MAX),
        _ => return Err("delta destination must be integer".to_owned()),
    };
    out.reserve(value_count);
    let lifted = lift(first)?;
    out.push(lifted);
    let bounded = i128::try_from(value_count - 1)
        .ok()
        .and_then(|count| i128::from(mask).checked_mul(count))
        .and_then(|span| first.checked_add(span))
        .is_some_and(|last| last <= maximum);
    if bounded {
        let mut current = lifted;
        return for_each_unpacked_group(bytes, width, value_count - 1, |group| {
            out.extend(group.iter().map(|delta| {
                current = step(current, *delta);
                current
            }));
            Ok(())
        });
    }
    let mut current = first;
    for_each_unpacked_group(bytes, width, value_count - 1, |group| {
        for delta in group {
            current = current
                .checked_add(i128::from(*delta))
                .filter(|value| *value <= maximum)
                .ok_or_else(|| "integer delta overflow".to_owned())?;
            out.push(lift(current)?);
        }
        Ok(())
    })
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
    let in_range = base >= 0
        && base
            .checked_add(i128::from(mask))
            .is_some_and(|top| top <= i128::from(u64::MAX));
    if in_range {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let base = base as u64;
        if decodes_whole(width, value_count) {
            unpack_whole_unsigned(width, bytes, value_count, base, out);
            return Ok(());
        }
        return for_each_unpacked_group(bytes, width, value_count, |group| {
            out.extend(group.iter().map(|value| base.wrapping_add(*value)));
            Ok(())
        });
    }
    for_each_unpacked_group(bytes, width, value_count, |group| {
        for normalized in group {
            let value = base
                .checked_add(i128::from(*normalized))
                .ok_or_else(|| "bit-packed integer overflow".to_owned())?;
            out.push(u64::try_from(value).map_err(|_| "bit-packed unsigned integer overflow")?);
        }
        Ok(())
    })
}

/// [`unpack`] appending to a caller's buffer, so a scan reuses one.
pub(super) fn unpack_into(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    out: &mut Vec<u64>,
) -> Result<(), String> {
    let (width, bytes) = unpack_header(decoder, value_count)?;
    out.reserve(value_count);
    if decodes_whole(width, value_count) {
        unpack_whole_unsigned(width, bytes, value_count, 0, out);
        return Ok(());
    }
    for_each_unpacked_group(bytes, width, value_count, |group| {
        out.extend_from_slice(group);
        Ok::<(), String>(())
    })
}

/// The bit width of the packed payload `decoder` stands at, read from a
/// copy so the payload is still there to decode.
pub(super) fn packed_width(decoder: &Decoder<'_>) -> Result<u32, String> {
    let width = decoder.clone().u8()?;
    if width > 64 {
        return Err(format!("invalid bit width {width}"));
    }
    Ok(u32::from(width))
}

/// The largest normalized value a payload of `width` bits can hold.
pub(super) const fn packed_ceiling(width: u32) -> u64 {
    width_mask(width)
}

/// Decodes the 64-value groups of a packed payload of `value_count` values
/// whose word in `words` (bit `i` of word `g` standing for value
/// `64 * g + i`) has any bit set, handing `sink` each such group's values
/// with its word. Groups no bit selects are skipped undecoded; the whole
/// payload is still consumed and its length checked.
///
/// Groups start on byte boundaries (64 values of `width` bits are `width`
/// words), so a group decodes straight from its offset.
pub(super) fn for_each_selected_group(
    decoder: &mut Decoder<'_>,
    value_count: usize,
    words: &[u64],
    mut sink: impl FnMut(&[u64], u64),
) -> Result<(), String> {
    let (width, bytes) = unpack_header(decoder, value_count)?;
    if words.len() < value_count.div_ceil(GROUP) {
        return Err("selection words are shorter than the payload".to_owned());
    }
    let whole_groups = value_count / GROUP;
    let group_bytes = GROUP * width as usize / 8;
    let mut buffer = [0_u64; GROUP];
    let mut wide = WideBuffer::new();
    let mut group = 0;
    while group < whole_groups {
        if words[group] == 0 {
            group += 1;
            continue;
        }
        // A run of selected groups decodes in one call.
        let first = group;
        while group < whole_groups && words[group] != 0 {
            group += 1;
        }
        let mut at = first;
        unpack_whole_groups(
            width,
            &bytes[first * group_bytes..],
            group - first,
            &mut buffer,
            &mut wide,
            &mut |values: &[u64]| {
                sink(values, words[at]);
                at += 1;
                Ok::<(), String>(())
            },
        )?;
    }
    let rest = value_count - whole_groups * GROUP;
    if rest > 0 && words[whole_groups] != 0 {
        let mut reader = BitReader::new(&bytes[whole_groups * group_bytes..]);
        let mask = width_mask(width);
        for value in &mut buffer[..rest] {
            *value = reader.read(width, mask);
        }
        sink(&buffer[..rest], words[whole_groups]);
    }
    Ok(())
}

pub(super) fn unpack(decoder: &mut Decoder<'_>, value_count: usize) -> Result<Vec<u64>, String> {
    let (width, bytes) = unpack_header(decoder, value_count)?;
    let mut values = Vec::with_capacity(value_count);
    if decodes_whole(width, value_count) {
        unpack_whole_unsigned(width, bytes, value_count, 0, &mut values);
        return Ok(values);
    }
    for_each_unpacked_group(bytes, width, value_count, |group| {
        values.extend_from_slice(group);
        Ok::<(), String>(())
    })?;
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

    /// The original value-at-a-time windowed decode, kept as the reference
    /// every faster path is checked against.
    fn unpack_windowed(decoder: &mut Decoder<'_>, value_count: usize) -> Vec<u64> {
        let (width, bytes) = unpack_header(decoder, value_count).expect("header");
        let width = width as usize;
        let mask = width_mask(u32::try_from(width).expect("width"));
        (0..value_count)
            .map(|value_index| {
                if width == 0 {
                    return 0;
                }
                let bit = value_index * width;
                let byte = bit / 8;
                let mut window = [0_u8; 16];
                let available = (bytes.len() - byte).min(16);
                window[..available].copy_from_slice(&bytes[byte..byte + available]);
                #[allow(clippy::cast_possible_truncation)]
                let value = (u128::from_le_bytes(window) >> (bit % 8)) as u64 & mask;
                value
            })
            .collect()
    }

    #[test]
    fn every_width_and_tail_length_matches_the_windowed_reference() {
        for width in 0_u8..=64 {
            for count in [0_usize, 1, 63, 64, 65, 127, 128, 129, 1_000, 16_384] {
                let framed = payload(width, count, u64::from(width) * 977 + count as u64);
                let expected = unpack_windowed(&mut Decoder::new(&framed), count);
                let grouped = unpack(&mut Decoder::new(&framed), count).expect("grouped unpack");
                assert_eq!(expected, grouped, "width {width} count {count}");
                let mut unsigned = Vec::new();
                unpack_unsigned_into(&mut Decoder::new(&framed), count, 0, &mut unsigned)
                    .expect("unsigned unpack");
                assert_eq!(expected, unsigned, "width {width} count {count}");
            }
        }
    }

    #[test]
    fn selected_groups_hand_over_exactly_their_values() {
        for width in [0_u8, 1, 7, 11, 21, 32, 33, 63, 64] {
            for count in [0_usize, 1, 63, 64, 65, 129, 1_000] {
                let framed = payload(width, count, u64::from(width) * 31 + count as u64);
                let expected = unpack_windowed(&mut Decoder::new(&framed), count);
                let groups = count.div_ceil(64);
                for pattern in 0..4_u64 {
                    let words: Vec<u64> = (0..groups as u64)
                        .map(|group| match pattern {
                            0 => u64::MAX,
                            1 => 0,
                            2 => 0x8000_0000_0000_0001 * (group % 2),
                            _ => 0x0101_0101_0101_0101 << (group % 8),
                        })
                        .collect();
                    let mut picked = Vec::new();
                    let mut decoder = Decoder::new(&framed);
                    for_each_selected_group(&mut decoder, count, &words, |values, word| {
                        for (index, value) in values.iter().enumerate() {
                            if word >> index & 1 == 1 {
                                picked.push(*value);
                            }
                        }
                    })
                    .expect("selected unpack");
                    decoder.finish().expect("payload consumed");
                    let want: Vec<u64> = (0..count)
                        .filter(|row| words[row / 64] >> (row % 64) & 1 == 1)
                        .map(|row| expected[row])
                        .collect();
                    assert_eq!(
                        picked, want,
                        "width {width} count {count} pattern {pattern}"
                    );
                }
            }
        }
    }

    /// Kernel timing: the value-at-a-time reader against the grouped
    /// kernel over 16K-value blocks (a segment block) at the widths a
    /// fixed-point amount, a date, a key and a code column pack to.
    /// `cargo test --release -p pintail-store unpack_kernel_timings -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement: run explicitly with --ignored --nocapture"]
    fn unpack_kernel_timings() {
        const BLOCK: usize = 16_384;
        const ROUNDS: usize = 400;
        for width in [3_u8, 11, 17, 21, 25, 32, 47, 57] {
            let framed = payload(width, BLOCK, u64::from(width));
            let mut out = Vec::with_capacity(BLOCK);
            let clock = std::time::Instant::now();
            for _ in 0..ROUNDS {
                out.clear();
                let mut decoder = Decoder::new(&framed);
                let (width, bytes) = unpack_header(&mut decoder, BLOCK).expect("header");
                let mask = width_mask(width);
                let mut reader = BitReader::new(bytes);
                for _ in 0..BLOCK {
                    out.push(7_u64.wrapping_add(reader.read(width, mask)));
                }
                std::hint::black_box(&out);
            }
            let reader_ns = clock.elapsed().as_secs_f64() * 1e9
                / f64::from(u32::try_from(ROUNDS * BLOCK).expect("fits"));
            let clock = std::time::Instant::now();
            for _ in 0..ROUNDS {
                out.clear();
                unpack_unsigned_into(&mut Decoder::new(&framed), BLOCK, 7, &mut out)
                    .expect("grouped");
                std::hint::black_box(&out);
            }
            let grouped_ns = clock.elapsed().as_secs_f64() * 1e9
                / f64::from(u32::try_from(ROUNDS * BLOCK).expect("fits"));
            eprintln!(
                "width {width:2}: reader {reader_ns:.3} ns/value, grouped {grouped_ns:.3} ns/value ({:.1}x)",
                reader_ns / grouped_ns
            );
        }
    }

    #[test]
    fn streaming_reader_matches_the_windowed_unpack() {
        for &width in &[0_u8, 1, 3, 7, 8, 13, 24, 31, 32, 33, 63, 64] {
            for &count in &[0_usize, 1, 2, 63, 64, 65, 2_290] {
                let framed = payload(width, count, u64::from(width) * 31 + count as u64);
                let expected = unpack_windowed(&mut Decoder::new(&framed), count);
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
            let normalized = unpack_windowed(&mut Decoder::new(&framed), count);
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

    /// Every width against the two-pass arithmetic, at lengths either side
    /// of a group and of a whole-payload chunk, with bases that keep every
    /// value in range (the unchecked decode) and bases that do not (the
    /// checked one, which must fail with the same message).
    #[test]
    fn based_decodes_match_the_two_pass_arithmetic_at_every_width() {
        let bases = [
            0_i128,
            1,
            -1,
            1_000_003,
            -1_000_003,
            i128::from(i64::MIN),
            i128::from(i64::MAX) - 5,
            i128::from(u64::MAX) - 5,
        ];
        for width in 0_u8..=64 {
            for count in [1_usize, 63, 64, 65, 200, 2_047, 2_048, 2_049, 4_100] {
                let framed = payload(width, count, u64::from(width) * 131 + count as u64);
                let normalized = unpack_windowed(&mut Decoder::new(&framed), count);
                for base in bases {
                    let sums = || {
                        normalized.iter().map(move |value| {
                            base.checked_add(i128::from(*value))
                                .ok_or_else(|| "bit-packed integer overflow".to_owned())
                        })
                    };
                    let signed: Result<Vec<i64>, String> = sums()
                        .map(|sum| {
                            i64::try_from(sum?)
                                .map_err(|_| "bit-packed signed integer overflow".to_owned())
                        })
                        .collect();
                    let mut out = vec![7_i64];
                    let outcome =
                        unpack_signed_into(&mut Decoder::new(&framed), count, base, &mut out);
                    match signed {
                        Ok(values) => {
                            outcome.expect("signed decode");
                            assert_eq!(out[0], 7);
                            assert_eq!(&out[1..], values, "width {width} count {count} {base}");
                        }
                        Err(message) => assert_eq!(outcome.expect_err("overflow"), message),
                    }
                    let unsigned: Result<Vec<u64>, String> = sums()
                        .map(|sum| {
                            u64::try_from(sum?)
                                .map_err(|_| "bit-packed unsigned integer overflow".to_owned())
                        })
                        .collect();
                    let mut out = vec![7_u64];
                    let outcome =
                        unpack_unsigned_into(&mut Decoder::new(&framed), count, base, &mut out);
                    match unsigned {
                        Ok(values) => {
                            outcome.expect("unsigned decode");
                            assert_eq!(out[0], 7);
                            assert_eq!(&out[1..], values, "width {width} count {count} {base}");
                        }
                        Err(message) => assert_eq!(outcome.expect_err("overflow"), message),
                    }
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
