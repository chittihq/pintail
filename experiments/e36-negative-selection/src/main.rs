//! e36: anomaly detectors vetoed by exact CDC invariants.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Policy {
    Threshold,
    Distance,
    Negative,
}
struct Out {
    checksum: u64,
    recall: u64,
    false_ppm: u64,
    checks: u64,
}
fn main() {
    println!("e36 — negative-selection CDC sentinels (simulation tier)");
    let t = trace();
    for (n, p) in [
        ("fixed thresholds", Policy::Threshold),
        ("distance", Policy::Distance),
        ("negative ensemble", Policy::Negative),
    ] {
        let o = run(&t, p);
        println!(
            "{n:<20} recall {}% false {} ppm checks {}",
            o.recall, o.false_ppm, o.checks
        );
    }
    let rs = [
        bench("threshold", || run(&t, Policy::Threshold).checksum),
        bench("distance", || run(&t, Policy::Distance).checksum),
        bench("negative", || run(&t, Policy::Negative).checksum),
    ];
    check_consistency(&rs);
}
fn trace() -> Vec<([i64; 4], bool, bool, u64)> {
    let mut r = Lcg::new(0x3600_0036);
    (0..100_000)
        .map(|i| {
            let fault = i % 100 == 0;
            let adversarial = i % 997 == 0;
            let mut x = [
                r.below(20) as i64,
                r.below(20) as i64,
                r.below(20) as i64,
                r.below(20) as i64,
            ];
            if fault {
                x[i % 4] += 60
            }
            (x, fault, adversarial, r.next_u64())
        })
        .collect()
}
fn run(t: &[([i64; 4], bool, bool, u64)], p: Policy) -> Out {
    let mut tp = 0;
    let mut fp = 0;
    let mut faults = 0;
    let mut healthy = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for &(x, fault, adversarial, token) in t {
        faults += u64::from(fault);
        healthy += u64::from(!fault);
        let anomaly = match p {
            Policy::Threshold => x.iter().any(|v| *v > 55),
            Policy::Distance => x.iter().map(|v| (v - 10) * (v - 10)).sum::<i64>() > 1100,
            Policy::Negative => x.iter().filter(|v| **v > 45).count() >= 1,
        };
        let invariant = fault && !adversarial;
        let quarantine = anomaly && invariant;
        tp += u64::from(quarantine && fault);
        fp += u64::from(quarantine && !fault);
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        recall: tp * 100 / faults,
        false_ppm: fp * 1_000_000 / healthy,
        checks: t.len() as u64,
    }
}
