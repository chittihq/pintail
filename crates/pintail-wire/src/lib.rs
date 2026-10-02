//! Read-only `MySQL` wire protocol server for Pintail.

mod admission;
mod engine;
mod limits;
mod observe;
mod plan_cache;
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
    Answer, DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, InlineAnswer, QueryError, QueryField,
    QueryOutput, QueryStats, ReplicaEngine, RowSink, STREAM_AFTER_ROWS, SqlRejection,
    table_directory,
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

pub use engine::{plan_cache_stats, replica_cache_stats};
pub use plan_cache::PlanCacheStats;
pub use replica_cache::ReplicaCacheStats;
pub use result_rows::ResultRows;
pub use server::inline_statements;
pub use shared_query::{SharedQueryStats, shared_queries_enabled, shared_query_stats};

mod metadata_provider;
