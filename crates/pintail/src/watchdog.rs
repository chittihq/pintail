//! Once-per-second cooperative cancellation under process or query-budget pressure.
use pintail_exec::{cancel_query_under_memory_pressure, shared_memory_budget};
use std::time::Duration;

/// Runs until server shutdown. Memory sampling uses a blocking worker so a
/// platform process lookup cannot stall the async reactor.
#[must_use]
pub fn spawn(mut shutdown: tokio::sync::broadcast::Receiver<()>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let process_limit = crate::config::available_memory_bytes()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let mut ticks = tokio::time::interval(Duration::from_secs(1));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = ticks.tick() => {
                    let resident = tokio::task::spawn_blocking(resident_bytes).await.ok().flatten();
                    let budget = shared_memory_budget();
                    // Short-circuit: at most one victim per tick, even when
                    // both the process and operator budgets are under pressure.
                    let victim = resident.and_then(|used| cancel_query_under_memory_pressure(used, process_limit))
                        .or_else(|| cancel_query_under_memory_pressure(budget.used(), budget.limit()));
                    if let Some(bytes) = victim {
                        pintail_log::log_info!("memory.watchdog cancelled one query: tracked_bytes={bytes} resident_bytes={resident:?} process_limit={process_limit} query_budget_used={} query_budget_limit={}", budget.used(), budget.limit());
                    }
                }
            }
        }
    })
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<usize> {
    // VmRSS is already in KiB; do not assume the kernel page size is 4096.
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|v| v.split_whitespace().next()?.parse::<usize>().ok())
        })?
        .checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<usize> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()?
        .checked_mul(1024)
}
