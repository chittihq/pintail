//! Result-set packets.
//!
//! Two details here diverge between clients and are invisible until one
//! breaks. `CLIENT_DEPRECATE_EOF` replaces the EOF packet that terminates a
//! result set with an OK packet, so a server that always sends EOF desyncs
//! any client that negotiated it. And the NULL bitmap in a binary result row
//! is offset by two reserved bits, while the one in `COM_STMT_EXECUTE`
//! parameters is not — using one offset for both shifts every NULL by two
//! columns.

use crate::handshake::CapabilityFlags;
use crate::packet::{put_length_encoded_bytes, put_length_encoded_integer};
use crate::types::{Column, ErrorKind, StatusFlags};

/// Marker byte introducing an OK packet.
const OK_HEADER: u8 = 0x00;
/// Marker byte introducing an EOF packet.
const EOF_HEADER: u8 = 0xfe;
/// Marker byte introducing an error packet.
const ERR_HEADER: u8 = 0xff;
/// Length-encoded NULL in a text row.
const TEXT_NULL: u8 = 0xfb;

/// Summary a statement returns when it produces no rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OkPacket {
    /// Rows the statement changed. Always zero for a read-only replica.
    pub affected_rows: u64,
    /// Last generated key, likewise always zero here.
    pub last_insert_id: u64,
    /// Server status bits.
    pub status: StatusFlags,
    /// Warning count clients surface through `SHOW WARNINGS`.
    pub warnings: u16,
}

/// Encodes an OK packet.
#[must_use]
pub fn encode_ok(packet: OkPacket, info: &str) -> Vec<u8> {
    let mut payload = vec![OK_HEADER];
    put_length_encoded_integer(&mut payload, packet.affected_rows);
    put_length_encoded_integer(&mut payload, packet.last_insert_id);
    payload.extend_from_slice(&packet.status.bits().to_le_bytes());
    payload.extend_from_slice(&packet.warnings.to_le_bytes());
    payload.extend_from_slice(info.as_bytes());
    payload
}

/// Encodes an error packet.
///
/// The `#` before the SQLSTATE is required by the 4.1 protocol; without it
/// clients read the state as part of the message and report a garbled error.
#[must_use]
pub fn encode_error(kind: ErrorKind, message: &str) -> Vec<u8> {
    let mut payload = vec![ERR_HEADER];
    payload.extend_from_slice(&kind.code().to_le_bytes());
    payload.push(b'#');
    payload.extend_from_slice(kind.sql_state());
    payload.extend_from_slice(message.as_bytes());
    payload
}

/// Encodes whichever packet terminates a result set for this client.
///
/// With `CLIENT_DEPRECATE_EOF` the terminator is an OK packet wearing the EOF
/// marker byte; without it, a three-byte EOF. Sending the wrong one leaves
/// the client waiting for rows that already ended.
#[must_use]
pub fn encode_eof(capabilities: CapabilityFlags, status: StatusFlags, warnings: u16) -> Vec<u8> {
    if capabilities.contains(CapabilityFlags::CLIENT_DEPRECATE_EOF) {
        let mut payload = vec![EOF_HEADER];
        put_length_encoded_integer(&mut payload, 0);
        put_length_encoded_integer(&mut payload, 0);
        payload.extend_from_slice(&status.bits().to_le_bytes());
        payload.extend_from_slice(&warnings.to_le_bytes());
        return payload;
    }
    let mut payload = vec![EOF_HEADER];
    payload.extend_from_slice(&warnings.to_le_bytes());
    payload.extend_from_slice(&status.bits().to_le_bytes());
    payload
}

