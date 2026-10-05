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
//! - `warn` - adds conditions an operator should act on
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

/// A destination for log lines beyond stderr.
///
/// Registered once at startup by the binary. Kept as a plain function pointer
/// so this crate keeps its zero-dependency property: shipping lines to Sentry
/// or Logtail needs an HTTP client and a runtime, and neither belongs in the
/// crate every engine crate depends on.
pub type Sink = fn(level: u8, message: &str);

static SINK: OnceLock<Sink> = OnceLock::new();

/// Registers the sink that receives every emitted line.
///
/// Idempotent by construction: a second call is ignored rather than replacing
/// a live exporter mid-run. Returns whether this call installed it.
pub fn set_sink(sink: Sink) -> bool {
    SINK.set(sink).is_ok()
}

/// Failures only.
pub const ERROR: u8 = 0;
/// Conditions an operator should act on that are not failures of the work
/// in hand.
pub const WARN: u8 = 1;
/// Lifecycle transitions and request outcomes. The default.
pub const INFO: u8 = 2;
/// Per-item detail: per table, per chunk, per cycle.
pub const DEBUG: u8 = 3;

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
            "warn" | "warning" => WARN,
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
    emit_at(INFO, message);
}

/// Writes one line to stderr and to the registered sink, unconditionally.
///
/// The level travels with the message because the sink needs it: an exporter
/// that raised an incident for every `info` line would be useless, and one
/// that could not tell an error from a heartbeat could not filter at all.
///
/// stderr is written first. A remote exporter is the part most likely to be
/// misconfigured or unreachable, and the local line is the one an operator
/// reads when it is.
///
/// The line reaches stderr in one write. stderr is unbuffered, so a line
/// written in pieces is a system call per piece - three for every line, on
/// the thread that logged it - and two threads' pieces can interleave. A
/// stderr that cannot be written to is ignored: a log line must never be
/// what stops the work it describes.
pub fn emit_at(level: u8, message: &str) {
    use std::io::Write as _;
    // Lines queued before this one are written before it: a line written
    // at once never overtakes a deferred line that was logged first.
    flush_deferred();
    let mut line = String::with_capacity(message.len() + 9);
    line.push_str("pintail ");
    line.push_str(message);
    line.push('\n');
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
    if let Some(sink) = SINK.get() {
        sink(level, message);
    }
}

/// Lines waiting for the background writer, and whether it has been woken
/// for them.
struct Deferred {
    lines: Vec<(u8, String)>,
    /// The writer is awake and will take whatever is queued; nobody needs
    /// to wake it again. False whenever the writer waits, so a line queued
    /// while it waits always wakes it.
    writing: bool,
}

struct DeferredQueue {
    queue: std::sync::Mutex<Deferred>,
    queued: std::sync::Condvar,
    /// Held by whoever is writing queued lines.
    flushing: std::sync::Mutex<()>,
}

static DEFERRED: OnceLock<Option<&'static DeferredQueue>> = OnceLock::new();

/// How long the background writer lets lines gather before it writes them:
/// the longest a deferred line trails the event it records.
const DEFERRED_GATHER: std::time::Duration = std::time::Duration::from_millis(2);

/// The most lines queued at once; past it a line is written by the thread
/// that logged it, so a stalled stderr slows logging rather than growing
/// the queue without bound.
const DEFERRED_LIMIT: usize = 8192;

fn deferred_queue() -> Option<&'static DeferredQueue> {
    *DEFERRED.get_or_init(|| {
        let queue: &'static DeferredQueue = Box::leak(Box::new(DeferredQueue {
            queue: std::sync::Mutex::new(Deferred {
                lines: Vec::new(),
                writing: false,
            }),
            queued: std::sync::Condvar::new(),
            flushing: std::sync::Mutex::new(()),
        }));
        std::thread::Builder::new()
            .name("pintail-log".to_owned())
            .spawn(move || {
                loop {
                    {
                        let mut deferred = queue
                            .queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while deferred.lines.is_empty() {
                            // Cleared before every wait, not once per pass:
                            // a line written at once can flush the lines
                            // this writer was just woken for before it runs,
                            // and a writer that went back to waiting still
                            // marked awake would never be woken again - the
                            // next deferred line would sit in the queue
                            // until something else flushed it.
                            deferred.writing = false;
                            deferred = queue
                                .queued
                                .wait(deferred)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        deferred.writing = true;
                    }
                    std::thread::sleep(DEFERRED_GATHER);
                    flush_deferred();
                }
            })
            .ok()
            .map(|_| queue)
    })
}

