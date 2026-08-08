//! Binary parameter decoding for `COM_STMT_EXECUTE`.
//!
//! Temporal values are the trap here: their length byte selects the shape, so
//! the same `DATETIME` arrives as 0, 4, 7 or 11 bytes depending on whether it
//! carries a time and fractional seconds. Reading a fixed width silently
//! misparses every value after the first short one. `TIME` adds a sign byte
//! and a day count that `DATETIME` does not have, so the two cannot share a
//! decoder.

use crate::packet::length_encoded_bytes;
use crate::resultset::{null_bitmap_len, parameter_null_flags};
use crate::types::ColumnType;

/// A parameter's declared type and signedness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterType {
    /// Protocol type tag.
    pub column_type: u8,
    /// Whether the integer is unsigned. Without this a client's `u64` above
    /// `i64::MAX` decodes as negative.
    pub unsigned: bool,
}

/// One decoded parameter, still in protocol terms.
///
/// Deliberately not mapped onto an engine type: this crate stays a protocol
/// crate, so the caller owns the conversion and its rounding rules.
#[derive(Clone, Debug, PartialEq)]
pub enum BinaryValue {
    /// SQL NULL.
    Null,
    /// Signed integer of any width.
    Int(i64),
    /// Unsigned integer of any width.
    UInt(u64),
    /// 32-bit float.
    Float(f32),
    /// 64-bit float.
    Double(f64),
    /// Length-encoded bytes: strings, blobs, DECIMAL and JSON all arrive
    /// this way and keep their exact text.
    Bytes(Vec<u8>),
    /// `DATE`, `DATETIME` or `TIMESTAMP`, rendered in `MySQL`'s text form so
    /// no precision is lost on the way to the engine.
    DateTime(String),
    /// `TIME`, which spans -838:59:59..=838:59:59 and so cannot be a clock.
    Time(String),
}

/// Reads the parameter type array a client sends when it rebinds.
///
/// Returns the types and the bytes consumed. The high bit of each entry's
/// second byte is the unsigned flag.
#[must_use]
pub fn parse_parameter_types(body: &[u8], count: usize) -> Option<(Vec<ParameterType>, usize)> {
    let needed = count.checked_mul(2)?;
    let raw = body.get(..needed)?;
    let types = raw
        .chunks_exact(2)
        .map(|entry| ParameterType {
            column_type: entry[0],
            unsigned: entry[1] & 0x80 != 0,
        })
        .collect();
    Some((types, needed))
}

/// Decodes one binary parameter, returning it and the bytes consumed.
///
/// Returns `None` when the body is too short for the declared type, which is
/// how a truncated or malformed execute packet is rejected rather than
/// producing a plausible wrong value.
#[must_use]
#[allow(clippy::too_many_lines)] // one arm per protocol type reads best unsplit
pub fn decode_binary_value(parameter: ParameterType, body: &[u8]) -> Option<(BinaryValue, usize)> {
    let fixed = |width: usize| body.get(..width);
    match parameter.column_type {
        tag if tag == ColumnType::MysqlTypeNull as u8 => Some((BinaryValue::Null, 0)),
        tag if tag == ColumnType::MysqlTypeTiny as u8 => {
            let raw = fixed(1)?;
            Some((
                if parameter.unsigned {
                    BinaryValue::UInt(u64::from(raw[0]))
                } else {
                    BinaryValue::Int(i64::from(i8::from_le_bytes([raw[0]])))
                },
                1,
            ))
        }
        tag if tag == ColumnType::MysqlTypeShort as u8
            || tag == ColumnType::MysqlTypeYear as u8 =>
        {
            let raw = fixed(2)?;
            let value = u16::from_le_bytes([raw[0], raw[1]]);
            Some((
                if parameter.unsigned {
                    BinaryValue::UInt(u64::from(value))
                } else {
                    BinaryValue::Int(i64::from(i16::from_le_bytes([raw[0], raw[1]])))
                },
                2,
            ))
        }
        tag if tag == ColumnType::MysqlTypeLong as u8 => {
            let raw = fixed(4)?;
            let value = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            Some((
                if parameter.unsigned {
                    BinaryValue::UInt(u64::from(value))
                } else {
                    BinaryValue::Int(i64::from(i32::from_le_bytes([
                        raw[0], raw[1], raw[2], raw[3],
                    ])))
                },
                4,
            ))
        }
        tag if tag == ColumnType::MysqlTypeLonglong as u8 => {
            let raw = fixed(8)?;
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(raw);
            let value = u64::from_le_bytes(bytes);
            Some((
                if parameter.unsigned {
                    BinaryValue::UInt(value)
                } else {
                    BinaryValue::Int(i64::from_le_bytes(bytes))
                },
                8,
            ))
        }
        tag if tag == ColumnType::MysqlTypeFloat as u8 => {
            let raw = fixed(4)?;
            let mut bytes = [0_u8; 4];
            bytes.copy_from_slice(raw);
            Some((BinaryValue::Float(f32::from_le_bytes(bytes)), 4))
        }
        tag if tag == ColumnType::MysqlTypeDouble as u8 => {
            let raw = fixed(8)?;
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(raw);
            Some((BinaryValue::Double(f64::from_le_bytes(bytes)), 8))
        }
        tag if tag == ColumnType::MysqlTypeDate as u8
            || tag == ColumnType::MysqlTypeDatetime as u8
            || tag == ColumnType::MysqlTypeTimestamp as u8 =>
        {
            decode_datetime(body)
        }
        tag if tag == ColumnType::MysqlTypeTime as u8 => decode_time(body),
        // Everything else — VARCHAR, BLOB, DECIMAL, JSON, BIT — arrives as
        // length-encoded bytes and keeps its exact text.
        _ => {
            let (value, consumed) = length_encoded_bytes(body)?;
            Some((
                value.map_or(BinaryValue::Null, |bytes| {
                    BinaryValue::Bytes(bytes.to_vec())
                }),
                consumed,
            ))
        }
    }
}

