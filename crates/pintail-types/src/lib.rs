//! Shared value, type, schema, and error model for Pintail.
//!
//! This crate deliberately has no dependencies on other Pintail crates. The
//! storage, replication, query, and protocol modules all cross this seam with
//! the same typed row representation.

mod canonical;
mod row;
mod schema;
mod value;

pub use canonical::{
    civil_from_days, div_decimal_round_half_up, format_date_days, format_datetime_micros,
    format_decimal_scaled, format_time_micros, parse_date_days, parse_datetime_lenient_micros,
    parse_datetime_micros, parse_decimal_rounded, parse_decimal_scaled, parse_time_micros,
    round_micros_to_fsp,
};
pub use row::{KeyPart, PrimaryKey, StoredRow};
pub use schema::{Column, KeyMode, SchemaError, TableSchema, declaration_labels};
pub use value::{DataType, Float64, Value};
