//! Runs the cases natively: checks the answers the generator can compute,
//! prints a digest of each for comparing builds, and times them.
//!
//! `answers [repeats] [case]`: with a count, each case runs that many more
//! times, which is what profile-guided training and a sampling profiler
//! want; with a name, only that case runs. `PINTAIL_SUITE_PAUSE_MS` sleeps
//! before each run, so a profiler's timeline shows the runs apart.
use std::time::Instant;

use pintail_instruction_suite::{CASES, Fixture, Tables, digest, expected};

fn main() {
    // The server's pool: named workers with the main thread's stack size.
    pintail_exec::init_parallel_pool().expect("the first use of the pool");
    let mut arguments = std::env::args().skip(1);
    let repeats: usize = arguments
        .next()
        .map_or(1, |value| value.parse().expect("a repeat count"));
    let only = arguments.next();
    let pause = std::time::Duration::from_millis(
        std::env::var("PINTAIL_SUITE_PAUSE_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    );
    for tables in [Tables::Commerce, Tables::Events] {
        let cases: Vec<_> = CASES
            .iter()
            .filter(|case| case.tables == tables)
            .filter(|case| only.as_deref().is_none_or(|name| name == case.name))
            .collect();
        if cases.is_empty() {
            continue;
        }
        let fixture = Fixture::build(tables);
        for case in cases {
            let mut answer = Vec::new();
            let mut fastest = f64::MAX;
            for _ in 0..=repeats {
                std::thread::sleep(pause);
                let clock = Instant::now();
                answer = fixture.run(case);
                fastest = fastest.min(clock.elapsed().as_secs_f64() * 1e3);
            }
            if let Some(want) = expected(case) {
                assert_eq!(answer, want, "{} changed its answer", case.name);
            }
            assert!(!answer.is_empty(), "{} answered nothing", case.name);
            println!(
                "{}\t{}\t{:016x}\t{fastest:.2}",
                case.name,
                answer.len(),
                digest(&answer)
            );
        }
    }
}