/// Encodes one `ColumnDefinition41`.
///
/// `column_length`, `character_set` and `decimals` are written from the
/// column rather than from constants; they are the fields clients use to
/// choose a native type, and fixing them is what made the previous
/// implementation unusable for an exact replica.
#[must_use]
pub fn encode_column_definition(column: &Column) -> Vec<u8> {
    let mut payload = Vec::new();
    put_length_encoded_bytes(&mut payload, b"def");
    put_length_encoded_bytes(&mut payload, column.schema.as_bytes());
    put_length_encoded_bytes(&mut payload, column.table.as_bytes());
    put_length_encoded_bytes(&mut payload, column.org_table.as_bytes());
    put_length_encoded_bytes(&mut payload, column.column.as_bytes());
    put_length_encoded_bytes(&mut payload, column.org_column.as_bytes());
    // Fixed-length remainder: charset, length, type, flags, decimals.
    payload.push(0x0c);
    payload.extend_from_slice(&column.character_set.to_le_bytes());
    payload.extend_from_slice(&column.column_length.to_le_bytes());
    payload.push(column.coltype as u8);
    payload.extend_from_slice(&column.colflags.bits().to_le_bytes());
    payload.push(column.decimals);
    payload.extend_from_slice(&[0, 0]);
    payload
}

/// Encodes one text-protocol row. `None` is a NULL column.
#[must_use]
pub fn encode_text_row(values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for value in values {
        match value {
            None => payload.push(TEXT_NULL),
            Some(bytes) => put_length_encoded_bytes(&mut payload, bytes),
        }
    }
    payload
}

/// Bytes needed for a binary NULL bitmap covering `columns` values.
///
/// Result rows reserve two leading bits; parameter lists do not. Passing the
/// wrong `reserved_bits` shifts every NULL, so the caller states which side
/// of the protocol it is on.
#[must_use]
pub const fn null_bitmap_len(columns: usize, reserved_bits: usize) -> usize {
    (columns + reserved_bits).div_ceil(8)
}

/// Encodes one binary-protocol row.
///
/// `values` supplies each column's already-encoded binary form; `None` marks
/// a NULL, which occupies a bitmap bit and contributes no bytes.
#[must_use]
pub fn encode_binary_row(values: &[Option<Vec<u8>>]) -> Vec<u8> {
    // Binary result rows lead with 0x00 and reserve two bitmap bits.
    const RESERVED_BITS: usize = 2;
    let mut payload = vec![0_u8];
    let bitmap_start = payload.len();
    payload.resize(
        bitmap_start + null_bitmap_len(values.len(), RESERVED_BITS),
        0,
    );
    for (index, value) in values.iter().enumerate() {
        match value {
            None => {
                let bit = index + RESERVED_BITS;
                payload[bitmap_start + bit / 8] |= 1 << (bit % 8);
            }
            Some(bytes) => payload.extend_from_slice(bytes),
        }
    }
    payload
}

