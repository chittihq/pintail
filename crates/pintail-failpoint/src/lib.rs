//! Deterministic recovery faults, absent from ordinary builds.
//!
//! With `failpoints`, `PINTAIL_FAILPOINT` contains comma-separated
//! `site[@nth][=abort|error]` entries. Each site fires once per process.

/// Visits a recovery boundary. Ordinary builds never inspect the environment.
///
/// # Errors
/// Returns an I/O error on the configured hit when the action is `error`.
#[inline]
pub fn hit(site: &'static str) -> std::io::Result<()> {
    #[cfg(feature = "failpoints")]
    {
        enabled::hit(site)
    }
    #[cfg(not(feature = "failpoints"))]
    {
        let _ = site;
        Ok(())
    }
}

#[cfg(any(feature = "failpoints", test))]
mod enabled {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Action {
        Abort,
        Error,
    }

    struct Fault {
        nth: u32,
        action: Action,
        hits: AtomicU32,
    }

    impl Fault {
        fn visit(&self) -> Option<Action> {
            // Saturation avoids rearming after u32::MAX visits.
            let prior = self
                .hits
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .ok()?;
            (prior + 1 == self.nth).then_some(self.action)
        }
    }

    fn parse(input: &str) -> Result<BTreeMap<String, Fault>, String> {
        let mut faults = BTreeMap::new();
        for entry in input
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (target, action) = entry.split_once('=').unwrap_or((entry, "abort"));
            let action = match action {
                "abort" => Action::Abort,
                "error" => Action::Error,
                _ => return Err(format!("invalid failpoint action: {entry}")),
            };
            let (site, nth) = target.split_once('@').unwrap_or((target, "1"));
            let nth = nth
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("invalid failpoint hit: {entry}"))?;
            if site.is_empty()
                || !site
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(format!("invalid failpoint site: {entry}"));
            }
            if faults
                .insert(
                    site.to_owned(),
                    Fault {
                        nth,
                        action,
                        hits: AtomicU32::new(0),
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate failpoint site: {site}"));
            }
        }
        Ok(faults)
    }

    #[cfg(feature = "failpoints")]
    pub(super) fn hit(site: &'static str) -> std::io::Result<()> {
        use std::io::Write as _;
        use std::sync::OnceLock;
        static FAULTS: OnceLock<Result<BTreeMap<String, Fault>, String>> = OnceLock::new();
        let faults = FAULTS
            .get_or_init(|| parse(&std::env::var("PINTAIL_FAILPOINT").unwrap_or_default()))
            .as_ref()
            .map_err(|error| std::io::Error::other(error.clone()))?;
        let Some(fault) = faults.get(site) else {
            return Ok(());
        };
        let Some(action) = fault.visit() else {
            return Ok(());
        };
        let label = if action == Action::Abort {
            "aborting"
        } else {
            "error"
        };
        // Write the witness before abort, even when stderr is a pipe. Do not
        // panic on a closed diagnostic pipe before reaching the boundary.
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "failpoint {site} hit {}: {label}", fault.nth);
        let _ = stderr.flush();
        match action {
            Action::Abort => std::process::abort(),
            Action::Error => Err(std::io::Error::other(format!("failpoint {site}"))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Action, parse};

        #[test]
        fn error_fires_only_on_the_named_nth_visit() {
            let faults = parse("wal@3=error,meta").unwrap();
            let fault = &faults["wal"];
            assert_eq!(fault.visit(), None);
            assert_eq!(fault.visit(), None);
            assert_eq!(fault.visit(), Some(Action::Error));
            assert_eq!(fault.visit(), None);
            assert_eq!(faults["meta"].visit(), Some(Action::Abort));
            assert!(!faults.contains_key("unknown"));
        }

        #[test]
        fn malformed_configuration_cannot_silently_disable_a_test() {
            for input in [
                "wal@0",
                "wal@bad",
                "wal=typo",
                "=error",
                "wal,wal@2",
                "wal@4294967296",
            ] {
                assert!(parse(input).is_err(), "{input}");
            }
            assert!(parse("").unwrap().is_empty());
        }

        #[test]
        fn concurrent_visitors_fire_exactly_once() {
            let faults = parse("wal@5=error").unwrap();
            let fault = &faults["wal"];
            let fired = std::thread::scope(|scope| {
                let handles = (0..16)
                    .map(|_| scope.spawn(|| fault.visit().is_some()))
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| usize::from(handle.join().unwrap()))
                    .sum::<usize>()
            });
            assert_eq!(fired, 1);
            assert_eq!(fault.visit(), None);
        }
    }
}
