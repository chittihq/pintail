//! Packet framing.
//!
//! Every `MySQL` packet is a three-byte little-endian payload length, a
//! one-byte sequence id, then the payload. A payload of exactly
//! [`MAX_PAYLOAD`] bytes means "more follows", so a body that lands on the
//! boundary must be followed by an empty packet or the peer waits forever for
//! a continuation that never comes. That rule is the whole reason splitting
//! and joining live here rather than at each call site.

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Largest payload one packet can carry. A body this size is a continuation
/// marker, never a complete message.
pub const MAX_PAYLOAD: usize = 0xff_ff_ff;

/// Reads length-prefixed packets, rejoining continuations into one payload.
pub struct PacketReader<R> {
    inner: R,
    sequence: u8,
}

impl<R: AsyncRead + Unpin> PacketReader<R> {
    /// Wraps a stream at sequence zero.
    pub const fn new(inner: R) -> Self {
        Self { inner, sequence: 0 }
    }

    /// The sequence id the next written packet must carry.
    pub const fn sequence(&self) -> u8 {
        self.sequence
    }

    /// Forces the next expected sequence id. The handshake restarts numbering
    /// at zero for every new command.
    pub const fn set_sequence(&mut self, sequence: u8) {
        self.sequence = sequence;
    }

    /// Returns the stream, so a plaintext connection can be upgraded to TLS
    /// mid-handshake without losing the sequence.
    pub fn into_inner(self) -> (R, u8) {
        (self.inner, self.sequence)
    }

    /// Reads one logical payload, rejoining continuation packets.
    ///
    /// Returns `Ok(None)` at a clean end of stream, which is how a client
    /// that closed its socket without sending `COM_QUIT` is distinguished
    /// from a truncated packet.
    ///
    /// # Errors
    /// Propagates I/O failures and reports a truncated header or body.
    pub async fn next_payload(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0_u8; 4];
            match self.inner.read_exact(&mut header).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // A clean close between packets is a disconnect, not a
                    // protocol violation; mid-payload it is corruption.
                    return if payload.is_empty() {
                        Ok(None)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "connection closed inside a split packet",
                        ))
                    };
                }
                Err(error) => return Err(error),
            }
            let length =
                usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
            self.sequence = header[3].wrapping_add(1);
            let start = payload.len();
            payload.resize(start + length, 0);
            self.inner.read_exact(&mut payload[start..]).await?;
            if length < MAX_PAYLOAD {
                return Ok(Some(payload));
            }
        }
    }
}

/// Writes length-prefixed packets, splitting oversized payloads.
pub struct PacketWriter<W> {
    inner: W,
    sequence: u8,
}

impl<W: AsyncWrite + Unpin> PacketWriter<W> {
    /// Wraps a stream at sequence zero.
    pub const fn new(inner: W) -> Self {
        Self { inner, sequence: 0 }
    }

    /// Sets the sequence id for the next packet.
    pub const fn set_sequence(&mut self, sequence: u8) {
        self.sequence = sequence;
    }

    /// Returns the stream so the connection can be upgraded to TLS.
    pub fn into_inner(self) -> (W, u8) {
        (self.inner, self.sequence)
    }

    /// Writes one payload, splitting it across packets when needed.
    ///
    /// A payload whose length is an exact multiple of [`MAX_PAYLOAD`] is
    /// followed by an empty packet, without which the peer keeps waiting for
    /// a continuation.
    ///
    /// # Errors
    /// Propagates I/O failures from the underlying stream.
    pub async fn write_payload(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let mut offset = 0;
        loop {
            let take = payload.len().saturating_sub(offset).min(MAX_PAYLOAD);
            let chunk = &payload[offset..offset + take];
            let length = u32::try_from(take).unwrap_or(0).to_le_bytes();
            self.inner
                .write_all(&[length[0], length[1], length[2], self.sequence])
                .await?;
            self.inner.write_all(chunk).await?;
            self.sequence = self.sequence.wrapping_add(1);
            offset += take;
            if take < MAX_PAYLOAD {
                return Ok(());
            }
            if offset == payload.len() {
                // Exact multiple of the maximum: terminate with an empty
                // packet so the peer stops expecting more.
                self.inner
                    .write_all(&[0, 0, 0, self.sequence])
                    .await
                    .map(|()| self.sequence = self.sequence.wrapping_add(1))?;
                return Ok(());
            }
        }
    }

    /// Flushes buffered bytes to the peer.
    ///
    /// # Errors
    /// Propagates I/O failures from the underlying stream.
    pub async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }
}

/// Reads a length-encoded integer, returning the value and bytes consumed.
///
/// `MySQL` encodes small values in one byte and escapes larger ones behind
/// `0xfc`/`0xfd`/`0xfe` prefixes. `0xfb` is NULL in a row context, reported
/// here as `None` so callers can tell it apart from the integer zero.
#[must_use]
pub fn length_encoded_integer(bytes: &[u8]) -> Option<(Option<u64>, usize)> {
    match *bytes.first()? {
        0xfb => Some((None, 1)),
        value @ 0..=0xfa => Some((Some(u64::from(value)), 1)),
        0xfc => bytes
            .get(1..3)
            .map(|raw| (Some(u64::from(u16::from_le_bytes([raw[0], raw[1]]))), 3)),
        0xfd => bytes.get(1..4).map(|raw| {
            (
                Some(u64::from(u32::from_le_bytes([raw[0], raw[1], raw[2], 0]))),
                4,
            )
        }),
        _ => bytes.get(1..9).map(|raw| {
            let mut value = [0_u8; 8];
            value.copy_from_slice(raw);
            (Some(u64::from_le_bytes(value)), 9)
        }),
    }
}

