//! Read-only `MySQL` wire protocol server for Pintail.

mod engine;
mod server;

pub use engine::{
    DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, QueryError, QueryField, QueryOutput, QueryStats,
    ReplicaEngine, table_directory,
};
pub use server::{serve, serve_until, serve_until_with_memory_limit};
