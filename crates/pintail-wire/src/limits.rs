//! Bounds on what a wire client can hold open without running a query.
//!
//! Query admission bounds execution. It says nothing about the two things a
//! client can accumulate while executing nothing: connections, each of which
//! is a task, a session and an engine handle for as long as it stays open;
//! and prepared statements, each of which keeps its statement text for as
//! long as the session lives. Neither is charged to the query memory
//! ceiling, so before these bounds the process resident set was bounded
//! only by the idle timeout and the client's patience.
//!
//! Both limits refuse the way `MySQL` refuses, with the error code drivers
//! already handle: 1040 "Too many connections" at the connection ceiling,
//! 1461 at the prepared-statement ceiling. Fast refusal is the useful
//! behaviour, as it is for admission: a pool can back off, which it cannot
//! do while queued.
//!
//! The counters are process-wide, like the admission bound: there is one
//! listener per process, and `/metrics` is process-wide.

use std::sync::atomic::{AtomicU64, Ordering};

/// `MySQL`'s own `max_connections` default. Chosen so the ceiling is one
/// pools are already tuned against, not one invented here.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1000;

/// Prepared statements one session may hold at once. `MySQL` bounds this
/// globally (`max_prepared_stmt_count`, 16382); per session, 1024 is above
/// every driver-side statement cache and below anything a client could
/// reach without a leak.
pub const DEFAULT_MAX_PREPARED_STATEMENTS: usize = 1024;

/// Statement text one session may keep across its prepared statements. A
/// megabyte statement is legitimate; a thousand of them is not, and the
/// count alone would allow it.
pub const DEFAULT_MAX_PREPARED_STATEMENT_BYTES: usize = 16 * 1024 * 1024;

/// What a wire listener enforces beyond query admission. Zero disables a
/// bound, as it does for `--max-concurrent-queries`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireLimits {
    /// Connections accepted at once, authenticated or not. A connection
    /// holds its slot from accept until its task ends.
    pub max_connections: usize,
    /// Prepared statements one session may hold open at once.
    pub max_prepared_statements: usize,
    /// Retained statement text per session, in bytes.
    pub max_prepared_statement_bytes: usize,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_prepared_statements: DEFAULT_MAX_PREPARED_STATEMENTS,
            max_prepared_statement_bytes: DEFAULT_MAX_PREPARED_STATEMENT_BYTES,
        }
    }
}

static CONNECTIONS_ACTIVE: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_REFUSED: AtomicU64 = AtomicU64::new(0);
static PREPARED_REFUSED: AtomicU64 = AtomicU64::new(0);
static CONNECTION_LIMIT: AtomicU64 = AtomicU64::new(DEFAULT_MAX_CONNECTIONS as u64);

/// A point-in-time reading of the wire bounds, for `/metrics`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WireMetrics {
    /// Connections currently holding a slot.
    pub connections_active: u64,
    /// The configured connection ceiling; zero when unbounded.
    pub connections_limit: u64,
    /// Connections refused at the ceiling since startup.
    pub connections_refused: u64,
    /// `PREPARE`s refused at a session ceiling since startup.
    pub prepared_statements_refused: u64,
}

/// The current reading.
#[must_use]
pub fn wire_metrics() -> WireMetrics {
    WireMetrics {
        connections_active: CONNECTIONS_ACTIVE.load(Ordering::Relaxed),
        connections_limit: CONNECTION_LIMIT.load(Ordering::Relaxed),
        connections_refused: CONNECTIONS_REFUSED.load(Ordering::Relaxed),
        prepared_statements_refused: PREPARED_REFUSED.load(Ordering::Relaxed),
    }
}

pub(crate) fn record_connection_limit(limit: usize) {
    CONNECTION_LIMIT.store(limit as u64, Ordering::Relaxed);
}

pub(crate) fn record_connection_refused() {
    CONNECTIONS_REFUSED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_prepared_refused() {
    PREPARED_REFUSED.fetch_add(1, Ordering::Relaxed);
}

/// Counts one connection for as long as it is held. Taken at accept and
/// dropped with the connection's task, so the active gauge follows the real
/// lifetime rather than the authenticated one.
pub(crate) struct ActiveConnection;

impl ActiveConnection {
    pub(crate) fn register() -> Self {
        CONNECTIONS_ACTIVE.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        CONNECTIONS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveConnection, wire_metrics};

    #[test]
    fn the_active_gauge_follows_the_guard() {
        let before = wire_metrics().connections_active;
        let first = ActiveConnection::register();
        let second = ActiveConnection::register();
        assert_eq!(wire_metrics().connections_active, before + 2);
        drop(first);
        assert_eq!(wire_metrics().connections_active, before + 1);
        drop(second);
        assert_eq!(wire_metrics().connections_active, before);
    }
}
