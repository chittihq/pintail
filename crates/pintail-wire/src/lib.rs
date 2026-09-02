//! Read-only `MySQL` wire protocol server for Pintail.

mod admission;
mod engine;
mod observe;
mod replica_cache;
mod server;

pub use admission::{
    QueryAdmission, QueryPermit, default_max_concurrent_queries, init_shared_admission,
    shared_admission,
};
pub use engine::{
    DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, QueryError, QueryField, QueryOutput, QueryStats,
    ReplicaEngine, SqlRejection, table_directory,
};
pub mod managed_tls;

pub use server::{
    DEFAULT_WIRE_IDLE_TIMEOUT, WireTls, load_wire_tls, serve, serve_until,
    serve_until_with_memory_limit, serve_until_with_options,
};

pub use engine::replica_cache_stats;
pub use replica_cache::ReplicaCacheStats;