/// `DATE`/`DATETIME`/`TIMESTAMP`: a length byte selects the shape.
fn decode_datetime(body: &[u8]) -> Option<(BinaryValue, usize)> {
    let length = usize::from(*body.first()?);
    let raw = body.get(1..1 + length)?;
    let rendered = match length {
        // A zero length is MySQL's all-zero date, which is a value rather
        // than an absence.
        0 => "0000-00-00".to_owned(),
        4 => format!(
            "{:04}-{:02}-{:02}",
            u16::from_le_bytes([raw[0], raw[1]]),
            raw[2],
            raw[3]
        ),
        7 => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            u16::from_le_bytes([raw[0], raw[1]]),
            raw[2],
            raw[3],
            raw[4],
            raw[5],
            raw[6]
        ),
        11 => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
            u16::from_le_bytes([raw[0], raw[1]]),
            raw[2],
            raw[3],
            raw[4],
            raw[5],
            raw[6],
            u32::from_le_bytes([raw[7], raw[8], raw[9], raw[10]])
        ),
        _ => return None,
    };
    Some((BinaryValue::DateTime(rendered), 1 + length))
}

/// `TIME` carries a sign and a day count, so its hour can exceed 24 and its
/// value can be negative — neither of which a date shape can express.
fn decode_time(body: &[u8]) -> Option<(BinaryValue, usize)> {
    let length = usize::from(*body.first()?);
    let raw = body.get(1..1 + length)?;
    let rendered = match length {
        0 => "00:00:00".to_owned(),
        8 | 12 => {
            let negative = raw[0] != 0;
            let days = u32::from_le_bytes([raw[1], raw[2], raw[3], raw[4]]);
            let hours = u32::from(raw[5]) + days * 24;
            let sign = if negative { "-" } else { "" };
            if length == 12 {
                let micros = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
                format!("{sign}{hours:02}:{:02}:{:02}.{micros:06}", raw[6], raw[7])
            } else {
                format!("{sign}{hours:02}:{:02}:{:02}", raw[6], raw[7])
            }
        }
        _ => return None,
    };
    Some((BinaryValue::Time(rendered), 1 + length))
}

