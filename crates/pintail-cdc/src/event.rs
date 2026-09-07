use mysql_async::binlog::events::{Event, EventData};

use crate::CdcError;

pub(crate) fn decode_event(event: &Event) -> Result<Option<EventData<'_>>, CdcError> {
    event.read_data().map_err(|error| {
        CdcError::Decode(format!(
            "binlog event at position {}: {error}",
            event.header().log_pos()
        ))
    })
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
        // The stream reads the payload before returning the event to CDC.
        let error = event
            .read_event::<TransactionPayloadEvent<'_>>()
            .expect_err("stream payload decode must not panic");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let CdcError::Decode(message) =
            stream_error(error.into()).expect_err("stream decode must fail")
        else {
            panic!("expected a stream decode error");
        };
        assert!(message.contains("1234"));
        assert!(message.contains("field ID exceeds 255"));
        let CdcError::Decode(message) = decode_event(&event).expect_err("CDC decode must fail")
        else {
            panic!("expected a decode error");
        };
        assert!(message.contains("1234"));
        assert!(message.contains("field ID exceeds 255"));
    }
}
