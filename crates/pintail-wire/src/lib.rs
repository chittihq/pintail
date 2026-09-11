//! Read-only `MySQL` wire protocol server for Pintail.

mod admission;
mod engine;
mod limits;
mod observe;
mod presentation;
mod replica_cache;
mod result_rows;
mod server;
mod shared_query;
mod trace;

pub use admission::{
    DEFAULT_QUEUE_WAIT, QueryAdmission, QueryClass, QueryPermit, default_max_concurrent_queries,
    init_shared_admission, init_shared_admission_with_reserved, init_shared_admission_with_wait,
    shared_admission,
};
pub use engine::{
    DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, QueryError, QueryField, QueryOutput, QueryStats,
    ReplicaEngine, SqlRejection, table_directory,
};
pub mod managed_tls;

pub use limits::{
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_PREPARED_STATEMENT_BYTES, DEFAULT_MAX_PREPARED_STATEMENTS,
    WireLimits, WireMetrics, wire_metrics,
};
pub use server::{
    DEFAULT_WIRE_IDLE_TIMEOUT, WireOptions, WireTls, load_wire_tls, serve, serve_until,
    serve_until_configured, serve_until_with_memory_limit, serve_until_with_options,
};

pub use engine::replica_cache_stats;
pub use replica_cache::ReplicaCacheStats;
pub use result_rows::ResultRows;
pub use shared_query::{SharedQueryStats, shared_query_stats};

mod metadata_provider;
