//! Per-statement phase timing, for measurement runs.
//!
//! `PINTAIL_QUERY_TRACE=<file>` appends one line per statement the engine
//! answers on the wire: a hash of the statement text, the time each phase
//! ended at in microseconds since the statement arrived, the path it took,
//! and how many values it materialized. Lines reach the file through a
//! background writer, so tracing puts no I/O on the statement itself. Unset,
//! a statement pays one check for whether tracing is on.
//!
//! The statement text itself is never written: the hash joins a line to a
//! benchmark case, and a literal in the text can be a row value.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn writer() -> Option<&'static mpsc::Sender<String>> {
    static WRITER: OnceLock<Option<mpsc::Sender<String>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let path = std::env::var_os("PINTAIL_QUERY_TRACE").filter(|path| !path.is_empty())?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()?;
            let (sender, receiver) = mpsc::channel::<String>();
            std::thread::Builder::new()
                .name("pintail-trace".to_owned())
                .spawn(move || {
                    let mut out = std::io::BufWriter::new(file);
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(200)) {
                            Ok(line) => {
                                let _ = out.write_all(line.as_bytes());
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                let _ = out.flush();
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                let _ = out.flush();
                                return;
                            }
                        }
                    }
                })
                .ok()?;
            Some(sender)
        })
        .as_ref()
}

/// One statement's trace, carried from its arrival to its encoded response.
#[derive(Debug)]
pub(crate) struct Trace {
    received: Instant,
    marks: Vec<(&'static str, u128)>,
    labels: Vec<(&'static str, String)>,
}

impl Trace {
    /// A trace for a statement that arrived at `received`, when tracing is on.
    pub(crate) fn start(received: Instant) -> Option<Self> {
        writer().is_some().then(|| Self {
            received,
            marks: Vec::with_capacity(16),
            labels: Vec::with_capacity(8),
        })
    }

    /// Records that `phase` ended now.
    pub(crate) fn mark(&mut self, phase: &'static str) {
        self.marks
            .push((phase, self.received.elapsed().as_micros()));
    }

    /// Records a fact about the statement's path.
    pub(crate) fn label(&mut self, key: &'static str, value: impl std::fmt::Display) {
        self.labels.push((key, value.to_string()));
    }

    /// Writes the line for the statement `sql`.
    pub(crate) fn finish(self, sql: &str) {
        let Some(writer) = writer() else {
            return;
        };
        let mut line = format!("sql={:016x}", statement_hash(sql));
        for (phase, micros) in &self.marks {
            let _ = write!(line, "\t{phase}={micros}");
        }
        for (key, value) in &self.labels {
            let _ = write!(line, "\t{key}={value}");
        }
        line.push('\n');
        let _ = writer.send(line);
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

/// Makes `trace` the statement's trace on this worker thread.
pub(crate) fn install(trace: Option<Trace>) {
    CURRENT.with(|current| *current.borrow_mut() = trace);
}

/// Takes this thread's trace back.
pub(crate) fn take() -> Option<Trace> {
    CURRENT.with(|current| current.borrow_mut().take())
}

/// Marks the end of `phase` on this thread's trace, if there is one.
pub(crate) fn mark(phase: &'static str) {
    CURRENT.with(|current| {
        if let Some(trace) = current.borrow_mut().as_mut() {
            trace.mark(phase);
        }
    });
}

/// Labels this thread's trace, if there is one.
pub(crate) fn label(key: &'static str, value: impl std::fmt::Display) {
    CURRENT.with(|current| {
        if let Some(trace) = current.borrow_mut().as_mut() {
            trace.label(key, value);
        }
    });
}

/// Records the executor's value-at-a-time counts on this thread's trace.
pub(crate) fn label_exec_counters() {
    let counters = pintail_exec::take_exec_counters();
    label("materialized", counters.values_materialized);
    label("scalar_rows", counters.rows_projected_scalar);
    label("sorted_rows", counters.rows_sorted);
    label("regathered", counters.cells_regathered);
}

/// FNV-1a over the statement's bytes: stable across builds, so a
/// measurement harness can compute it for the text it sent.
pub(crate) fn statement_hash(sql: &str) -> u64 {
    sql.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_statement_hash_is_fnv_1a() {
        assert_eq!(super::statement_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(super::statement_hash("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
