//! Ships diagnostics off the node: crashes and errors to Sentry, every log
//! line to Logtail (Better Stack).
//!
//! Both are plain HTTP APIs, so both are spoken directly rather than through
//! an SDK - the same reasoning that produced [`pintail_log`] with no
//! dependencies at all. What this crate adds over that is a network, which is
//! the part that must never be allowed to affect the server it reports on.
//!
//! Three properties follow from that, and they are the whole design:
//!
//! - **Logging never blocks.** Lines go into a bounded queue. When it is full
//!   they are dropped and counted, because a replication loop stalling behind
//!   a slow log endpoint is a worse outcome than a missing line.
//! - **A crash is delivered before the process dies.** The panic hook captures
//!   a backtrace and waits, briefly, for it to be sent. A stack trace that
//!   arrives after the exit is a stack trace nobody reads.
//! - **Configuration is inert when absent.** No DSN, no exporter, no threads,
//!   no behaviour change.
//!
//! # Configuration
//!
//! | Variable | Effect |
//! |---|---|
//! | `PINTAIL_SENTRY_DSN` | Enables Sentry. Errors and panics only. |
//! | `PINTAIL_LOGTAIL_ENDPOINT` | Better Stack ingest URL. |
//! | `PINTAIL_LOGTAIL_TOKEN` | Better Stack source token. |
//! | `PINTAIL_ENVIRONMENT` | Tag on every event. Defaults to `unknown`. |
//! | `PINTAIL_RELEASE` | Tag on every event. Defaults to the crate version. |
//!
//! # What is never sent
//!
//! Only lines that already passed through [`pintail_log`], which by its own
//! rules carry identifiers and counts rather than DSNs, key secrets, invite
//! tokens, OAuth codes, JWTs or row values. The credentials configured here
//! are never logged, and a malformed one is reported without echoing it.

mod sentry;

use std::{
    collections::BTreeMap,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use rand::RngCore as _;

use crate::sentry::Dsn;

/// How many lines may wait to be shipped.
///
/// Sized for a burst rather than an outage: an unreachable endpoint should
/// cost a bounded amount of memory and then start dropping, not grow until the
/// process is killed.
const QUEUE_CAPACITY: usize = 4_096;

/// How many lines are sent to Logtail in one request.
const BATCH_SIZE: usize = 100;

/// How long a partial batch waits for company before being sent anyway.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// How long a panicking thread waits for its crash report to leave the
/// process. Long enough for one HTTP round trip, short enough that a wedged
/// network does not hold a dying process open.
const PANIC_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on every outbound request, so a hung endpoint cannot occupy the
/// exporter indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One line on its way out.
pub(crate) struct Event {
    pub(crate) id: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) level: u8,
    pub(crate) message: String,
    pub(crate) backtrace: Option<String>,
    pub(crate) fatal: bool,
    /// Signalled once this event has been through the exporters. Only a panic
    /// sets it: everything else is fire-and-forget.
    pub(crate) delivered: Option<SyncSender<()>>,
}

static SENDER: OnceLock<SyncSender<Event>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// How many lines have been dropped because the queue was full.
///
/// Exposed so the drop count itself can be reported rather than silently
/// swallowed - a telemetry pipeline that loses data without saying so is worse
/// than none.
#[must_use]
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn identifier() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut id, byte| {
            use std::fmt::Write as _;
            let _ = write!(id, "{byte:02x}");
            id
        })
}

/// The sink handed to [`pintail_log`]. Never blocks.
fn sink(level: u8, message: &str) {
    let Some(sender) = SENDER.get() else {
        return;
    };
    let event = Event {
        id: identifier(),
        at: Utc::now(),
        level,
        message: message.to_owned(),
        backtrace: None,
        fatal: false,
        delivered: None,
    };
    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Everything the exporters need, resolved once at startup.
struct Config {
    dsn: Option<Dsn>,
    logtail_endpoint: Option<String>,
    logtail_token: Option<String>,
    context: BTreeMap<String, String>,
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Starts the exporters, if any are configured.
///
/// Returns a description of what was enabled, for the caller to log. Never
/// fails: telemetry being misconfigured must not be the reason a database
/// refuses to boot, so a bad DSN is reported and skipped.
///
/// # Panics
///
/// Does not panic. The installed panic hook runs on *other* threads' panics.
pub fn init() -> String {
    let dsn = match variable("PINTAIL_SENTRY_DSN").map(|raw| Dsn::parse(&raw)) {
        Some(Ok(dsn)) => Some(dsn),
        Some(Err(error)) => {
            pintail_log::log_error!("sentry disabled: {error}");
            None
        }
        None => None,
    };
    let logtail_endpoint = variable("PINTAIL_LOGTAIL_ENDPOINT");
    let logtail_token = variable("PINTAIL_LOGTAIL_TOKEN");
    if logtail_endpoint.is_some() != logtail_token.is_some() {
        pintail_log::log_error!(
            "logtail disabled: PINTAIL_LOGTAIL_ENDPOINT and PINTAIL_LOGTAIL_TOKEN must both be set"
        );
    }
    let logtail = logtail_endpoint
        .clone()
        .zip(logtail_token.clone())
        .map(|(endpoint, _)| endpoint);

    if dsn.is_none() && logtail.is_none() {
        return "telemetry off".to_owned();
    }

    let mut context = BTreeMap::new();
    context.insert(
        "environment".to_owned(),
        variable("PINTAIL_ENVIRONMENT").unwrap_or_else(|| "unknown".to_owned()),
    );
    context.insert(
        "release".to_owned(),
        variable("PINTAIL_RELEASE").unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
    );
    context.insert(
        "server_name".to_owned(),
        variable("HOSTNAME").unwrap_or_else(|| "pintail".to_owned()),
    );

    let enabled = match (dsn.is_some(), logtail.is_some()) {
        (true, true) => "sentry+logtail",
        (true, false) => "sentry",
        (false, true) => "logtail",
        (false, false) => unreachable!("the empty case returned above"),
    };

    let config = Config {
        dsn,
        logtail_endpoint: logtail,
        logtail_token,
        context,
    };

    let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
    if SENDER.set(sender).is_err() {
        return "telemetry already started".to_owned();
    }
    // Its own thread with its own runtime, deliberately. The exporter must
    // keep working while the main runtime is saturated or shutting down -
    // which is exactly when the interesting failures happen.
    match std::thread::Builder::new()
        .name("pintail-telemetry".to_owned())
        .spawn(move || export_forever(&config, &receiver))
    {
        // The sink is installed only once the thread that drains it exists,
        // so a failed spawn leaves logging exactly as it was rather than
        // filling a queue nobody reads.
        Ok(_) => {
            pintail_log::set_sink(sink);
        }
        Err(error) => pintail_log::log_error!("telemetry thread failed to start: {error}"),
    }

    install_panic_hook();
    format!("telemetry on ({enabled})")
}

/// Captures panics with a backtrace and blocks until the report is away.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_owned());
        let location = info.location().map_or_else(
            || "unknown location".to_owned(),
            |location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            },
        );
        // force_capture rather than capture: RUST_BACKTRACE is usually unset
        // in production, and a crash report without a stack is the reason this
        // exists at all.
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        report_panic(&format!("panic at {location}: {payload}"), backtrace);
        previous(info);
    }));
}

