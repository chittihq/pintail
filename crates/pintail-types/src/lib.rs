//! Shared value, type, schema, and error model for Pintail.
//!
//! This crate deliberately has no dependencies on other Pintail crates. The
//! storage, replication, query, and protocol modules all cross this seam with
//! the same typed row representation.

mod row;
mod schema;
mod value;

pub use row::{KeyPart, PrimaryKey, StoredRow};
pub use schema::{Column, KeyMode, SchemaError, TableSchema};
pub use value::{DataType, Float64, Value};