/// Logs one line without writing it on the calling thread.
///
/// For the one line a server writes per statement it answers: written at
/// once it is a system call on the thread answering the statement, for
/// every statement, and that call was a sixth of what a trivial statement
/// cost the server. The line is queued, and a background thread writes
/// what has gathered every couple of milliseconds, in the order it was
/// logged. A line logged at once ([`emit_at`]) first writes everything
/// queued before it, so the two kinds never appear out of order, and a
/// process that logs its own shutdown leaves nothing queued behind.
///
/// Not for a line that reports a failure: a process that dies in the next
/// two milliseconds takes its queued lines with it.
pub fn emit_deferred(level: u8, message: String) {
    let Some(queue) = deferred_queue() else {
        emit_at(level, &message);
        return;
    };
    let mut deferred = queue
        .queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if deferred.lines.len() >= DEFERRED_LIMIT {
        drop(deferred);
        emit_at(level, &message);
        return;
    }
    deferred.lines.push((level, message));
    if !deferred.writing {
        deferred.writing = true;
        drop(deferred);
        queue.queued.notify_one();
    }
}

/// Writes every deferred line queued so far, in order, in one write.
pub fn flush_deferred() {
    use std::io::Write as _;
    let Some(Some(queue)) = DEFERRED.get() else {
        return;
    };
    // A sink that logs lands back here on the same thread, which already
    // holds the flush lock below. Its own line is written at once; the
    // lines this flush took are already on stderr.
    if IN_FLUSH.with(std::cell::Cell::get) {
        return;
    }
    // One flush at a time takes lines, writes them and hands them to the
    // sink, so two flushes cannot deliver their lines out of order to either.
    // The sink is called before the flush lock is let go: called after it, a
    // line written at once on another thread could reach the sink ahead of
    // the deferred lines this flush wrote before it. The queue lock is not
    // held, so a sink may queue deferred lines.
    let flushing = queue
        .flushing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lines = std::mem::take(
        &mut queue
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lines,
    );
    if lines.is_empty() {
        return;
    }
    let mut text = String::with_capacity(lines.iter().map(|(_, line)| line.len() + 9).sum());
    for (_, line) in &lines {
        text.push_str("pintail ");
        text.push_str(line);
        text.push('\n');
    }
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
    if let Some(sink) = SINK.get() {
        IN_FLUSH.with(|in_flush| in_flush.set(true));
        for (level, line) in &lines {
            sink(*level, line);
        }
        IN_FLUSH.with(|in_flush| in_flush.set(false));
    }
    drop(flushing);
}

std::thread_local! {
    /// Whether this thread is inside [`flush_deferred`]'s sink calls.
    static IN_FLUSH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Logs a failure. Always emitted.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::ERROR) {
            $crate::emit_at($crate::ERROR, &format!($($arg)*));
        }
    };
}

/// Logs a condition an operator should act on. Emitted at `warn` and below.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::WARN) {
            $crate::emit_at($crate::WARN, &format!($($arg)*));
        }
    };
}

/// Logs a lifecycle transition. Emitted at `info` and below.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::INFO) {
            $crate::emit_at($crate::INFO, &format!($($arg)*));
        }
    };
}

