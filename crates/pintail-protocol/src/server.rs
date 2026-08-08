//! Connection driver: handshake, then a command loop.
//!
//! The driver owns sequence numbering and packet boundaries so a handler
//! never sees them. Sequence ids reset to zero at the start of every command
//! and continue from the request within one; getting that wrong desynchronises
//! the client one packet later, where the symptom points at the wrong code.

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::command::Command;
use crate::handshake::{CapabilityFlags, Handshake, HandshakeResponse, SCRAMBLE_SIZE};
use crate::packet::{PacketReader, PacketWriter, put_length_encoded_integer};
use crate::resultset::{
    OkPacket, encode_binary_row, encode_column_definition, encode_eof, encode_error, encode_ok,
    encode_text_row,
};
use crate::types::{Column, ErrorKind, StatusFlags};

/// Capabilities the server offers. `CLIENT_DEPRECATE_EOF` is advertised so a
/// modern client negotiates it; the writers honour whichever form the client
/// then chose.
#[must_use]
pub fn server_capabilities() -> CapabilityFlags {
    CapabilityFlags::CLIENT_LONG_PASSWORD
        | CapabilityFlags::CLIENT_PROTOCOL_41
        | CapabilityFlags::CLIENT_SECURE_CONNECTION
        | CapabilityFlags::CLIENT_CONNECT_WITH_DB
        | CapabilityFlags::CLIENT_PLUGIN_AUTH
        | CapabilityFlags::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
        | CapabilityFlags::CLIENT_CONNECT_ATTRS
        | CapabilityFlags::CLIENT_DEPRECATE_EOF
}

/// One result set a handler produces.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    /// Column metadata, carrying the real length, charset and scale.
    pub columns: Vec<Column>,
    /// Rows as already-encoded column values; `None` is NULL.
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    /// Whether rows are in the binary protocol, as a prepared execute
    /// returns, rather than the text protocol.
    pub binary: bool,
}

/// What a handler returns for one command.
#[derive(Clone, Debug)]
pub enum Response {
    /// A result set.
    Rows(Box<ResultSet>),
    /// A statement that produced no rows.
    Ok(OkPacket, String),
    /// A failure the client should see as a `MySQL` error.
    Error(ErrorKind, String),
}

/// Metadata a prepare returns.
#[derive(Clone, Debug, Default)]
pub struct PreparedStatement {
    /// Assigned handle.
    pub id: u32,
    /// Parameter placeholders.
    pub parameters: Vec<Column>,
    /// Result columns, when they are known before execution.
    pub columns: Vec<Column>,
}

/// Everything the driver asks of the server it front-ends.
#[async_trait]
pub trait Handler: Send {
    /// Version string reported to clients.
    fn server_version(&self) -> String {
        "8.0.0".to_owned()
    }

    /// Connection id echoed by `CONNECTION_ID()`.
    fn connection_id(&self) -> u32 {
        0
    }

    /// Authentication plugin to request.
    fn auth_plugin(&self) -> &'static str {
        "mysql_native_password"
    }

    /// Verifies a handshake or change-user response. Returning `false`
    /// produces an access-denied error and closes the connection.
    async fn authenticate(&mut self, response: &HandshakeResponse, scramble: &[u8]) -> bool;

    /// Runs a text-protocol statement.
    async fn query(&mut self, sql: &[u8]) -> Response;

    /// Prepares a statement.
    async fn prepare(&mut self, sql: &[u8]) -> Result<PreparedStatement, (ErrorKind, String)>;

    /// Executes a prepared statement. `body` is the raw execute payload after
    /// the handle, because decoding needs the statement's parameter types.
    async fn execute(&mut self, statement: u32, body: &[u8]) -> Response;

    /// Deallocates a prepared statement. The protocol expects no reply.
    async fn close_statement(&mut self, statement: u32);

    /// Drops a statement's accumulated long data.
    async fn reset_statement(&mut self, _statement: u32) -> bool {
        true
    }

    /// Restores session defaults, keeping the connection open.
    async fn reset_connection(&mut self) {}

    /// Changes the default schema.
    async fn init_database(&mut self, database: &[u8]) -> Result<(), (ErrorKind, String)>;
}