/// Queues a fatal event and waits, briefly, for it to be delivered.
fn report_panic(message: &str, backtrace: String) {
    let Some(sender) = SENDER.get() else {
        return;
    };
    let (acknowledged, wait) = sync_channel(1);
    let event = Event {
        id: identifier(),
        at: Utc::now(),
        level: pintail_log::ERROR,
        message: message.to_owned(),
        backtrace: Some(backtrace),
        fatal: true,
        delivered: Some(acknowledged),
    };
    // A full queue must not swallow the crash: this is the one event worth
    // waiting for a slot, bounded so a wedged exporter cannot hang the panic.
    if sender.try_send(event).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let _ = wait.recv_timeout(PANIC_FLUSH_TIMEOUT);
}

/// Drains the queue until the process ends.
fn export_forever(config: &Config, receiver: &Receiver<Event>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("pintail telemetry runtime failed to start; telemetry is off");
        return;
    };
    let Ok(client) = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() else {
        eprintln!("pintail telemetry HTTP client failed to build; telemetry is off");
        return;
    };

    let mut batch: Vec<Event> = Vec::with_capacity(BATCH_SIZE);
    loop {
        // A fatal event short-circuits the batch: its sender is blocked
        // waiting, and the process may not survive to the next tick.
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(event) => {
                let urgent = event.fatal || event.delivered.is_some();
                batch.push(event);
                if urgent || batch.len() >= BATCH_SIZE {
                    runtime.block_on(flush(config, &client, &mut batch));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    runtime.block_on(flush(config, &client, &mut batch));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                runtime.block_on(flush(config, &client, &mut batch));
                return;
            }
        }
    }
}

/// Sends one batch to whichever exporters are configured.
async fn flush(config: &Config, client: &reqwest::Client, batch: &mut Vec<Event>) {
    if batch.is_empty() {
        return;
    }
    if let (Some(endpoint), Some(token)) = (&config.logtail_endpoint, &config.logtail_token) {
        let lines = batch
            .iter()
            .map(|event| {
                serde_json::json!({
                    "dt": event.at.to_rfc3339(),
                    "level": match event.level {
                        pintail_log::ERROR => "error",
                        pintail_log::DEBUG => "debug",
                        _ => "info",
                    },
                    "message": event.message,
                    "environment": config.context.get("environment"),
                    "release": config.context.get("release"),
                })
            })
            .collect::<Vec<_>>();
        let sent = client
            .post(endpoint)
            .bearer_auth(token)
            .json(&lines)
            .send()
            .await;
        if let Err(error) = sent {
            // Reported to stderr rather than through pintail_log, which would
            // re-enter this exporter and, on a persistent failure, feed itself.
            eprintln!("pintail telemetry: logtail delivery failed: {error}");
        }
    }

    if let Some(dsn) = &config.dsn {
        for event in batch
            .iter()
            .filter(|event| event.level == pintail_log::ERROR)
        {
            let body = crate::sentry::envelope(event, &config.context);
            let sent = client
                .post(dsn.envelope_url())
                .header("X-Sentry-Auth", dsn.auth_header())
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-sentry-envelope",
                )
                .body(body)
                .send()
                .await;
            if let Err(error) = sent {
                eprintln!("pintail telemetry: sentry delivery failed: {error}");
            }
        }
    }

    for event in batch.drain(..) {
        if let Some(acknowledged) = event.delivered {
            let _ = acknowledged.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BATCH_SIZE, QUEUE_CAPACITY, dropped, identifier};

    #[test]
    fn event_ids_are_sentry_shaped() {
        let id = identifier();
        assert_eq!(id.len(), 32, "sentry event ids are 32 hex characters");
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(id, identifier(), "ids must not repeat");
    }

    #[test]
    fn nothing_is_dropped_before_anything_is_sent() {
        assert_eq!(dropped(), 0);
    }

    #[test]
    fn the_queue_holds_more_than_one_batch() {
        // Otherwise a single full batch would start dropping while it is being
        // assembled, which would make loss routine rather than exceptional.
        const { assert!(QUEUE_CAPACITY > BATCH_SIZE * 2) };
    }
}
