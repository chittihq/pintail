//! End-to-end: a log line and a panic, against stand-ins for Sentry and
//! Logtail.
//!
//! Both exporters are only ever exercised against someone else's production
//! service, which is not a thing a test can do - so these speak the same HTTP
//! the real endpoints do and assert on what actually left the process. The
//! panic case is the one that matters: a crash report that arrives without a
//! stack trace, or after the process has exited, is the failure this whole
//! crate exists to prevent.

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::mpsc::{Sender, channel},
    time::Duration,
};

/// One captured request.
struct Captured {
    path: String,
    headers: Vec<String>,
    body: String,
}

/// Serves plain HTTP until the test drops it, forwarding what it received.
fn capture_server(sink: Sender<Captured>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind capture server");
    let address = listener.local_addr().expect("capture address").to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let sink = sink.clone();
            // One thread per connection, each keeping the connection open for
            // further requests. Closing after every response would leave a
            // stale entry in the client's pool, and the retry that follows
            // costs a request timeout - which turned a three-second test into
            // a thirty-five-second one.
            std::thread::spawn(move || {
                while let Some(captured) = serve_once(&stream) {
                    if sink.send(captured).is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{address}")
}

fn serve_once(stream: &TcpStream) -> Option<Captured> {
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_owned();

    let mut headers = Vec::new();
    let mut length = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        headers.push(line);
    }

    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).ok()?;
    writer
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
        .ok()?;
    let _ = writer.flush();
    Some(Captured {
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Everything is driven through one test: `init` reads the environment once
/// per process and installs a global sink, so a second test in the same binary
/// would configure nothing and assert on someone else's exporter.
#[test]
fn errors_and_panics_reach_both_exporters_with_a_stack_trace() {
    let (sentry_tx, sentry_rx) = channel();
    let (logtail_tx, logtail_rx) = channel();
    let sentry = capture_server(sentry_tx);
    let logtail = capture_server(logtail_tx);

    // A DSN shaped exactly like Sentry's, pointed at the stand-in. The scheme
    // and project id are what the parser turns into an envelope URL.
    unsafe {
        std::env::set_var(
            "PINTAIL_SENTRY_DSN",
            format!("{}/7", sentry.replace("http://", "http://publickey@")),
        );
        std::env::set_var("PINTAIL_LOGTAIL_ENDPOINT", &logtail);
        std::env::set_var("PINTAIL_LOGTAIL_TOKEN", "logtail-test-token");
        std::env::set_var("PINTAIL_ENVIRONMENT", "test-suite");
        std::env::set_var("PINTAIL_RELEASE", "0.0.0-test");
    }

    let summary = pintail_telemetry::init();
    assert!(
        summary.contains("sentry") && summary.contains("logtail"),
        "both exporters should be enabled: {summary}",
    );

    pintail_log::log_error!("replication stalled on table orders");

    // Logtail receives every level, as a batched JSON array.
    let batch = logtail_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("logtail received nothing");
    assert!(
        batch
            .headers
            .iter()
            .any(|header| header.eq_ignore_ascii_case("authorization: Bearer logtail-test-token")),
        "logtail request must authenticate: {:?}",
        batch.headers,
    );
    assert!(batch.body.starts_with('['), "logtail takes a JSON array");
    assert!(batch.body.contains("replication stalled on table orders"));
    assert!(batch.body.contains("\"level\":\"error\""));
    assert!(batch.body.contains("test-suite"), "environment is tagged");

    // Sentry receives the error as an envelope, on the derived URL.
    let envelope = sentry_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("sentry received nothing");
    assert_eq!(envelope.path, "/api/7/envelope/");
    assert!(
        envelope.headers.iter().any(|header| header
            .to_ascii_lowercase()
            .starts_with("x-sentry-auth:")
            && header.contains("sentry_key=publickey")),
        "sentry request must carry its auth header: {:?}",
        envelope.headers,
    );
    assert!(
        envelope
            .body
            .contains("replication stalled on table orders")
    );

    // The crash path. A panic on another thread must produce a fatal event
    // carrying real frames, and must be delivered before the panic returns.
    let panicked = std::thread::spawn(|| panic!("deliberate test panic"));
    assert!(panicked.join().is_err(), "the thread should have panicked");

    let crash = sentry_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("sentry received no crash report");
    assert!(
        crash.body.contains("\"level\":\"fatal\""),
        "a panic is fatal, not an ordinary error",
    );
    assert!(crash.body.contains("deliberate test panic"));
    assert!(
        crash.body.contains("\"type\":\"panic\""),
        "the exception must be typed as a panic: {}",
        crash.body,
    );
    // The whole point: frames, not just a message.
    assert!(
        crash.body.contains("\"stacktrace\""),
        "a crash report must carry a stack trace",
    );
    assert!(
        crash.body.contains("\"function\""),
        "the stack trace must contain parsed frames: {}",
        crash.body,
    );
    // And the raw text alongside them, so a future backtrace format change
    // degrades to something still readable rather than to nothing.
    assert!(crash.body.contains("backtrace"));
}
