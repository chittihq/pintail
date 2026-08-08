//! Pintail's `MySQL` client/server wire protocol.
//!
//! Written against `MySQL`'s documented protocol rather than derived from an
//! existing crate, because the fields Pintail must control — a column's real
//! length, character set and decimal scale — are exactly the ones the
//! available implementation fixed at constants.

pub mod packet;
pub mod types;

pub use packet::{MAX_PAYLOAD, PacketReader, PacketWriter};
pub use types::{Column, ColumnFlags, ColumnType, ErrorKind, StatusFlags};
