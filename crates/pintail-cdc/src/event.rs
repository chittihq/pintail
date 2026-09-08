use mysql_async::binlog::events::{Event, EventData};

use crate::CdcError;

/// `MySQL`'s transaction-payload event type.
pub const TRANSACTION_PAYLOAD_EVENT: u8 = 0x28;

/// The compression types the decoder accepts; anything else reaches an
/// `unwrap()` on a `Result` inside it.
const ZSTD: u64 = 0;
const NO_COMPRESSION: u64 = 255;

pub(crate) fn decode_event(event: &Event) -> Result<Option<EventData<'_>>, CdcError> {
    let position = u64::from(event.header().log_pos());
    if event.header().event_type_raw() == TRANSACTION_PAYLOAD_EVENT {
        check_transaction_payload_header(event.data(), position)?;
    }
    event
        .read_data()
        .map_err(|error| CdcError::Decode(format!("binlog event at position {position}: {error}")))
}

/// Walks a transaction-payload header before the decoder does, so a
/// corrupted one becomes an error rather than a panic.
///
/// Two narrowing conversions inside the library's own parser end in
/// `unwrap()`: the header's field id, and the compression type. A field id
/// above 255, or a compression type that is neither ZSTD nor none, aborts
/// the replication task instead of returning a decode error - which the
/// quarantine and dead-letter paths already know how to handle. This reads
/// the same structure the parser reads and refuses first.
///
/// A header this cannot follow - truncated, or carrying a field it does not
/// know - is left alone. The decoder reports those as errors on its own;
/// only the two panicking values are this function's business.
///
/// # Errors
///
/// Returns [`CdcError::Decode`] when the header carries a field id above
/// 255, or a compression type the decoder would not accept.
pub fn check_transaction_payload_header(data: &[u8], position: u64) -> Result<(), CdcError> {
    let refuse = |what: &str, value: u64| {
        CdcError::Decode(format!(
            "binlog event at position {position}: transaction payload header {what} {value} is \
             outside the range the decoder can represent"
        ))
    };
    let mut cursor = data;
    loop {
        let Some(field) = read_length_encoded(&mut cursor) else {
            return Ok(());
        };
        if u8::try_from(field).is_err() {
            return Err(refuse("field id", field));
        }
        match field {
            // Payload size and uncompressed size: a length then a value,
            // neither of which is narrowed.
            1 | 3 => {
                if read_length_encoded(&mut cursor).is_none()
                    || read_length_encoded(&mut cursor).is_none()
                {
                    return Ok(());
                }
            }
            // Compression type: the value is turned into an enum and
            // unwrapped, so anything but the two it knows panics.
            2 => {
                if read_length_encoded(&mut cursor).is_none() {
                    return Ok(());
                }
                let Some(algorithm) = read_length_encoded(&mut cursor) else {
                    return Ok(());
                };
                if algorithm != ZSTD && algorithm != NO_COMPRESSION {
                    return Err(refuse("compression type", algorithm));
                }
            }
            // Zero ends the header and the payload after it is the
            // decoder's. Anything else in range is a field the decoder does
            // not know, and it returns a clean error for that on its own.
            // Neither is this function's business.
            _ => return Ok(()),
        }
    }
}

/// One `MySQL` length-encoded integer, advancing the cursor. `None` when
/// the bytes do not hold one, which leaves the decoder to report why.
fn read_length_encoded(cursor: &mut &[u8]) -> Option<u64> {
    let (&first, rest) = cursor.split_first()?;
    let (value, rest) = match first {
        // 0xfb is the NULL marker and does not appear in this header.
        0xfb => return None,
        0xfc => (
            u64::from(u16::from_le_bytes(rest.get(..2)?.try_into().ok()?)),
            &rest[2..],
        ),
        0xfd => {
            let bytes = rest.get(..3)?;
            (
                u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])),
                &rest[3..],
            )
        }
        0xfe => (
            u64::from_le_bytes(rest.get(..8)?.try_into().ok()?),
            &rest[8..],
        ),
        other => (u64::from(other), rest),
    };
    *cursor = rest;
    Some(value)
}

pub(crate) fn stream_error(error: mysql_async::Error) -> Result<mysql_async::Error, CdcError> {
    if matches!(&error, mysql_async::Error::Io(mysql_async::IoError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData)
    {
        return Err(CdcError::Decode(error.to_string()));
    }
    Ok(error)
}

#[cfg(test)]
mod tests {
    use mysql_async::binlog::{
        BinlogChecksumAlg, BinlogVersion,
        events::{BinlogEventFooter, Event, FormatDescriptionEvent, TransactionPayloadEvent},
    };

    use super::{decode_event, stream_error};
    use crate::CdcError;

    #[test]
    fn transaction_payload_field_above_u8_returns_positioned_decode_error() {
        let input = include_bytes!("../../../fuzz/corpus/binlog/transaction_payload_field");
        let format = FormatDescriptionEvent::new(BinlogVersion::Version4).with_footer(
            BinlogEventFooter::new(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_OFF),
        );
        let event = Event::read(&format, input.as_slice()).expect("framed event");
        // `mysql_async` decodes a transaction payload inside its own
        // stream, before the event reaches CDC, so this is the call that
        // has to return rather than abort. The published crate panics
        // here; the pinned fork does not.
        let error = event
            .read_event::<TransactionPayloadEvent<'_>>()
            .expect_err("stream payload decode must not panic");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let CdcError::Decode(message) =
            stream_error(error.into()).expect_err("stream decode must fail")
        else {
            panic!("expected a stream decode error");
        };
        // The artifact's first out-of-range field id.
        assert!(message.contains("256"), "stream message: {message}");
        assert!(message.contains("exceeds 255"), "stream message: {message}");
        // And Pintail's own guard, which reads the same header before
        // handing the event on. It cannot prevent the stream's decode, but
        // it names the event's position, which the library's error does
        // not.
        let CdcError::Decode(message) = decode_event(&event).expect_err("CDC decode must fail")
        else {
            panic!("expected a decode error");
        };
        assert!(message.contains("256"), "decode message: {message}");
        assert!(message.contains("position"), "decode message: {message}");
    }
}
