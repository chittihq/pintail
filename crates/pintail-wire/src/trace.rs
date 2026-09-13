//! Per-statement phase timing, for measurement runs.
//!
//! `PINTAIL_QUERY_TRACE_JSON=<file>` writes the same phases as a Chrome
//! trace instead: open the file in `ui.perfetto.dev` or `chrome://tracing`
//! and each statement is a span with its phases nested inside, one row per
//! worker thread so concurrent statements read as concurrent. The array is
//! left unterminated on purpose, so a trace is readable from a server that
//! is still running. Both variables can be set at once.
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
        .get_or_init(|| open_writer("PINTAIL_QUERY_TRACE", None))
        .as_ref()
}

/// The Chrome trace writer, for `PINTAIL_QUERY_TRACE_JSON`.
///
/// The file opens with `[` and every statement appends one event object and
/// a comma. The array is deliberately never closed: a trace is read after
/// the fact from a server that may still be running, and both
/// `ui.perfetto.dev` and `chrome://tracing` accept an unterminated array, so
/// leaving it open costs nothing and needing a clean shutdown to get a
/// readable trace would cost the cases worth tracing.
fn json_writer() -> Option<&'static mpsc::Sender<String>> {
    static WRITER: OnceLock<Option<mpsc::Sender<String>>> = OnceLock::new();
    WRITER
        .get_or_init(|| open_writer("PINTAIL_QUERY_TRACE_JSON", Some("[\n")))
        .as_ref()
}

/// When this process started tracing, so every statement's events land on one
/// timeline rather than each starting from zero.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn open_writer(variable: &str, opening: Option<&str>) -> Option<mpsc::Sender<String>> {
    {
        let path = std::env::var_os(variable).filter(|path| !path.is_empty())?;
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
        if let Some(opening) = opening {
            let _ = sender.send(opening.to_owned());
        }
        Some(sender)
    }
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
        // Anchors the shared timeline on the first traced statement.
        let _ = epoch();
        (writer().is_some() || json_writer().is_some()).then(|| Self {
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
        let hash = statement_hash(sql);
        if let Some(writer) = writer() {
            let mut line = format!("sql={hash:016x}");
            for (phase, micros) in &self.marks {
                let _ = write!(line, "\t{phase}={micros}");
            }
            for (key, value) in &self.labels {
                let _ = write!(line, "\t{key}={value}");
            }
            line.push('\n');
            let _ = writer.send(line);
        }
        if let Some(writer) = json_writer() {
            let _ = writer.send(self.chrome_events(hash));
        }
    }

    /// The statement as Chrome trace events: one span covering it, and one
    /// per phase nested inside by time containment.
    ///
    /// A phase is recorded by when it ENDED, so a span runs from the previous
    /// phase's end to this one's - which is what turns a list of instants
    /// into the durations a timeline draws.
    fn chrome_events(&self, hash: u64) -> String {
        let base = self.received.saturating_duration_since(epoch()).as_micros();
        let total = self.marks.last().map_or(0, |(_, micros)| *micros);
        // One row per worker thread, so statements running at the same time
        // are visibly concurrent rather than stacked on one line.
        let thread = thread_row();
        let mut out = String::with_capacity(128 * (self.marks.len() + 1));
        let mut args = String::from("{");
        for (index, (key, value)) in self.labels.iter().enumerate() {
            if index > 0 {
                args.push(',');
            }
            let _ = write!(args, "\"{key}\":\"{}\"", escape(value));
        }
        args.push('}');
        let _ = write!(
            out,
            "{{\"name\":\"stmt {hash:016x}\",\"cat\":\"statement\",\"ph\":\"X\",\"pid\":1,\"tid\":{thread},\"ts\":{base},\"dur\":{total},\"args\":{args}}},"
        );
        out.push('\n');
        let mut previous = 0_u128;
        for (phase, micros) in &self.marks {
            let _ = write!(
                out,
                "{{\"name\":\"{phase}\",\"cat\":\"phase\",\"ph\":\"X\",\"pid\":1,\"tid\":{thread},\"ts\":{},\"dur\":{}}},",
                base + previous,
                micros.saturating_sub(previous)
            );
            out.push('\n');
            previous = *micros;
        }
        out
    }
}

/// A small stable number for this worker thread, so a timeline puts
/// concurrent statements on separate rows.
fn thread_row() -> u64 {
    thread_local! {
        static ROW: OnceLock<u64> = const { OnceLock::new() };
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    ROW.with(|row| *row.get_or_init(|| NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
}

/// Escapes a label value for the JSON string it is written into.
fn escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            control if control.is_control() => vec![' '],
            other => vec![other],
        })
        .collect()
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

    /// The phase marks record when a phase ENDED, so the events have to run
    /// from the previous end to this one. Getting that wrong draws every
    /// span from zero and the timeline says nothing.
    #[test]
    fn phases_become_spans_between_consecutive_marks() {
        let mut trace = super::Trace {
            received: std::time::Instant::now(),
            marks: vec![("parse", 10), ("plan", 30), ("execute", 100)],
            labels: vec![("path", "columnar".to_owned())],
        };
        trace.marks.sort_by_key(|(_, micros)| *micros);
        let events = trace.chrome_events(0xabcd);

        // One statement span plus one per phase, each a complete event.
        assert_eq!(events.matches("\"ph\":\"X\"").count(), 4);
        assert!(events.contains("\"name\":\"stmt 000000000000abcd\""));
        assert!(events.contains("\"path\":\"columnar\""));
        // parse ends at 10 having started at 0; plan runs 10..30; execute
        // 30..100; and the statement covers the whole 100. The absolute `ts`
        // and the thread row depend on what else the process has traced, so
        // the durations are the claim.
        let durations = events
            .match_indices("\"dur\":")
            .map(|(at, _)| {
                events[at + 6..]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(durations, ["100", "10", "20", "70"], "{events}");
    }

    /// A label carrying a quote must not break the object it sits in.
    #[test]
    fn a_label_is_escaped_into_its_json_string() {
        let trace = super::Trace {
            received: std::time::Instant::now(),
            marks: vec![("execute", 5)],
            labels: vec![("sql", "he said \"hi\"".to_owned())],
        };
        let events = trace.chrome_events(1);
        assert!(events.contains("he said \\\"hi\\\""), "{events}");
    }
}