/// What the client's first handshake packet turned out to be.
#[derive(Clone, Debug)]
pub enum InitialResponse {
    /// A full `HandshakeResponse41`, ready to authenticate.
    Full(HandshakeResponse),
    /// A bare capability packet requesting TLS. The caller upgrades the
    /// stream via [`Connection::into_parts`] and re-reads the full response
    /// over the encrypted connection.
    Ssl,
}

/// Drives one client connection.
pub struct Connection<R, W> {
    reader: PacketReader<R>,
    writer: PacketWriter<W>,
    capabilities: CapabilityFlags,
}

impl<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send> Connection<R, W> {
    /// Wraps a duplex stream.
    pub const fn new(reader: R, writer: W) -> Self {
        Self {
            reader: PacketReader::new(reader),
            writer: PacketWriter::new(writer),
            capabilities: CapabilityFlags::empty(),
        }
    }

    /// Capabilities the client agreed to, valid after the handshake.
    pub const fn capabilities(&self) -> CapabilityFlags {
        self.capabilities
    }

    /// Sends `HandshakeV10`. The first of the three handshake phases; split
    /// out from [`Self::handshake`] so a caller offering TLS can inspect the
    /// client's answer before deciding whether to authenticate over this
    /// stream or upgrade first.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub async fn send_greeting(
        &mut self,
        handler: &dyn Handler,
        scramble: [u8; SCRAMBLE_SIZE],
    ) -> std::io::Result<()> {
        let greeting = Handshake {
            server_version: &handler.server_version(),
            connection_id: handler.connection_id(),
            scramble,
            capabilities: server_capabilities(),
            character_set: 255,
            auth_plugin: handler.auth_plugin(),
        };
        self.writer.set_sequence(0);
        self.writer.write_payload(&greeting.encode()).await?;
        self.writer.flush().await
    }

    /// Reads the client's answer to the greeting.
    ///
    /// A client offering TLS sends a short capability-only packet, upgrades,
    /// then repeats the full response encrypted; [`InitialResponse::Ssl`]
    /// distinguishes that packet from a truncated one so the caller upgrades
    /// rather than rejecting a well-formed request.
    ///
    /// # Errors
    /// Propagates I/O failures, and reports a truncated or malformed packet.
    pub async fn read_initial_response(&mut self) -> std::io::Result<InitialResponse> {
        let payload =
            self.reader.next_payload().await?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response")
            })?;
        if HandshakeResponse::is_ssl_request(&payload) {
            return Ok(InitialResponse::Ssl);
        }
        let response = HandshakeResponse::parse(&payload).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed handshake response",
            )
        })?;
        self.capabilities = response.capabilities;
        Ok(InitialResponse::Full(response))
    }

    /// Verifies the client's response and writes the outcome. The final
    /// handshake phase, run once over whichever stream — plaintext or
    /// upgraded — carried the full response.
    ///
    /// # Errors
    /// Propagates I/O failures, and reports a failed login.
    pub async fn complete_authentication(
        &mut self,
        handler: &mut dyn Handler,
        response: HandshakeResponse,
        scramble: &[u8],
    ) -> std::io::Result<HandshakeResponse> {
        if handler.authenticate(&response, scramble).await {
            self.writer.set_sequence(self.reader.sequence());
            let ok = encode_ok(OkPacket::default(), "");
            self.writer.write_payload(&ok).await?;
            self.writer.flush().await?;
            Ok(response)
        } else {
            self.writer.set_sequence(self.reader.sequence());
            let denied = encode_error(ErrorKind::ErAccessDeniedError, "access denied");
            self.writer.write_payload(&denied).await?;
            self.writer.flush().await?;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "authentication failed",
            ))
        }
    }

    /// Runs the opening handshake over one plaintext or already-upgraded
    /// stream. Rejects an [`InitialResponse::Ssl`] packet, since honoring a
    /// TLS request means upgrading between [`Self::read_initial_response`]
    /// and this point — see [`Self::into_parts`] for that split.
    ///
    /// # Errors
    /// Propagates I/O failures, and reports a truncated, TLS-requesting, or
    /// failed login.
    pub async fn handshake(
        &mut self,
        handler: &mut dyn Handler,
        scramble: [u8; SCRAMBLE_SIZE],
    ) -> std::io::Result<HandshakeResponse> {
        self.send_greeting(handler, scramble).await?;
        let response = match self.read_initial_response().await? {
            InitialResponse::Full(response) => response,
            InitialResponse::Ssl => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "client requested TLS; use read_initial_response and into_parts",
                ));
            }
        };
        self.complete_authentication(handler, response, &scramble)
            .await
    }

    /// Reclaims the underlying reader and writer along with their sequence
    /// ids, so a caller that saw [`InitialResponse::Ssl`] can wrap the
    /// streams in TLS and continue the same handshake sequence on the
    /// encrypted connection via [`Self::new_at_sequence`]. `MySQL` does not
    /// reset numbering across the upgrade, so losing the sequence here
    /// desynchronises the very next packet.
    #[must_use]
    pub fn into_parts(self) -> (R, W, u8, u8) {
        let (reader, read_sequence) = self.reader.into_inner();
        let (writer, write_sequence) = self.writer.into_inner();
        (reader, writer, read_sequence, write_sequence)
    }

    /// Wraps a stream pair at specific sequence ids, the counterpart to
    /// [`Self::into_parts`] after a mid-handshake TLS upgrade.
    pub const fn new_at_sequence(
        reader: R,
        writer: W,
        read_sequence: u8,
        write_sequence: u8,
    ) -> Self {
        let mut reader = PacketReader::new(reader);
        reader.set_sequence(read_sequence);
        let mut writer = PacketWriter::new(writer);
        writer.set_sequence(write_sequence);
        Self {
            reader,
            writer,
            capabilities: CapabilityFlags::empty(),
        }
    }

    /// Reads and serves one command.
    ///
    /// Returns `false` once the client asked to quit or closed its socket, so
    /// the caller can distinguish a normal end from an error.
    ///
    /// # Errors
    /// Propagates I/O failures from the underlying stream.
    pub async fn serve_one(&mut self, handler: &mut dyn Handler) -> std::io::Result<bool> {
        let Some(payload) = self.reader.next_payload().await? else {
            return Ok(false);
        };
        // Every command restarts numbering, and the reply continues from the
        // request's sequence.
        self.writer.set_sequence(self.reader.sequence());
        match Command::parse(&payload) {
            Command::Quit => return Ok(false),
            Command::Ping => self.write_ok().await?,

            Command::InitDb(database) => match handler.init_database(database).await {
                Ok(()) => self.write_ok().await?,
                Err((kind, message)) => self.write_error(kind, &message).await?,
            },
            Command::Query(sql) => {
                let response = handler.query(sql).await;
                self.write_response(response).await?;
            }
            Command::Prepare(sql) => match handler.prepare(sql).await {
                Ok(statement) => self.write_prepare_response(&statement).await?,
                Err((kind, message)) => self.write_error(kind, &message).await?,
            },
            Command::Execute { statement, body } => {
                let response = handler.execute(statement, body).await;
                self.write_response(response).await?;
            }
            Command::Close(statement) => handler.close_statement(statement).await,
            Command::ResetStatement(statement) => {
                if handler.reset_statement(statement).await {
                    self.write_ok().await?;
                } else {
                    self.write_error(ErrorKind::ErUnknownError, "unknown statement")
                        .await?;
                }
            }
            // FIELD_LIST is legacy; an empty result is a valid answer and
            // keeps old clients moving.
            Command::FieldList(_) => {
                self.write_eof().await?;
            }
            Command::SendLongData { .. } => {}
            // Change-user reauthenticates in MySQL; a replica keyed to one
            // database has nothing to switch to, so both restore session
            // defaults and acknowledge.
            Command::ResetConnection | Command::ChangeUser { .. } => {
                handler.reset_connection().await;
                self.write_ok().await?;
            }
            // A malformed or unrecognised command is answered rather than
            // dropped, so the client learns which of the two happened.
            Command::Unknown(code) => {
                self.write_error(
                    ErrorKind::ErUnknownComError,
                    &format!("unknown command {code}"),
                )
                .await?;
            }
        }
        Ok(true)
    }

    async fn write_ok(&mut self) -> std::io::Result<()> {
        let payload = encode_ok(OkPacket::default(), "");
        self.writer.write_payload(&payload).await?;
        self.writer.flush().await
    }

    async fn write_eof(&mut self) -> std::io::Result<()> {
        let payload = encode_eof(self.capabilities, StatusFlags::empty(), 0);
        self.writer.write_payload(&payload).await?;
        self.writer.flush().await
    }

    async fn write_error(&mut self, kind: ErrorKind, message: &str) -> std::io::Result<()> {
        let payload = encode_error(kind, message);
        self.writer.write_payload(&payload).await?;
        self.writer.flush().await
    }

    async fn write_response(&mut self, response: Response) -> std::io::Result<()> {
        match response {
            Response::Ok(packet, info) => {
                let payload = encode_ok(packet, &info);
                self.writer.write_payload(&payload).await?;
                self.writer.flush().await
            }
            Response::Error(kind, message) => self.write_error(kind, &message).await,
            Response::Rows(result) => self.write_result_set(&result).await,
        }
    }

    async fn write_result_set(&mut self, result: &ResultSet) -> std::io::Result<()> {
        let mut header = Vec::new();
        put_length_encoded_integer(&mut header, result.columns.len() as u64);
        self.writer.write_payload(&header).await?;
        for column in &result.columns {
            self.writer
                .write_payload(&encode_column_definition(column))
                .await?;
        }
        // The column list is terminated by EOF unless the client deprecated
        // it, in which case rows follow immediately.
        if !self
            .capabilities
            .contains(CapabilityFlags::CLIENT_DEPRECATE_EOF)
        {
            let payload = encode_eof(self.capabilities, StatusFlags::empty(), 0);
            self.writer.write_payload(&payload).await?;
        }
        for row in &result.rows {
            let payload = if result.binary {
                encode_binary_row(row)
            } else {
                let borrowed: Vec<Option<&[u8]>> =
                    row.iter().map(|value| value.as_deref()).collect();
                encode_text_row(&borrowed)
            };
            self.writer.write_payload(&payload).await?;
        }
        self.write_eof().await
    }

    async fn write_prepare_response(
        &mut self,
        statement: &PreparedStatement,
    ) -> std::io::Result<()> {
        let mut header = vec![0x00];
        header.extend_from_slice(&statement.id.to_le_bytes());
        header.extend_from_slice(
            &u16::try_from(statement.columns.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        header.extend_from_slice(
            &u16::try_from(statement.parameters.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        header.push(0x00);
        header.extend_from_slice(&0_u16.to_le_bytes());
        self.writer.write_payload(&header).await?;

        for parameter in &statement.parameters {
            self.writer
                .write_payload(&encode_column_definition(parameter))
                .await?;
        }
        if !statement.parameters.is_empty()
            && !self
                .capabilities
                .contains(CapabilityFlags::CLIENT_DEPRECATE_EOF)
        {
            let payload = encode_eof(self.capabilities, StatusFlags::empty(), 0);
            self.writer.write_payload(&payload).await?;
        }
        for column in &statement.columns {
            self.writer
                .write_payload(&encode_column_definition(column))
                .await?;
        }
        if !statement.columns.is_empty()
            && !self
                .capabilities
                .contains(CapabilityFlags::CLIENT_DEPRECATE_EOF)
        {
            let payload = encode_eof(self.capabilities, StatusFlags::empty(), 0);
            self.writer.write_payload(&payload).await?;
        }
        self.writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Connection, Handler, InitialResponse, PreparedStatement, Response, ResultSet,
        server_capabilities,
    };
    use crate::handshake::{CapabilityFlags, HandshakeResponse, SCRAMBLE_SIZE};
    use crate::packet::PacketReader;
    use crate::resultset::OkPacket;
    use crate::types::{Column, ColumnType, ErrorKind};
    use async_trait::async_trait;

    struct Fixture {
        accept: bool,
        last_query: Vec<u8>,
    }

    #[async_trait]
    impl Handler for Fixture {
        async fn authenticate(&mut self, _: &HandshakeResponse, _: &[u8]) -> bool {
            self.accept
        }

        async fn query(&mut self, sql: &[u8]) -> Response {
            self.last_query = sql.to_vec();
            let mut column = Column::new("n", ColumnType::MysqlTypeLonglong);
            column.column_length = 20;
            Response::Rows(Box::new(ResultSet {
                columns: vec![column],
                rows: vec![vec![Some(b"7".to_vec())], vec![None]],
                binary: false,
            }))
        }

        async fn prepare(&mut self, _: &[u8]) -> Result<PreparedStatement, (ErrorKind, String)> {
            Ok(PreparedStatement {
                id: 3,
                parameters: vec![Column::new("?", ColumnType::MysqlTypeLonglong)],
                columns: Vec::new(),
            })
        }

        async fn execute(&mut self, _: u32, _: &[u8]) -> Response {
            Response::Ok(OkPacket::default(), String::new())
        }

        async fn close_statement(&mut self, _: u32) {}

        async fn init_database(&mut self, _: &[u8]) -> Result<(), (ErrorKind, String)> {
            Ok(())
        }
    }

    fn client_response(capabilities: CapabilityFlags) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&capabilities.bits().to_le_bytes());
        payload.extend_from_slice(&16_777_216_u32.to_le_bytes());
        payload.push(255);
        payload.extend_from_slice(&[0_u8; 23]);
        payload.extend_from_slice(b"analytics\0");
        payload.push(0);
        if capabilities.contains(CapabilityFlags::CLIENT_CONNECT_WITH_DB) {
            payload.extend_from_slice(b"analytics\0");
        }
        if capabilities.contains(CapabilityFlags::CLIENT_PLUGIN_AUTH) {
            payload.extend_from_slice(b"mysql_native_password\0");
        }
        payload
    }

    /// Frames a payload as one packet with the given sequence.
    fn packet(sequence: u8, payload: &[u8]) -> Vec<u8> {
        let length = u32::try_from(payload.len()).unwrap_or(0).to_le_bytes();
        let mut framed = vec![length[0], length[1], length[2], sequence];
        framed.extend_from_slice(payload);
        framed
    }

    #[tokio::test]
    async fn a_rejected_login_tells_the_client_before_failing() {
        let input = packet(1, &client_response(CapabilityFlags::CLIENT_PROTOCOL_41));
        let mut output = Vec::new();
        let mut connection = Connection::new(input.as_slice(), &mut output);
        let mut handler = Fixture {
            accept: false,
            last_query: Vec::new(),
        };
        assert!(
            connection
                .handshake(&mut handler, [0_u8; SCRAMBLE_SIZE])
                .await
                .is_err()
        );
        // Greeting, then an error packet — never a silent close.
        let mut reader = PacketReader::new(output.as_slice());
        let greeting = reader.next_payload().await.expect("io").expect("greeting");
        assert_eq!(greeting[0], 10);
        let denied = reader.next_payload().await.expect("io").expect("error");
        assert_eq!(denied[0], 0xff);
        assert_eq!(
            &denied[1..3],
            &ErrorKind::ErAccessDeniedError.code().to_le_bytes()
        );
    }

    #[tokio::test]
    async fn a_query_writes_header_columns_rows_and_terminator() {
        let mut input = packet(1, &client_response(server_capabilities()));
        input.extend_from_slice(&packet(0, b"\x03SELECT 1"));
        let mut output = Vec::new();
        let mut connection = Connection::new(input.as_slice(), &mut output);
        let mut handler = Fixture {
            accept: true,
            last_query: Vec::new(),
        };
        connection
            .handshake(&mut handler, [0_u8; SCRAMBLE_SIZE])
            .await
            .expect("handshake");
        assert!(connection.serve_one(&mut handler).await.expect("serve"));
        assert_eq!(handler.last_query, b"SELECT 1");

        let mut reader = PacketReader::new(output.as_slice());
        reader.next_payload().await.expect("io").expect("greeting");
        reader.next_payload().await.expect("io").expect("auth ok");
        // One column, so the header is a single length-encoded 1. With
        // DEPRECATE_EOF negotiated no EOF separates columns from rows.
        assert_eq!(reader.next_payload().await.expect("io"), Some(vec![1]));
        let definition = reader.next_payload().await.expect("io").expect("column");
        assert!(definition.starts_with(&[3, b'd', b'e', b'f']));
        assert_eq!(
            reader.next_payload().await.expect("io"),
            Some(vec![1, b'7'])
        );
        assert_eq!(reader.next_payload().await.expect("io"), Some(vec![0xfb]));
        let terminator = reader.next_payload().await.expect("io").expect("eof");
        assert_eq!(terminator[0], 0xfe);
    }

    #[tokio::test]
    async fn quit_and_a_closed_socket_both_end_the_loop() {
        for tail in [Some(packet(0, b"\x01")), None] {
            let mut input = packet(1, &client_response(server_capabilities()));
            if let Some(tail) = tail {
                input.extend_from_slice(&tail);
            }
            let mut output = Vec::new();
            let mut connection = Connection::new(input.as_slice(), &mut output);
            let mut handler = Fixture {
                accept: true,
                last_query: Vec::new(),
            };
            connection
                .handshake(&mut handler, [0_u8; SCRAMBLE_SIZE])
                .await
                .expect("handshake");
            assert!(!connection.serve_one(&mut handler).await.expect("serve"));
        }
    }

    #[tokio::test]
    async fn an_unknown_command_is_answered_rather_than_dropped() {
        let mut input = packet(1, &client_response(server_capabilities()));
        input.extend_from_slice(&packet(0, b"\x99"));
        let mut output = Vec::new();
        let mut connection = Connection::new(input.as_slice(), &mut output);
        let mut handler = Fixture {
            accept: true,
            last_query: Vec::new(),
        };
        connection
            .handshake(&mut handler, [0_u8; SCRAMBLE_SIZE])
            .await
            .expect("handshake");
        assert!(connection.serve_one(&mut handler).await.expect("serve"));

        let mut reader = PacketReader::new(output.as_slice());
        reader.next_payload().await.expect("io").expect("greeting");
        reader.next_payload().await.expect("io").expect("auth ok");
        let error = reader.next_payload().await.expect("io").expect("error");
        assert_eq!(error[0], 0xff);
    }

    fn ssl_request(capabilities: CapabilityFlags) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&capabilities.bits().to_le_bytes());
        payload.extend_from_slice(&16_777_216_u32.to_le_bytes());
        payload.push(255);
        payload.extend_from_slice(&[0_u8; 23]);
        payload
    }

    #[tokio::test]
    async fn a_bare_ssl_request_is_recognised_before_a_full_response_is_expected() {
        let input = packet(
            1,
            &ssl_request(CapabilityFlags::CLIENT_PROTOCOL_41 | CapabilityFlags::CLIENT_SSL),
        );
        let mut output = Vec::new();
        let mut connection = Connection::new(input.as_slice(), &mut output);
        let handler = Fixture {
            accept: true,
            last_query: Vec::new(),
        };
        connection
            .send_greeting(&handler, [0_u8; SCRAMBLE_SIZE])
            .await
            .expect("greeting");
        assert!(matches!(
            connection.read_initial_response().await.expect("read"),
            InitialResponse::Ssl
        ));
    }

    #[tokio::test]
    async fn a_tls_upgrade_preserves_sequence_numbers_across_the_stream_swap() {
        // The client's SSL request is sequence 1; after upgrading, its full
        // response continues at sequence 2. Losing that number here would
        // desynchronise every packet the encrypted side sends afterward.
        let ssl_packet = packet(
            1,
            &ssl_request(CapabilityFlags::CLIENT_PROTOCOL_41 | CapabilityFlags::CLIENT_SSL),
        );
        let mut output = Vec::new();
        let mut connection = Connection::new(ssl_packet.as_slice(), &mut output);
        let mut handler = Fixture {
            accept: true,
            last_query: Vec::new(),
        };
        connection
            .send_greeting(&handler, [0_u8; SCRAMBLE_SIZE])
            .await
            .expect("greeting");
        assert!(matches!(
            connection.read_initial_response().await.expect("read"),
            InitialResponse::Ssl
        ));

        let (_reader, writer, read_sequence, write_sequence) = connection.into_parts();
        assert_eq!(read_sequence, 2, "next read continues from the SSL request");

        // Simulate the encrypted stream by feeding the deferred full
        // response as if it arrived over TLS, framed at the preserved
        // sequence.
        let upgraded_input = packet(
            read_sequence,
            &client_response(CapabilityFlags::CLIENT_PROTOCOL_41),
        );
        let mut upgraded = Connection::new_at_sequence(
            upgraded_input.as_slice(),
            writer,
            read_sequence,
            write_sequence,
        );
        let response = upgraded
            .read_initial_response()
            .await
            .expect("read upgraded response");
        let InitialResponse::Full(response) = response else {
            panic!("expected a full response over the upgraded stream");
        };
        let response = upgraded
            .complete_authentication(&mut handler, response, &[0_u8; SCRAMBLE_SIZE])
            .await
            .expect("authenticate");
        assert_eq!(response.username, b"analytics");
    }
}
