//! Client command packets.
//!
//! A command is one leading byte and a body whose shape depends on it. An
//! unrecognised or malformed command becomes [`Command::Unknown`] rather than
//! a parse failure, because the protocol's answer to one is an error packet
//! on a connection that stays open — dropping the socket instead leaves the
//! client guessing whether the server died or rejected the statement.

use crate::packet::length_encoded_bytes;

/// A parsed client command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// `COM_QUERY`: run a statement in the text protocol.
    Query(&'a [u8]),
    /// `COM_STMT_PREPARE`: parse a statement and return its metadata.
    Prepare(&'a [u8]),
    /// `COM_STMT_EXECUTE`: run a prepared statement. The body stays raw
    /// because decoding parameters needs the statement's parameter types,
    /// which only the caller holds.
    Execute {
        /// Statement handle.
        statement: u32,
        /// Remaining body: flags, iteration count, NULL bitmap, parameters.
        body: &'a [u8],
    },
    /// `COM_STMT_SEND_LONG_DATA`: append to one parameter before execute.
    SendLongData {
        /// Statement handle.
        statement: u32,
        /// Parameter index.
        parameter: u16,
        /// Bytes to append.
        data: &'a [u8],
    },
    /// `COM_STMT_CLOSE`: deallocate. The protocol sends no reply.
    Close(u32),
    /// `COM_STMT_RESET`: drop accumulated long data, keep the statement.
    ResetStatement(u32),
    /// `COM_INIT_DB`: change the default schema.
    InitDb(&'a [u8]),
    /// `COM_FIELD_LIST`: legacy column listing, still used by old clients.
    FieldList(&'a [u8]),
    /// `COM_PING`.
    Ping,
    /// `COM_QUIT`.
    Quit,
    /// `COM_RESET_CONNECTION`: restore session defaults, keep the socket.
    ResetConnection,
    /// `COM_CHANGE_USER`: reauthenticate on the same socket.
    ChangeUser {
        /// Requested username.
        username: &'a [u8],
        /// Authentication response.
        auth_response: &'a [u8],
        /// Requested default schema.
        database: &'a [u8],
    },
    /// Anything else, including a truncated body.
    Unknown(u8),
}

impl<'a> Command<'a> {
    /// Parses one command payload.
    ///
    /// An empty payload is `Unknown(0)`; a body too short for its command is
    /// `Unknown(code)`. Both are answered with an error packet rather than a
    /// dropped connection.
    #[must_use]
    pub fn parse(payload: &'a [u8]) -> Self {
        let Some((&code, body)) = payload.split_first() else {
            return Self::Unknown(0);
        };
        let statement_handle = || {
            body.get(..4)
                .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        };
        match code {
            0x01 => Self::Quit,
            0x02 => Self::InitDb(body),
            0x03 => Self::Query(body),
            0x04 => Self::FieldList(body),
            0x0e => Self::Ping,
            0x11 => Self::parse_change_user(body).unwrap_or(Self::Unknown(code)),
            0x16 => Self::Prepare(body),
            0x17 => statement_handle().map_or(Self::Unknown(code), |statement| Self::Execute {
                statement,
                body: body.get(4..).unwrap_or_default(),
            }),
            0x18 => Self::parse_send_long_data(body).unwrap_or(Self::Unknown(code)),
            0x19 => statement_handle().map_or(Self::Unknown(code), Self::Close),
            0x1a => statement_handle().map_or(Self::Unknown(code), Self::ResetStatement),
            0x1f => Self::ResetConnection,
            other => Self::Unknown(other),
        }
    }

    fn parse_send_long_data(body: &'a [u8]) -> Option<Self> {
        let statement = body
            .get(..4)
            .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))?;
        let parameter = body
            .get(4..6)
            .map(|raw| u16::from_le_bytes([raw[0], raw[1]]))?;
        Some(Self::SendLongData {
            statement,
            parameter,
            data: body.get(6..).unwrap_or_default(),
        })
    }

    /// `COM_CHANGE_USER` is username, auth response, database, then charset
    /// and plugin. Only the first three matter to a replica that
    /// reauthenticates against its own key store.
    fn parse_change_user(body: &'a [u8]) -> Option<Self> {
        let end = body.iter().position(|byte| *byte == 0)?;
        let username = body.get(..end)?;
        let rest = body.get(end + 1..)?;
        // Modern clients length-prefix the auth response; the legacy form is
        // a single length byte. Both start with a length, so the
        // length-encoded reader covers each.
        let (auth_response, consumed) = length_encoded_bytes(rest)?;
        let rest = rest.get(consumed..)?;
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(rest.len());
        let database = rest.get(..end)?;
        Some(Self::ChangeUser {
            username,
            auth_response: auth_response.unwrap_or_default(),
            database,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Command;
    use crate::packet::put_length_encoded_bytes;

    #[test]
    fn every_served_command_parses() {
        assert_eq!(Command::parse(b"\x03SELECT 1"), Command::Query(b"SELECT 1"));
        assert_eq!(
            Command::parse(b"\x16SELECT ?"),
            Command::Prepare(b"SELECT ?")
        );
        assert_eq!(
            Command::parse(b"\x02analytics"),
            Command::InitDb(b"analytics")
        );
        assert_eq!(Command::parse(b"\x04events"), Command::FieldList(b"events"));
        assert_eq!(Command::parse(b"\x0e"), Command::Ping);
        assert_eq!(Command::parse(b"\x01"), Command::Quit);
        assert_eq!(Command::parse(b"\x1f"), Command::ResetConnection);
        assert_eq!(Command::parse(b"\x19\x07\x00\x00\x00"), Command::Close(7));
        assert_eq!(
            Command::parse(b"\x1a\x07\x00\x00\x00"),
            Command::ResetStatement(7)
        );
    }

    #[test]
    fn execute_keeps_its_body_for_the_caller_to_decode() {
        assert_eq!(
            Command::parse(b"\x17\x02\x00\x00\x00\x00\x01\x00\x00\x00"),
            Command::Execute {
                statement: 2,
                body: b"\x00\x01\x00\x00\x00",
            }
        );
    }

    #[test]
    fn send_long_data_carries_its_parameter_index() {
        assert_eq!(
            Command::parse(b"\x18\x05\x00\x00\x00\x03\x00payload"),
            Command::SendLongData {
                statement: 5,
                parameter: 3,
                data: b"payload",
            }
        );
    }

    #[test]
    fn change_user_reads_username_response_and_database() {
        let mut body = Vec::from(&b"analytics\0"[..]);
        put_length_encoded_bytes(&mut body, b"scrambled");
        body.extend_from_slice(b"reporting\0");
        body.extend_from_slice(&[0x2d, 0x00]);
        assert_eq!(
            Command::parse(&[&[0x11][..], &body].concat()),
            Command::ChangeUser {
                username: b"analytics",
                auth_response: b"scrambled",
                database: b"reporting",
            }
        );
    }

    #[test]
    fn malformed_and_unknown_commands_stay_recoverable() {
        // Each of these must be answerable with an error packet rather than
        // by dropping the connection, so none may fail to parse.
        assert_eq!(Command::parse(b""), Command::Unknown(0));
        assert_eq!(Command::parse(b"\x19\x01"), Command::Unknown(0x19));
        assert_eq!(Command::parse(b"\x17\x01"), Command::Unknown(0x17));
        assert_eq!(
            Command::parse(b"\x18\x01\x00\x00\x00"),
            Command::Unknown(0x18)
        );
        assert_eq!(
            Command::parse(b"\x11no-null-terminator"),
            Command::Unknown(0x11)
        );
        assert_eq!(Command::parse(b"\x99"), Command::Unknown(0x99));
    }
}
