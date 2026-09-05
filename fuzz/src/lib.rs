//! Shared entry points for deterministic smoke tests and coverage-guided fuzzing.
use pintail_protocol::{Command, HandshakeResponse, PacketReader};

pub fn wire(data: &[u8]) {
    let _ = HandshakeResponse::parse(data);
    let _ = HandshakeResponse::is_ssl_request(data);
    let _ = Command::parse(data);
    let _ = pintail_protocol::value::parse_parameter_types(data, data.len().min(64) / 2);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut reader = PacketReader::new(data);
        // A finite slice returns EOF instead of waiting on an external socket.
        let _ = reader.next_payload().await;
    });
}

pub fn binlog(data: &[u8]) {
    use mysql_async::binlog::{
        BinlogChecksumAlg, BinlogVersion,
        events::{BinlogEventFooter, Event, FormatDescriptionEvent},
    };
    if data.len() < 19 || data.len() > 65_536 {
        return;
    }
    // Event::read expects a complete framed event; the transport owns that
    // invariant. Normalize only its length, retaining arbitrary type/body bytes.
    let mut framed = data.to_vec();
    let length = u32::try_from(framed.len()).expect("bounded event");
    framed[9..13].copy_from_slice(&length.to_le_bytes());
    let format = FormatDescriptionEvent::new(BinlogVersion::Version4).with_footer(
        BinlogEventFooter::new(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_OFF),
    );
    if let Ok(event) = Event::read(&format, framed.as_slice()) {
        let _ = event.read_data();
    }
}

pub fn storage(data: &[u8]) {
    pintail_store::fuzzing::decode_record(data);
    let file = tempfile::NamedTempFile::new().expect("temporary WAL");
    std::fs::write(file.path(), data).expect("write temporary WAL");
    pintail_store::fuzzing::read_wal(file.path());
}

#[cfg(test)]
mod tests {
    use super::{binlog, storage, wire};

    #[test]
    #[ignore = "known mysql_common 0.37.3 panic; see fuzz/README.md"]
    fn transaction_payload_field_above_u8_is_rejected_without_panicking() {
        let mut event = vec![0_u8; 19];
        event[4] = 40;
        event.extend_from_slice(&[0xfc, 0, 1]); // length-encoded field ID 256
        binlog(&event);
    }

    #[test]
    fn binlog_query_seed_and_its_truncated_prefixes() {
        let input = include_bytes!("../corpus/binlog/query");
        for length in 0..=input.len() {
            binlog(&input[..length]);
        }
    }

    #[test]
    fn deterministic_wire_and_storage_byte_corpus() {
        for input in [
            include_bytes!("../corpus/wire/handshake").as_slice(),
            include_bytes!("../corpus/binlog/query").as_slice(),
            include_bytes!("../corpus/storage/row").as_slice(),
            &[255; 32],
        ] {
            for length in 0..=input.len() {
                wire(&input[..length]);

                storage(&input[..length]);
            }
        }
        let mut state = 953_u64;
        for length in 0..256 {
            let input: Vec<_> = (0..length)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    (state >> 32) as u8
                })
                .collect();
            wire(&input);
            storage(&input);
        }
    }
}
