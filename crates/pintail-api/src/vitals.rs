//! Live process vitals: CPU, resident memory, and query throughput.
//!
//! Streamed rather than polled. At one sample per second an HTTP request per
//! sample would cost a connection setup, an auth check and a metadata open
//! every second per viewer; one SSE stream costs those once.
//!
//! Every figure describes *this process*, not the host. In a container the
//! host's totals are the wrong denominator - a 64GB machine running Pintail
//! under a 4GB cap should read as busy at 3.5GB, not as 5% used - so the
//! limits come from the cgroup when there is one.

use std::{
    convert::Infallible,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Extension,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, stream};
use serde::Serialize;

use crate::{ApiState, auth::AuthPrincipal, error::ApiError};

/// How often a sample is produced. The dashboard draws a moving window, so
/// this is also its resolution.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// One reading.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Vitals {
    /// Percent of one core's worth of time, normalised by the cores this
    /// process may actually use. 100 means fully using its allowance.
    pub(crate) cpu_percent: f64,
    pub(crate) memory_bytes: u64,
    /// The cap this process is held to, when one exists. `None` on a host with
    /// no cgroup memory limit, where a percentage would be meaningless.
    pub(crate) memory_limit_bytes: Option<u64>,
    pub(crate) queries_per_second: f64,
    /// Cumulative, so a client that missed samples can still see totals.
    pub(crate) queries_total: u64,
}

/// What the previous sample saw, so rates can be differences rather than
/// guesses. CPU time and query counts are both monotonic counters; a rate only
/// exists between two readings of them.
struct Previous {
    at: Instant,
    cpu_seconds: f64,
    queries: u64,
}

static PREVIOUS: Mutex<Option<Previous>> = Mutex::new(None);

/// Takes one reading, relative to the last.
///
/// The first call after startup has nothing to compare against and reports
/// zero rates rather than inventing them from process start, which would show
/// a meaningless average over the whole uptime.
pub(crate) fn sample(state: &ApiState) -> Vitals {
    let queries = state.runtime_metrics().queries;
    let cpu_seconds = process_cpu_seconds();
    let now = Instant::now();

    let mut guard = PREVIOUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (cpu_percent, queries_per_second) = guard.as_ref().map_or((0.0, 0.0), |previous| {
        let elapsed = now.duration_since(previous.at).as_secs_f64();
        if elapsed <= 0.0 {
            return (0.0, 0.0);
        }
        let cores = available_cores();
        let busy = (cpu_seconds - previous.cpu_seconds).max(0.0);
        (
            (busy / elapsed / cores * 100.0).clamp(0.0, 100.0),
            f64::from(u32::try_from(queries.saturating_sub(previous.queries)).unwrap_or(u32::MAX))
                / elapsed,
        )
    });
    *guard = Some(Previous {
        at: now,
        cpu_seconds,
        queries,
    });
    drop(guard);

    Vitals {
        cpu_percent,
        memory_bytes: resident_bytes(),
        memory_limit_bytes: memory_limit_bytes(),
        queries_per_second,
        queries_total: queries,
    }
}

/// Total CPU seconds this process has consumed, user plus system.
#[cfg(target_os = "linux")]
fn process_cpu_seconds() -> f64 {
    // Fields 14 and 15 of /proc/self/stat, in clock ticks. The comm field can
    // contain spaces and parentheses, so everything up to the last ')' is
    // skipped rather than split on whitespace from the start.
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return 0.0;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After the comm field, index 0 is `state`, so utime is 11 and stime 12.
    let ticks = fields
        .get(11)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
        + fields
            .get(12)
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
    ticks / 100.0
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_seconds() -> f64 {
    // Development only. `ps` reports cumulative CPU time as [[dd-]hh:]mm:ss.
    std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|elapsed| {
            elapsed
                .trim()
                .split(':')
                .filter_map(|part| part.trim().parse::<f64>().ok())
                .fold(0.0, |total, part| total * 60.0 + part)
        })
        .map_or(0.0, |seconds| seconds)
}

/// Cores this process may use: the cgroup quota when one is set, otherwise
/// every core on the machine.
fn available_cores() -> f64 {
    #[cfg(target_os = "linux")]
    if let Ok(quota) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut parts = quota.split_whitespace();
        if let (Some(limit), Some(period)) = (parts.next(), parts.next())
            && limit != "max"
            && let (Ok(limit), Ok(period)) = (limit.parse::<f64>(), period.parse::<f64>())
            && period > 0.0
        {
            return (limit / period).max(1.0);
        }
    }
    // A core count never approaches the mantissa limit; the cast is exact.
    std::thread::available_parallelism().map_or(1.0, |cores| {
        f64::from(u32::try_from(cores.get()).unwrap_or(1))
    })
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> u64 {
    // Second field of /proc/self/statm, in pages. Read rather than shelled
    // out: at one sample per second, spawning `ps` would fork this process
    // 86,400 times a day.
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| {
            statm
                .split_whitespace()
                .nth(1)
                .and_then(|pages| pages.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages.saturating_mul(4096))
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|rss| rss.trim().parse::<u64>().ok())
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
        .unwrap_or(0)
}

/// The memory ceiling this process is held to, if any.
fn memory_limit_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok());
        if limit.is_some() {
            return limit;
        }
    }
    None
}

/// Streams one sample per second for as long as the client stays connected.
pub(crate) async fn stream(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    principal.require_scope("read")?;
    let stream = stream::unfold(state, |state| async move {
        tokio::time::sleep(SAMPLE_INTERVAL).await;
        let event = Event::default()
            .event("vitals")
            .json_data(sample(&state))
            .unwrap_or_else(|error| Event::default().event("error").data(error.to_string()));
        Some((Ok(event), state))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::{available_cores, process_cpu_seconds, resident_bytes};

    #[test]
    fn the_process_reports_its_own_footprint() {
        // Exact values are the operating system's business; that they are
        // plausible is this crate's.
        assert!(resident_bytes() > 0, "a running process occupies memory");
        assert!(
            process_cpu_seconds() >= 0.0,
            "cpu time is a monotonic counter",
        );
        assert!(available_cores() >= 1.0, "at least one core is usable");
    }
}
