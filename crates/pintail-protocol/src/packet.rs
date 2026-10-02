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
    /// Bytes taken from the transport and not yet consumed: what one read
    /// brought beyond the bytes asked for, and bytes a caller unavoidably
    /// consumed (for example, while probing for a peer disconnect) and
    /// handed back. Drained before the underlying stream is touched again,
    /// so a byte is read exactly once no matter which caller reads it.
    buffer: Vec<u8>,
    /// Where the unconsumed bytes of `buffer` begin and end.
    start: usize,
    end: usize,
    /// Whether a read may take more from the transport than was asked for.
    read_ahead: bool,
}

/// The most one read takes from the transport when reading ahead. A command
/// is a four-byte header and its body, and nearly every command is far
/// smaller than this, so one read brings all of it; a larger body is read
/// straight into its payload. Small enough that an idle connection holding
/// it costs nothing worth counting.
const READ_AHEAD: usize = 1024;

impl<R: AsyncRead + Unpin> PacketReader<R> {
    /// Wraps a stream at sequence zero.
    pub const fn new(inner: R) -> Self {
        Self {
            inner,
            sequence: 0,
            buffer: Vec::new(),
            start: 0,
            end: 0,
            read_ahead: false,
        }
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

    /// Lets a read take whatever the transport has, up to a small bound,
    /// instead of exactly the bytes asked for.
    ///
    /// Read exactly, a command costs three reads: the byte that proves it
    /// arrived, the rest of its header, its body. Read ahead, it costs one.
    /// Off until the caller turns it on, because bytes read ahead belong
    /// to this reader: a stream handed back through [`Self::into_inner`]
    /// for a TLS upgrade must not have had the next protocol's first bytes
    /// taken from it.
    pub const fn set_read_ahead(&mut self, read_ahead: bool) {
        self.read_ahead = read_ahead;
    }

    /// Whether bytes already taken from the transport are waiting here.
    #[must_use]
    pub const fn has_buffered(&self) -> bool {
        self.start < self.end
    }

    /// Queues bytes to be read before the underlying stream is touched
    /// again. For a caller that had to perform a real read while checking
    /// whether the peer was still connected and got data rather than EOF —
    /// those bytes are still owed to the protocol and must not be dropped
    /// silently or read twice.
    pub fn prime(&mut self, bytes: &[u8]) {
        if !self.has_buffered() {
            self.start = 0;
            self.end = 0;
        }
        let end = self.end + bytes.len();
        if self.buffer.len() < end {
            self.buffer.resize(end, 0);
        }
        self.buffer[self.end..end].copy_from_slice(bytes);
        self.end = end;
    }

    /// Waits until the next packet has begun to arrive, without consuming it.
    ///
    /// What the read that proves it arrived brought is kept, so the packet
    /// still reads whole afterwards. This is what lets a caller put a
    /// deadline on an idle connection without putting one on the work a
    /// command asks for.
    ///
    /// `Ok(false)` is end of stream: the peer closed while nothing was in
    /// flight.
    ///
    /// # Errors
    /// Propagates I/O failures from the underlying stream.
    pub async fn wait_for_packet(&mut self) -> std::io::Result<bool> {
        if self.has_buffered() {
            return Ok(true);
        }
        if self.read_ahead {
            return Ok(self.fill().await? > 0);
        }
        let mut byte = [0_u8; 1];
        if self.inner.read(&mut byte).await? == 0 {
            return Ok(false);
        }
        self.prime(&byte);
        Ok(true)
    }

    /// Takes what the transport has into the buffer, up to [`READ_AHEAD`]
    /// bytes, and returns how many arrived; zero is end of stream.
    async fn fill(&mut self) -> std::io::Result<usize> {
        debug_assert!(!self.has_buffered(), "a fill replaces a drained buffer");
        if self.buffer.len() < READ_AHEAD {
            self.buffer.resize(READ_AHEAD, 0);
        }
        self.start = 0;
        self.end = 0;
        let read = self.inner.read(&mut self.buffer).await?;
        self.end = read;
        Ok(read)
    }

    /// Returns the stream, so a plaintext connection can be upgraded to TLS
    /// mid-handshake without losing the sequence.
    ///
    /// Bytes still buffered would be lost with this reader, and dropping
    /// them would answer a different command than the client actually sent;
    /// a caller upgrading a connection must not have read ahead.
    #[must_use]
    pub fn into_inner(self) -> (R, u8) {
        debug_assert!(
            !self.has_buffered(),
            "buffered bytes must be drained before an upgrade, or they are lost"
        );
        (self.inner, self.sequence)
    }

    /// Moves buffered bytes into `buf` and returns how many were moved.
    fn take_buffered(&mut self, buf: &mut [u8]) -> usize {
        let take = (self.end - self.start).min(buf.len());
        buf[..take].copy_from_slice(&self.buffer[self.start..self.start + take]);
        self.start += take;
        take
    }

    async fn read_exact_primed(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let mut filled = self.take_buffered(buf);
        while filled < buf.len() {
            if !self.read_ahead || buf.len() - filled >= READ_AHEAD {
                // Exactly these bytes, straight into their place: nothing
                // beyond them is taken, and a large body is not copied.
                self.inner.read_exact(&mut buf[filled..]).await?;
                return Ok(());
            }
            if self.fill().await? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "early eof",
                ));
            }
            filled += self.take_buffered(&mut buf[filled..]);
        }
        Ok(())
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
            match self.read_exact_primed(&mut header).await {
                Ok(()) => {}
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
            self.read_exact_primed(&mut payload[start..]).await?;
            if length < MAX_PAYLOAD {
                return Ok(Some(payload));
            }
        }
    }
}