/// Decodes a whole `COM_STMT_EXECUTE` body after the statement handle.
///
/// Layout: flags, iteration count, NULL bitmap, a rebind flag, then the type
/// array when rebinding, then one value per non-NULL parameter. `types`
/// supplies the previously bound types for a client that is not rebinding.
///
/// # Errors
/// Returns `None` on a truncated body or an unusable temporal length.
#[must_use]
pub fn decode_execute_parameters(
    body: &[u8],
    parameters: usize,
    remembered: Option<&[ParameterType]>,
) -> Option<(Vec<BinaryValue>, Vec<ParameterType>)> {
    // flags(1) + iteration count(4)
    let mut cursor = 5;
    let bitmap_len = null_bitmap_len(parameters, 0);
    let bitmap = body.get(cursor..cursor + bitmap_len)?;
    let nulls = parameter_null_flags(bitmap, parameters);
    cursor += bitmap_len;

    let rebinding = *body.get(cursor)? == 1;
    cursor += 1;
    let types = if rebinding {
        let (types, consumed) = parse_parameter_types(body.get(cursor..)?, parameters)?;
        cursor += consumed;
        types
    } else {
        remembered?.to_vec()
    };
    if types.len() != parameters {
        return None;
    }

    let mut values = Vec::with_capacity(parameters);
    for (index, parameter) in types.iter().enumerate() {
        if nulls.get(index).copied().unwrap_or(false) {
            values.push(BinaryValue::Null);
            continue;
        }
        let (value, consumed) = decode_binary_value(*parameter, body.get(cursor..)?)?;
        cursor += consumed;
        values.push(value);
    }
    Some((values, types))
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryValue, IntWidth, ParameterType, decode_binary_value, decode_execute_parameters,
        parse_parameter_types,
    };
    use crate::types::ColumnType;

    fn parameter(column_type: ColumnType, unsigned: bool) -> ParameterType {
        ParameterType {
            column_type: column_type as u8,
            unsigned,
        }
    }

    #[test]
    fn integers_respect_their_unsigned_flag() {
        // The same eight bytes are -1 signed and u64::MAX unsigned. Ignoring
        // the flag turns a large row count negative.
        let raw = [0xff_u8; 8];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeLonglong, false), &raw),
            Some((BinaryValue::Int(-1), 8))
        );
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeLonglong, true), &raw),
            Some((BinaryValue::UInt(u64::MAX), 8))
        );
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeTiny, false), &[0xff]),
            Some((BinaryValue::Int(-1), 1))
        );
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeTiny, true), &[0xff]),
            Some((BinaryValue::UInt(255), 1))
        );
    }

    #[test]
    fn a_datetime_decodes_at_every_documented_length() {
        let date = [4, 0xe8, 0x07, 1, 15];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDate, false), &date),
            Some((BinaryValue::DateTime("2024-01-15".to_owned()), 5))
        );
        let seconds = [7, 0xe8, 0x07, 1, 15, 10, 30, 45];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &seconds),
            Some((BinaryValue::DateTime("2024-01-15 10:30:45".to_owned()), 8))
        );
        let micros = [11, 0xe8, 0x07, 1, 15, 10, 30, 45, 0xe8, 0x03, 0x00, 0x00];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &micros),
            Some((
                BinaryValue::DateTime("2024-01-15 10:30:45.001000".to_owned()),
                12
            ))
        );
        // Zero length is MySQL's all-zero date, a value rather than absence.
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &[0]),
            Some((BinaryValue::DateTime("0000-00-00".to_owned()), 1))
        );
        // An undocumented length is rejected rather than guessed.
        assert!(
            decode_binary_value(
                parameter(ColumnType::MysqlTypeDatetime, false),
                &[5, 0, 0, 0, 0, 0]
            )
            .is_none()
        );
    }

    #[test]
    fn time_carries_a_sign_and_a_day_count_a_date_cannot() {
        // 2 days + 3 hours is 51:00:00, which no clock type can hold.
        let positive = [8, 0, 2, 0, 0, 0, 3, 4, 5];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeTime, false), &positive),
            Some((BinaryValue::Time("51:04:05".to_owned()), 9))
        );
        let negative = [8, 1, 0, 0, 0, 0, 1, 2, 3];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeTime, false), &negative),
            Some((BinaryValue::Time("-01:02:03".to_owned()), 9))
        );
        let micros = [12, 0, 0, 0, 0, 0, 1, 2, 3, 0xe8, 0x03, 0x00, 0x00];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeTime, false), &micros),
            Some((BinaryValue::Time("01:02:03.001000".to_owned()), 13))
        );
    }

    #[test]
    fn decimal_and_json_keep_their_exact_text() {
        // These ride the length-encoded path precisely so a scale survives.
        let raw = [5, b'1', b'0', b'.', b'5', b'0'];
        assert_eq!(
            decode_binary_value(parameter(ColumnType::MysqlTypeNewdecimal, false), &raw),
            Some((BinaryValue::Bytes(b"10.50".to_vec()), 6))
        );
    }

    #[test]
    fn truncated_bodies_decode_to_none_rather_than_a_plausible_value() {
        for column_type in [
            ColumnType::MysqlTypeLonglong,
            ColumnType::MysqlTypeDouble,
            ColumnType::MysqlTypeLong,
        ] {
            assert!(decode_binary_value(parameter(column_type, false), &[1, 2]).is_none());
        }
        assert!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &[]).is_none()
        );
        assert!(
            decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &[7, 1]).is_none()
        );
    }

    #[test]
    fn encoded_integers_round_trip_through_the_decoder() {
        // The same bytes must be readable back at the width they were
        // written, whichever sign the caller intended — the wire carries no
        // sign tag of its own.
        for (width, tag, value) in [
            (IntWidth::Tiny, ColumnType::MysqlTypeTiny, -1_i64),
            (IntWidth::Short, ColumnType::MysqlTypeShort, -1_i64),
            (IntWidth::Long, ColumnType::MysqlTypeLong, -1_i64),
            (IntWidth::LongLong, ColumnType::MysqlTypeLonglong, i64::MIN),
        ] {
            let encoded = super::encode_binary_int(value, width);
            let (decoded, consumed) =
                decode_binary_value(parameter(tag, false), &encoded).expect("decode");
            assert_eq!(decoded, BinaryValue::Int(value));
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn encoded_datetimes_round_trip_at_every_length_the_decoder_reads() {
        type DatetimeCase = (u16, u8, u8, Option<(u8, u8, u8, Option<u32>)>, &'static str);
        let cases: &[DatetimeCase] = &[
            (0, 0, 0, None, "0000-00-00"),
            (2024, 1, 15, None, "2024-01-15"),
            (2024, 1, 15, Some((10, 30, 45, None)), "2024-01-15 10:30:45"),
            (
                2024,
                1,
                15,
                Some((10, 30, 45, Some(1_000))),
                "2024-01-15 10:30:45.001000",
            ),
        ];
        for (year, month, day, time, expected) in cases.iter().copied() {
            let encoded = super::encode_binary_datetime(year, month, day, time);
            let (decoded, consumed) =
                decode_binary_value(parameter(ColumnType::MysqlTypeDatetime, false), &encoded)
                    .expect("decode");
            assert_eq!(decoded, BinaryValue::DateTime(expected.to_owned()));
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn encoded_times_round_trip_including_the_beyond_24_hour_and_negative_cases() {
        type TimeCase = (bool, u32, u8, u8, u8, Option<u32>, &'static str);
        let cases: &[TimeCase] = &[
            (false, 0, 0, 0, 0, None, "00:00:00"),
            (false, 2, 3, 4, 5, None, "51:04:05"),
            (true, 0, 1, 2, 3, None, "-01:02:03"),
            (false, 0, 1, 2, 3, Some(1_000), "01:02:03.001000"),
        ];
        for (negative, days, hour, minute, second, micros, expected) in cases.iter().copied() {
            let encoded = super::encode_binary_time(negative, days, hour, minute, second, micros);
            let (decoded, consumed) =
                decode_binary_value(parameter(ColumnType::MysqlTypeTime, false), &encoded)
                    .expect("decode");
            assert_eq!(decoded, BinaryValue::Time(expected.to_owned()));
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn parameter_types_carry_the_unsigned_bit() {
        let (types, consumed) =
            parse_parameter_types(&[0x08, 0x80, 0x08, 0x00], 2).expect("two types");
        assert_eq!(consumed, 4);
        assert!(types[0].unsigned);
        assert!(!types[1].unsigned);
        assert!(parse_parameter_types(&[0x08], 1).is_none());
    }

    #[test]
    fn an_execute_body_binds_nulls_types_and_values_together() {
        // flags, iteration count, bitmap marking parameter 1 NULL, rebind
        // flag, two types, then one value for the non-NULL parameter.
        let body = [
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0b0000_0010,
            0x01,
            0x08,
            0x00,
            0xfe,
            0x00,
            0x07,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        let (values, types) = decode_execute_parameters(&body, 2, None).expect("decode");
        assert_eq!(values[0], BinaryValue::Int(7));
        assert_eq!(values[1], BinaryValue::Null);
        assert_eq!(types.len(), 2);

        // A client that does not rebind reuses the remembered types.
        let reused = [
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0b0000_0010,
            0x00,
            0x07,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        let (values, _) = decode_execute_parameters(&reused, 2, Some(&types)).expect("reuse");
        assert_eq!(values[0], BinaryValue::Int(7));
        assert_eq!(values[1], BinaryValue::Null);
        // Without remembered types there is nothing to decode against.
        assert!(decode_execute_parameters(&reused, 2, None).is_none());
    }
}

/// Encodes an 8/16/32/64-bit integer's raw little-endian bytes for the
/// binary result protocol. Signed and unsigned values of the same width
/// share an identical byte pattern (two's complement), so the caller need
/// only pick the width that matches the column's declared type; the
/// UNSIGNED flag on the column, not this encoding, is what tells the client
/// how to interpret the sign.
#[must_use]
pub fn encode_binary_int(value: i64, width: IntWidth) -> Vec<u8> {
    match width {
        IntWidth::Tiny => vec![value.to_le_bytes()[0]],
        IntWidth::Short => value.to_le_bytes()[..2].to_vec(),
        IntWidth::Long => value.to_le_bytes()[..4].to_vec(),
        IntWidth::LongLong => value.to_le_bytes().to_vec(),
    }
}

/// Integer column width, matching the four protocol type tags that carry a
/// fixed-width integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntWidth {
    /// `MYSQL_TYPE_TINY`.
    Tiny,
    /// `MYSQL_TYPE_SHORT` / `MYSQL_TYPE_YEAR`.
    Short,
    /// `MYSQL_TYPE_LONG`.
    Long,
    /// `MYSQL_TYPE_LONGLONG`.
    LongLong,
}

/// Encodes a `DATE`/`DATETIME`/`TIMESTAMP` result value in the binary
/// protocol's variable-length form: the shortest of 0/4/7/11 bytes that
/// carries every non-zero field, exactly mirroring the lengths
/// [`decode_datetime`] reads. A shorter encoding than the value needs would
/// silently drop the time or the fractional seconds; a longer one than
/// necessary is legal but not what real servers send.
#[must_use]
pub fn encode_binary_datetime(
    year: u16,
    month: u8,
    day: u8,
    time: Option<(u8, u8, u8, Option<u32>)>,
) -> Vec<u8> {
    let Some((hour, minute, second, micros)) = time else {
        if year == 0 && month == 0 && day == 0 {
            return vec![0];
        }
        let mut body = vec![4];
        body.extend_from_slice(&year.to_le_bytes());
        body.push(month);
        body.push(day);
        return body;
    };
    let micros = micros.filter(|value| *value != 0);
    let mut body = vec![if micros.is_some() { 11 } else { 7 }];
    body.extend_from_slice(&year.to_le_bytes());
    body.push(month);
    body.push(day);
    body.push(hour);
    body.push(minute);
    body.push(second);
    if let Some(micros) = micros {
        body.extend_from_slice(&micros.to_le_bytes());
    }
    body
}

/// Encodes a `TIME` result value in the binary protocol's variable-length
/// form, mirroring the lengths [`decode_time`] reads. `days` and `hour` are
/// carried separately on the wire — `MySQL`'s TIME spans beyond 24 hours —
/// but only their sum is meaningful, so a caller with a plain hour count
/// past 24 passes `days: 0` and the full count as `hour`.
#[must_use]
pub fn encode_binary_time(
    negative: bool,
    days: u32,
    hour: u8,
    minute: u8,
    second: u8,
    micros: Option<u32>,
) -> Vec<u8> {
    let micros = micros.filter(|value| *value != 0);
    if !negative && days == 0 && hour == 0 && minute == 0 && second == 0 && micros.is_none() {
        return vec![0];
    }
    let mut body = vec![if micros.is_some() { 12 } else { 8 }];
    body.push(u8::from(negative));
    body.extend_from_slice(&days.to_le_bytes());
    body.push(hour);
    body.push(minute);
    body.push(second);
    if let Some(micros) = micros {
        body.extend_from_slice(&micros.to_le_bytes());
    }
    body
}
