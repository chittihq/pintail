//! Pintail's `MySQL` client/server wire protocol.
//!
//! Written against `MySQL`'s documented protocol rather than derived from an
//! existing crate, because the fields Pintail must control — a column's real
//! length, character set and decimal scale — are exactly the ones the
//! available implementation fixed at constants.

pub mod command;
pub mod handshake;
pub mod packet;
pub mod resultset;
pub mod server;
pub mod types;
pub mod value;

pub use command::Command;
pub use handshake::{CapabilityFlags, Handshake, HandshakeResponse, SCRAMBLE_SIZE};
pub use packet::{MAX_PAYLOAD, PacketReader, PacketWriter};
pub use resultset::{OkPacket, encode_column_definition, encode_eof, encode_error, encode_ok};
pub use server::{Connection, Handler, PreparedStatement, Response, ResultSet};
pub use types::{Column, ColumnFlags, ColumnType, ErrorKind, StatusFlags};
pub use value::{BinaryValue, ParameterType, decode_execute_parameters};