/// Appends a length-encoded integer.
pub fn put_length_encoded_integer(output: &mut Vec<u8>, value: u64) {
    match value {
        0..=0xfa => output.push(u8::try_from(value).unwrap_or(0)),
        0xfb..=0xffff => {
            output.push(0xfc);
            output.extend_from_slice(&u16::try_from(value).unwrap_or(0).to_le_bytes());
        }
        0x1_0000..=0xff_ffff => {
            output.push(0xfd);
            output.extend_from_slice(&u32::try_from(value).unwrap_or(0).to_le_bytes()[..3]);
        }
        _ => {
            output.push(0xfe);
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// Appends a length-encoded byte string.
pub fn put_length_encoded_bytes(output: &mut Vec<u8>, value: &[u8]) {
    put_length_encoded_integer(output, value.len() as u64);
    output.extend_from_slice(value);
}

/// Reads a length-encoded byte string, returning it and the bytes consumed.
#[must_use]
pub fn length_encoded_bytes(bytes: &[u8]) -> Option<(Option<&[u8]>, usize)> {
    let (length, consumed) = length_encoded_integer(bytes)?;
    let Some(length) = length else {
        return Some((None, consumed));
    };
    let length = usize::try_from(length).ok()?;
    let value = bytes.get(consumed..consumed + length)?;
    Some((Some(value), consumed + length))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PAYLOAD, PacketReader, PacketWriter, length_encoded_bytes, length_encoded_integer,
        put_length_encoded_bytes, put_length_encoded_integer,
    };

    async fn round_trip(payload: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut writer = PacketWriter::new(&mut encoded);
        writer.write_payload(payload).await.expect("write");
        let mut reader = PacketReader::new(encoded.as_slice());
        reader
            .next_payload()
            .await
            .expect("read")
            .expect("one payload")
    }

    #[tokio::test]
    async fn payloads_round_trip_across_the_split_boundary() {
        // The boundary cases are the whole point: a body of exactly the
        // maximum must be followed by an empty packet, or the peer hangs.
        for length in [0, 1, 512, MAX_PAYLOAD - 1, MAX_PAYLOAD, MAX_PAYLOAD + 1] {
            let payload = vec![0x5a_u8; length];
            assert_eq!(round_trip(&payload).await.len(), length, "length {length}");
        }
    }

    #[tokio::test]
    async fn a_maximum_length_body_is_terminated_by_an_empty_packet() {
        let mut encoded = Vec::new();
        let mut writer = PacketWriter::new(&mut encoded);
        writer
            .write_payload(&vec![7_u8; MAX_PAYLOAD])
            .await
            .expect("write");
        // Header + full body, then a bare header with zero length.
        assert_eq!(encoded.len(), 4 + MAX_PAYLOAD + 4);
        assert_eq!(&encoded[encoded.len() - 4..], &[0, 0, 0, 1]);
    }

    #[tokio::test]
    async fn a_clean_close_between_packets_reports_no_payload() {
        let mut reader = PacketReader::new(&[][..]);
        assert!(reader.next_payload().await.expect("clean eof").is_none());
    }

    #[tokio::test]
    async fn a_close_inside_a_split_packet_is_an_error() {
        // A continuation-sized packet with nothing after it is corruption,
        // not a disconnect.
        let mut encoded = vec![0xff, 0xff, 0xff, 0];
        encoded.extend(std::iter::repeat_n(1_u8, MAX_PAYLOAD));
        let mut reader = PacketReader::new(encoded.as_slice());
        assert!(reader.next_payload().await.is_err());
    }

    #[test]
    fn length_encoded_integers_round_trip_at_every_width() {
        for value in [
            0,
            0xfa,
            0xfb,
            0xffff,
            0x1_0000,
            0xff_ffff,
            0x100_0000,
            u64::MAX,
        ] {
            let mut encoded = Vec::new();
            put_length_encoded_integer(&mut encoded, value);
            let (decoded, consumed) = length_encoded_integer(&encoded).expect("decode");
            assert_eq!(decoded, Some(value), "value {value}");
            assert_eq!(consumed, encoded.len(), "value {value}");
        }
    }

    #[test]
    fn a_null_marker_is_not_the_integer_zero() {
        assert_eq!(length_encoded_integer(&[0xfb]), Some((None, 1)));
        assert_eq!(length_encoded_integer(&[0x00]), Some((Some(0), 1)));
    }

    #[test]
    fn length_encoded_bytes_round_trip() {
        let mut encoded = Vec::new();
        put_length_encoded_bytes(&mut encoded, b"pintail");
        assert_eq!(
            length_encoded_bytes(&encoded),
            Some((Some(&b"pintail"[..]), encoded.len()))
        );
    }
}