/// Writes length-prefixed packets, splitting oversized payloads.
///
/// Packets accumulate in a buffer and reach the stream in one write when
/// the response is flushed or the buffer fills. Written straight through,
/// every packet cost two writes (header, body) and every row of a result
/// set its own segments on the wire.
pub struct PacketWriter<W> {
    inner: W,
    sequence: u8,
    buffer: Vec<u8>,
}

/// Buffered bytes beyond which a response is written out before it is
/// complete, so a large result set is not held in memory twice.
const WRITE_BUFFER_HIGH_WATER: usize = 64 * 1024;

impl<W: AsyncWrite + Unpin> PacketWriter<W> {
    /// Wraps a stream at sequence zero.
    pub const fn new(inner: W) -> Self {
        Self {
            inner,
            sequence: 0,
            buffer: Vec::new(),
        }
    }

    /// Sets the sequence id for the next packet.
    pub const fn set_sequence(&mut self, sequence: u8) {
        self.sequence = sequence;
    }

    /// Returns the stream so the connection can be upgraded to TLS.
    /// Writes out anything still buffered and hands back the stream with
    /// the next sequence id, so a caller can continue the same packet
    /// sequence over another transport.
    ///
    /// # Errors
    ///
    /// Returns the error of writing the remaining bytes.
    pub async fn into_inner(mut self) -> std::io::Result<(W, u8)> {
        self.write_buffered().await?;
        Ok((self.inner, self.sequence))
    }

