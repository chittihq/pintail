//! Once-per-second cooperative cancellation under process or query-budget pressure.
use pintail_exec::{cancel_query_under_memory_pressure, shared_memory_budget};
use std::time::{Duration, Instant};

const CANCELLATION_GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct CancellationCadence {
    next_allowed: Option<Instant>,
}

impl CancellationCadence {
    fn try_cancel(
        &mut self,
        now: Instant,
        cancel: impl FnOnce() -> Option<usize>,
    ) -> Option<usize> {
        if self.next_allowed.is_some_and(|deadline| now < deadline) {
            return None;
        }
        let victim = cancel()?;
        self.next_allowed = Some(now + CANCELLATION_GRACE);
        Some(victim)
    }
}

/// Runs until server shutdown. Memory sampling uses a blocking worker so a
/// platform process lookup cannot stall the async reactor.
#[must_use]
pub fn spawn(mut shutdown: tokio::sync::broadcast::Receiver<()>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let process_limit = crate::config::available_memory_bytes()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let mut cadence = CancellationCadence::default();
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
                    let victim = cadence.try_cancel(Instant::now(), || {
                        resident.and_then(|used| cancel_query_under_memory_pressure(used, process_limit))
                            .or_else(|| cancel_query_under_memory_pressure(budget.used(), budget.limit()))
                    });
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

#[cfg(test)]
mod tests {
    use super::{CANCELLATION_GRACE, CancellationCadence};
    use std::time::{Duration, Instant};

    #[test]
    fn sustained_pressure_gives_each_victim_time_to_release_memory() {
        let mut cadence = CancellationCadence::default();
        let start = Instant::now();
        let mut cancellations = 0;
        for second in 0..=10 {
            let victim = cadence.try_cancel(start + Duration::from_secs(second), || {
                cancellations += 1;
                Some(1_000)
            });
            assert_eq!(victim.is_some(), second % CANCELLATION_GRACE.as_secs() == 0);
        }
        assert_eq!(cancellations, 3);
    }
    #[test]
    fn empty_samples_do_not_delay_the_first_victim_or_reset_the_grace() {
        let mut cadence = CancellationCadence::default();
        let start = Instant::now();
        assert_eq!(cadence.try_cancel(start, || None), None);
        assert_eq!(cadence.try_cancel(start, || Some(100)), Some(100));
        assert_eq!(
            cadence.try_cancel(start + Duration::from_secs(1), || None),
            None
        );
        assert_eq!(
            cadence.try_cancel(start + Duration::from_secs(2), || Some(50)),
            None
        );
        assert_eq!(
            cadence.try_cancel(start + CANCELLATION_GRACE, || Some(50)),
            Some(50)
        );
    }
}