/// Logs a routine per-statement record through the background writer
/// ([`emit_deferred`]). Emitted at `info` and below.
#[macro_export]
macro_rules! log_info_deferred {
    ($($arg:tt)*) => {
        if $crate::enabled($crate::INFO) {
            $crate::emit_deferred($crate::INFO, format!($($arg)*));
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
            $crate::emit_at($crate::DEBUG, &format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::{DEBUG, ERROR, INFO, enabled};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Every line any test logs, as the sink received it. The sink is the
    /// process's one sink, so tests share it and each picks out its own lines
    /// by prefix.
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(_level: u8, message: &str) {
        SEEN.lock().unwrap().push(message.to_owned());
    }

    /// Installs [`record`]. Every test installs the same function, so
    /// whichever test runs first in the process wins and the rest find it in
    /// place.
    fn install_sink() {
        let _ = super::set_sink(record);
    }

    fn seen_with(prefix: &str) -> Vec<String> {
        SEEN.lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Waits until the sink has received `line`; panics with `what` if the
    /// background writer has not written it within ten seconds.
    fn wait_for(line: &str, what: &str) {
        let waited = Instant::now();
        while !SEEN.lock().unwrap().iter().any(|seen| seen == line) {
            assert!(waited.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    #[test]
    fn error_is_always_enabled() {
        // Whatever the environment says, a failure is reportable. The default
        // is info, so this holds without configuring anything.
        assert!(enabled(ERROR));
    }

    #[test]
    fn deferred_lines_reach_the_sink_in_order_and_before_a_line_written_at_once() {
        install_sink();
        for index in 0..50 {
            super::emit_deferred(INFO, format!("ordered {index}"));
        }
        // Written at once: everything queued before it goes first.
        super::emit_at(INFO, "ordered at-once");
        let mut expected = (0..50)
            .map(|index| format!("ordered {index}"))
            .collect::<Vec<_>>();
        expected.push("ordered at-once".to_owned());
        assert_eq!(seen_with("ordered "), expected);
        // And a deferred line with nothing after it is written by the
        // background writer on its own.
        super::emit_deferred(INFO, "ordered last".to_owned());
        wait_for("ordered last", "the background writer never wrote the line");
        expected.push("ordered last".to_owned());
        assert_eq!(seen_with("ordered "), expected);
    }

    #[test]
    fn the_writer_wakes_again_after_a_flush_took_the_lines_it_was_woken_for() {
        // Each round wakes the idle writer with a deferred line and then, on
        // this thread, writes a line at once - which flushes the queued line
        // before the writer has run. The writer wakes to an empty queue; it
        // must still be woken for the next deferred line, which nothing
        // else will ever flush.
        install_sink();
        for round in 0..200 {
            super::emit_deferred(INFO, format!("wake {round} queued"));
            super::emit_at(INFO, &format!("wake {round} at-once"));
            super::emit_deferred(INFO, format!("wake {round} last"));
            wait_for(
                &format!("wake {round} last"),
                &format!("round {round}: the background writer was never woken again"),
            );
            // Let the writer go back to waiting, so the next round's line
            // wakes it from there.
            std::thread::sleep(Duration::from_micros(300));
        }
    }

    #[test]
    fn lines_from_many_threads_all_reach_the_sink_each_thread_in_order() {
        // Threads mixing deferred lines with lines written at once: every
        // line reaches the sink, and each thread's lines reach it in the
        // order that thread logged them, whichever thread or the writer
        // happened to write them.
        install_sink();
        const THREADS: usize = 6;
        const LINES: usize = 3000;
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                scope.spawn(move || {
                    for index in 0..LINES {
                        let line = format!("mix {thread} {index}");
                        if index % 7 == 5 {
                            super::emit_at(INFO, &line);
                        } else {
                            super::emit_deferred(INFO, line);
                        }
                    }
                });
            }
        });
        for thread in 0..THREADS {
            wait_for(
                &format!("mix {thread} {}", LINES - 1),
                &format!("thread {thread}: its last deferred line was never written"),
            );
        }
        for thread in 0..THREADS {
            let expected = (0..LINES)
                .map(|index| format!("mix {thread} {index}"))
                .collect::<Vec<_>>();
            assert_eq!(
                seen_with(&format!("mix {thread} ")),
                expected,
                "thread {thread}'s lines reached the sink missing or out of order"
            );
        }
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