    async fn write_buffered(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            self.inner.write_all(&self.buffer).await?;
            self.buffer.clear();
        }
        // A connection keeps its writer for its whole life; the buffer's
        // capacity is bounded so an idle pool does not hold what its largest
        // response once needed.
        if self.buffer.capacity() > 2 * WRITE_BUFFER_HIGH_WATER {
            self.buffer.shrink_to(WRITE_BUFFER_HIGH_WATER);
        }
        Ok(())
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
            self.buffer
                .extend_from_slice(&[length[0], length[1], length[2], self.sequence]);
            if chunk.len() >= WRITE_BUFFER_HIGH_WATER {
                // A body at or past the high-water mark goes to the stream
                // straight from the caller's slice, after the bytes ahead of
                // it, so the buffer never grows to hold a large packet.
                self.write_buffered().await?;
                self.inner.write_all(chunk).await?;
            } else {
                self.buffer.extend_from_slice(chunk);
                if self.buffer.len() >= WRITE_BUFFER_HIGH_WATER {
                    self.write_buffered().await?;
                }
            }
            self.sequence = self.sequence.wrapping_add(1);
            offset += take;
            if take < MAX_PAYLOAD {
                return Ok(());
            }
            if offset == payload.len() {
                // Exact multiple of the maximum: terminate with an empty
                // packet so the peer stops expecting more.
                self.buffer.extend_from_slice(&[0, 0, 0, self.sequence]);
                self.sequence = self.sequence.wrapping_add(1);
                return Ok(());
            }
        }
    }

    /// Flushes buffered bytes to the peer.
    ///
    /// # Errors
    /// Propagates I/O failures from the underlying stream.
    pub async fn flush(&mut self) -> std::io::Result<()> {
        self.write_buffered().await?;
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
        MAX_PAYLOAD, PacketReader, PacketWriter, READ_AHEAD, length_encoded_bytes,
        length_encoded_integer, put_length_encoded_bytes, put_length_encoded_integer,
    };

    async fn round_trip(payload: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut writer = PacketWriter::new(&mut encoded);
        writer.write_payload(payload).await.expect("write");
        writer.flush().await.expect("flush");
        let mut reader = PacketReader::new(encoded.as_slice());
        reader
            .next_payload()
            .await
            .expect("read")
            .expect("one payload")
    }

    #[tokio::test]
    async fn primed_bytes_are_read_exactly_once_before_the_underlying_stream() {
        // A caller that peeked the socket for a disconnect and got real data
        // instead of EOF hands those bytes back rather than losing them.
        // They must come out ahead of, not mixed into, whatever the stream
        // still holds.
        let mut encoded = Vec::new();
        let mut writer = PacketWriter::new(&mut encoded);
        writer.write_payload(b"tail").await.expect("write");
        writer.flush().await.expect("flush");

        // The header's first two bytes were "unavoidably" read elsewhere.
        let stolen = encoded[..2].to_vec();
        let remaining = &encoded[2..];

        let mut reader = PacketReader::new(remaining);
        reader.prime(&stolen);
        let payload = reader
            .next_payload()
            .await
            .expect("read")
            .expect("one payload");
        assert_eq!(payload, b"tail");
    }

    /// A stream that hands out at most `chunk` bytes per read and counts
    /// the reads it was asked for.
    struct Counted {
        bytes: Vec<u8>,
        at: usize,
        chunk: usize,
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl tokio::io::AsyncRead for Counted {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let take = (self.bytes.len() - self.at)
                .min(self.chunk)
                .min(buf.remaining());
            buf.put_slice(&self.bytes[self.at..self.at + take]);
            self.at += take;
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn framed(payloads: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for payload in payloads {
            let length = u32::try_from(payload.len()).expect("length").to_le_bytes();
            bytes.extend_from_slice(&[length[0], length[1], length[2], 0]);
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    async fn drain(bytes: Vec<u8>, chunk: usize, read_ahead: bool) -> (Vec<Vec<u8>>, usize) {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut reader = PacketReader::new(Counted {
            bytes,
            at: 0,
            chunk,
            reads: reads.clone(),
        });
        reader.set_read_ahead(read_ahead);
        let mut payloads = Vec::new();
        while reader.wait_for_packet().await.expect("wait") {
            payloads.push(reader.next_payload().await.expect("io").expect("payload"));
        }
        (payloads, reads.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Read exactly, a command is three reads - the byte that proves it
    /// arrived, the rest of the header, the body. Read ahead, a command
    /// whose bytes are already there is one.
    #[tokio::test]
    async fn reading_ahead_takes_a_command_in_one_read() {
        let command = framed(&[b"\x03SELECT 1"]);
        let (payloads, reads) = drain(command.clone(), usize::MAX, false).await;
        assert_eq!(payloads, [b"\x03SELECT 1".to_vec()]);
        assert_eq!(reads, 3 + 1, "three for the command, one finds the end");
        let (payloads, reads) = drain(command, usize::MAX, true).await;
        assert_eq!(payloads, [b"\x03SELECT 1".to_vec()]);
        assert_eq!(reads, 1 + 1, "one for the command, one finds the end");
    }

    /// Whatever the transport delivers per read - commands run together,
    /// or one byte at a time - the payloads are the same.
    #[tokio::test]
    async fn reading_ahead_frames_payloads_however_the_bytes_arrive() {
        let large = vec![7_u8; 3 * READ_AHEAD + 5];
        let exact = vec![9_u8; READ_AHEAD - 4];
        let expected: Vec<Vec<u8>> = vec![
            b"\x03SELECT 1".to_vec(),
            Vec::new(),
            large.clone(),
            b"\x0e".to_vec(),
            exact.clone(),
            b"\x01".to_vec(),
        ];
        let bytes = framed(&expected.iter().map(Vec::as_slice).collect::<Vec<_>>());
        for chunk in [1, 2, 3, 5, 7, READ_AHEAD - 1, READ_AHEAD, usize::MAX] {
            for read_ahead in [false, true] {
                let (payloads, _) = drain(bytes.clone(), chunk, read_ahead).await;
                assert_eq!(payloads, expected, "chunk {chunk}, read ahead {read_ahead}");
            }
        }
        // Three commands that arrived together are one read.
        let together = framed(&[b"\x03SELECT 1", b"\x0e", b"\x01"]);
        let (payloads, reads) = drain(together, usize::MAX, true).await;
        assert_eq!(payloads.len(), 3);
        assert_eq!(reads, 1 + 1);
    }

    /// A stream that ends inside a packet is an error with or without
    /// reading ahead; one that ends between packets is a clean close.
    #[tokio::test]
    async fn reading_ahead_tells_a_cut_packet_from_a_clean_close() {
        let mut cut = framed(&[b"\x03SELECT 1"]);
        cut.truncate(cut.len() - 2);
        for read_ahead in [false, true] {
            let mut reader = PacketReader::new(cut.as_slice());
            reader.set_read_ahead(read_ahead);
            let error = reader.next_payload().await.expect_err("cut packet");
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
            let mut reader = PacketReader::new(&b""[..]);
            reader.set_read_ahead(read_ahead);
            assert_eq!(reader.next_payload().await.expect("io"), None);
        }
    }

    #[tokio::test]
    async fn priming_with_nothing_owed_changes_nothing() {
        assert_eq!(round_trip(b"steady").await, b"steady");
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
        writer.flush().await.expect("flush");
        // Header + full body, then a bare header with zero length.
        assert_eq!(encoded.len(), 4 + MAX_PAYLOAD + 4);
        assert_eq!(&encoded[encoded.len() - 4..], &[0, 0, 0, 1]);
    }

    #[tokio::test]
    async fn a_large_body_bypasses_the_buffer_and_leaves_it_bounded() {
        let mut encoded = Vec::new();
        let mut writer = PacketWriter::new(&mut encoded);
        writer
            .write_payload(&vec![3_u8; 4 * super::WRITE_BUFFER_HIGH_WATER])
            .await
            .expect("write");
        writer.write_payload(b"small").await.expect("write");
        writer.flush().await.expect("flush");
        assert!(
            writer.buffer.capacity() <= 2 * super::WRITE_BUFFER_HIGH_WATER,
            "capacity {} outgrew the bound",
            writer.buffer.capacity()
        );
        let mut reader = PacketReader::new(encoded.as_slice());
        let large = reader.next_payload().await.expect("read").expect("payload");
        assert_eq!(large.len(), 4 * super::WRITE_BUFFER_HIGH_WATER);
        let small = reader.next_payload().await.expect("read").expect("payload");
        assert_eq!(small, b"small");
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
