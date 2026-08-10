//! Level-gated diagnostic logging, shared by every Pintail crate.
//!
//! This exists because the engine crates could not log at all. They are
//! libraries with no access to the API's event bus, so the only failures that
//! ever reached an operator were the ones that propagated all the way up to a
//! supervisor or an HTTP handler. Everything a long-running operation does in
//! between - which binlog position CDC resumed from, which snapshot chunk is
//! in flight, why a poll cycle decided to re-read - was invisible.
//!
//! There is no `log` or `tracing` dependency behind this. A facade needs a
//! level, one environment lookup and a write to stderr, all of which are in
//! std; taking a dependency to get them would add a tree to every crate in the
//! workspace for no capability this codebase uses.
//!
//! # Levels
//!
//! `PINTAIL_LOG` selects one of:
//!
//! - `error` - failures only
//! - `info` - the default: lifecycle transitions and request outcomes
//! - `debug` - adds per-item detail (per table, per chunk, per cycle)
//!
//! An unrecognised value falls back to `info`. Logging configuration must
//! never be the reason a server refuses to boot.
//!
//! # What must never be logged
//!
//! Source DSNs, API key secrets, invite tokens, OAuth exchange codes, JWTs and
//! row values. Call sites pass identifiers and counts. A log is the one place
//! a credential is most likely to be copied into a bug report.

use std::sync::OnceLock;

/// Failures only.
pub const ERROR: u8 = 0;
/// Lifecycle transitions and request outcomes. The default.
pub const INFO: u8 = 1;
/// Per-item detail: per table, per chunk, per cycle.
pub const DEBUG: u8 = 2;

/// Whether a message at `level` should be written.
///
/// The environment is read once and cached. A long-running process should not
/// pay an env lookup per log line, and a level that changed halfway through a
/// run would make the output harder to read rather than easier.
#[must_use]
pub fn enabled(level: u8) -> bool {
    static CONFIGURED: OnceLock<u8> = OnceLock::new();
    let configured = *CONFIGURED.get_or_init(|| {
        match std::env::var("PINTAIL_LOG")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "error" | "err" => ERROR,
            "debug" | "trace" => DEBUG,
            _ => INFO,
        }
    });
    level <= configured
}

/// Writes one line to stderr, unconditionally.
///
/// Callers go through the macros, which check the level first. Kept public so
/// the macros work from other crates without exposing stderr handling to each
/// of them.
pub fn emit(message: &str) {
    eprintln!("pintail {message}");
}

/// Logs a failure. Always emitted.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::ERROR) {
            $crate::emit(&format!($($arg)*));
        }
    };
}

/// Logs a lifecycle transition. Emitted at `info` and below.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::INFO) {
            $crate::emit(&format!($($arg)*));
        }
    };
}

/// Logs per-item detail. Emitted only at `debug`.
///
/// The level is checked before the arguments are formatted, so a debug line in
/// a hot loop costs one comparison when it is switched off.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::DEBUG) {
            $crate::emit(&format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::{DEBUG, ERROR, INFO, enabled};

    #[test]
    fn error_is_always_enabled() {
        // Whatever the environment says, a failure is reportable. The default
        // is info, so this holds without configuring anything.
        assert!(enabled(ERROR));
    }

    #[test]
    fn info_is_the_default_and_debug_is_not() {
        // These read the same cached level, so they assert the default rather
        // than each setting a different one - OnceLock means the first read in
        // the process wins, and tests share a process.
        assert!(enabled(INFO));
        assert!(!enabled(DEBUG));
    }
}