/// Reads the NULL flags out of a `COM_STMT_EXECUTE` parameter bitmap.
///
/// Parameters reserve no leading bits, unlike result rows.
#[must_use]
pub fn parameter_null_flags(bitmap: &[u8], parameters: usize) -> Vec<bool> {
    (0..parameters)
        .map(|index| {
            bitmap
                .get(index / 8)
                .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        OkPacket, encode_binary_row, encode_column_definition, encode_eof, encode_error, encode_ok,
        encode_text_row, null_bitmap_len, parameter_null_flags,
    };
    use crate::handshake::CapabilityFlags;
    use crate::types::{Column, ColumnFlags, ColumnType, ErrorKind, StatusFlags};

    #[test]
    fn an_error_packet_marks_its_sql_state() {
        let encoded = encode_error(ErrorKind::ErQueryInterrupted, "deadline exceeded");
        assert_eq!(encoded[0], 0xff);
        assert_eq!(&encoded[1..3], &1317_u16.to_le_bytes());
        // The '#' is mandatory; without it clients garble the message.
        assert_eq!(encoded[3], b'#');
        assert_eq!(&encoded[4..9], b"70100");
        assert!(encoded.ends_with(b"deadline exceeded"));
    }

    #[test]
    fn the_result_terminator_follows_the_negotiated_capability() {
        let status = StatusFlags::SERVER_STATUS_AUTOCOMMIT;
        let legacy = encode_eof(CapabilityFlags::empty(), status, 3);
        // Legacy EOF: marker, warnings, status — warnings first.
        let mut expected = vec![0xfe];
        expected.extend_from_slice(&3_u16.to_le_bytes());
        expected.extend_from_slice(&status.bits().to_le_bytes());
        assert_eq!(legacy, expected);

        let modern = encode_eof(CapabilityFlags::CLIENT_DEPRECATE_EOF, status, 3);
        // Deprecated-EOF form is an OK body behind the EOF marker, so it is
        // longer and orders status before warnings.
        assert_eq!(modern[0], 0xfe);
        assert!(modern.len() > legacy.len());
        assert_eq!(&modern[3..5], &status.bits().to_le_bytes());
        assert_eq!(&modern[5..7], &3_u16.to_le_bytes());
    }

    #[test]
    fn a_column_definition_carries_the_real_metadata() {
        let column = Column {
            schema: "analytics".to_owned(),
            table: "orders".to_owned(),
            org_table: "orders".to_owned(),
            column: "total".to_owned(),
            org_column: "total".to_owned(),
            column_length: 14,
            character_set: 255,
            decimals: 2,
            coltype: ColumnType::MysqlTypeNewdecimal,
            colflags: ColumnFlags::NOT_NULL_FLAG,
        };
        let encoded = encode_column_definition(&column);
        // The fixed tail is the part that used to be constant: 0x0c, then
        // charset, length, type, flags, scale.
        let tail = &encoded[encoded.len() - 13..];
        assert_eq!(tail[0], 0x0c);
        assert_eq!(&tail[1..3], &255_u16.to_le_bytes());
        assert_eq!(&tail[3..7], &14_u32.to_le_bytes());
        assert_eq!(tail[7], ColumnType::MysqlTypeNewdecimal as u8);
        assert_eq!(
            &tail[8..10],
            &ColumnFlags::NOT_NULL_FLAG.bits().to_le_bytes()
        );
        assert_eq!(tail[10], 2, "decimal scale must survive to the client");
    }

    #[test]
    fn text_rows_distinguish_null_from_empty() {
        let encoded = encode_text_row(&[Some(b"7"), None, Some(b"")]);
        assert_eq!(encoded, vec![1, b'7', 0xfb, 0]);
    }

    #[test]
    fn binary_rows_offset_their_null_bitmap_by_two_reserved_bits() {
        // Column 0 NULL must set bit 2, not bit 0. Getting this wrong shifts
        // every NULL by two columns and is invisible until a row has one.
        let encoded = encode_binary_row(&[None, Some(vec![9])]);
        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], 0b0000_0100);
        assert_eq!(&encoded[2..], &[9]);

        let second_null = encode_binary_row(&[Some(vec![9]), None]);
        assert_eq!(second_null[1], 0b0000_1000);
    }

    #[test]
    fn parameter_bitmaps_have_no_reserved_offset() {
        // The same first-column NULL is bit 0 here, unlike a result row.
        assert_eq!(parameter_null_flags(&[0b0000_0001], 2), vec![true, false]);
        assert_eq!(parameter_null_flags(&[0b0000_0010], 2), vec![false, true]);
        // A truncated bitmap reports not-NULL rather than panicking.
        assert_eq!(parameter_null_flags(&[], 2), vec![false, false]);
    }

    #[test]
    fn bitmap_sizing_covers_the_byte_boundaries() {
        // Two reserved bits mean a result row spills into a second byte at
        // seven columns, not at nine.
        assert_eq!(null_bitmap_len(0, 2), 1);
        assert_eq!(null_bitmap_len(6, 2), 1);
        assert_eq!(null_bitmap_len(7, 2), 2);
        // Parameters reserve nothing, so they spill one column later.
        assert_eq!(null_bitmap_len(8, 0), 1);
        assert_eq!(null_bitmap_len(9, 0), 2);
    }

    #[test]
    fn an_ok_packet_reports_no_writes_on_a_read_only_replica() {
        let encoded = encode_ok(OkPacket::default(), "");
        assert_eq!(encoded, vec![0x00, 0, 0, 0, 0, 0, 0]);
    }
}
